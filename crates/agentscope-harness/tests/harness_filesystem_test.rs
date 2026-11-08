//! Integration tests for the harness filesystem backends.
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::{ContentBlock, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};
use agentscope_harness::tool::FilesystemTool;
use agentscope_harness::{
    CompositeFilesystem, ExecuteResponse, Filesystem, LocalFilesystem, OverlayFilesystem,
    SandboxBackedFilesystem, SandboxFilesystem,
};

fn ctx() -> RuntimeContext {
    RuntimeContext::default()
}

fn tool_ctx() -> ToolExecutionContext {
    ToolExecutionContext {
        tool_call: None,
        runtime: serde_json::Map::new(),
        emitter: None,
        tool_request_config: None,
    }
}

async fn temp_dir(tag: &str) -> std::path::PathBuf {
    let base =
        std::env::temp_dir().join(format!("agentscope_harness_{tag}_{}", std::process::id()));
    let _ = tokio::fs::remove_dir_all(&base).await;
    tokio::fs::create_dir_all(&base).await.unwrap();
    base
}
#[tokio::test]
async fn local_write_read_edit_delete() {
    let dir = temp_dir("local").await;
    let fs = LocalFilesystem::sandboxed(&dir);

    fs.write(&ctx(), "/a.txt", "hello\nworld").await.unwrap();
    assert!(fs.exists(&ctx(), "/a.txt").await.unwrap());

    let data = fs.read(&ctx(), "/a.txt", 0, 0).await.unwrap();
    assert_eq!(data.content, "hello\nworld");

    let n = fs
        .edit(&ctx(), "/a.txt", "world", "rust", false)
        .await
        .unwrap();
    assert_eq!(n, 1);
    let data = fs.read(&ctx(), "/a.txt", 0, 0).await.unwrap();
    assert_eq!(data.content, "hello\nrust");

    // write to existing path fails
    assert!(fs.write(&ctx(), "/a.txt", "x").await.is_err());

    fs.delete(&ctx(), "/a.txt").await.unwrap();
    assert!(!fs.exists(&ctx(), "/a.txt").await.unwrap());
}

#[tokio::test]
async fn read_clamps_offset_past_eof_instead_of_erroring() {
    let dir = temp_dir("read_past_eof").await;
    let fs = LocalFilesystem::sandboxed(&dir);
    fs.write(&ctx(), "/a.txt", "l1\nl2\nl3").await.unwrap();

    // offset at or past the end -> empty slice, no error
    assert_eq!(fs.read(&ctx(), "/a.txt", 3, 10).await.unwrap().content, "");
    assert_eq!(
        fs.read(&ctx(), "/a.txt", 395, 100).await.unwrap().content,
        ""
    );
    // a huge limit must not overflow the slice bound
    assert_eq!(
        fs.read(&ctx(), "/a.txt", 1, usize::MAX)
            .await
            .unwrap()
            .content,
        "l2\nl3"
    );
}

#[tokio::test]
async fn read_file_tool_accepts_single_path_and_clamps_paging() {
    use serde_json::json;

    let dir = temp_dir("read_tool_params").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    fs.write(&ctx(), "/a.txt", "l1\nl2\nl3").await.unwrap();
    let tool = FilesystemTool::read_file(Arc::clone(&fs));

    // `filePath` as a bare string + a negative offset is tolerated.
    let out = tool
        .call(
            json!({"filePath": "/a.txt", "offset": -1, "limit": 2})
                .as_object()
                .unwrap()
                .clone(),
            tool_ctx(),
        )
        .await
        .unwrap();
    assert_eq!(tool_text(&out), "### /a.txt\nl1\nl2");

    // offset past the end reads to the end of the file instead of erroring.
    let out = tool
        .call(
            json!({"filePath": ["/a.txt"], "offset": 395})
                .as_object()
                .unwrap()
                .clone(),
            tool_ctx(),
        )
        .await
        .unwrap();
    assert_eq!(tool_text(&out), "### /a.txt\n");
}

#[tokio::test]
async fn local_sandboxed_blocks_traversal() {
    let dir = temp_dir("sandbox").await;
    let fs = LocalFilesystem::sandboxed(&dir);
    // /.. would escape the root
    assert!(fs.write(&ctx(), "/../escape.txt", "x").await.is_err());
}

#[tokio::test]
async fn local_glob_and_grep() {
    let dir = temp_dir("glob").await;
    let fs = LocalFilesystem::sandboxed(&dir);
    fs.write(&ctx(), "/src/main.rs", "fn main() {}")
        .await
        .unwrap();
    fs.write(&ctx(), "/src/lib.rs", "pub fn lib()")
        .await
        .unwrap();
    fs.write(&ctx(), "/README.md", "docs").await.unwrap();

    let files = fs.glob(&ctx(), "**/*.rs", "/").await.unwrap();
    assert_eq!(files.len(), 2);

    let matches = fs
        .grep(&ctx(), "fn", Some("/"), Some("**/*.rs"))
        .await
        .unwrap();
    assert_eq!(matches.len(), 2);
}

#[tokio::test]
async fn grep_single_file_and_regex() {
    let dir = temp_dir("grep_single").await;
    let fs = LocalFilesystem::sandboxed(&dir);
    fs.write(
        &ctx(),
        "/src/main.rs",
        "fn main() {\n    println!(\"hi\");\n}",
    )
    .await
    .unwrap();
    fs.write(&ctx(), "/README.md", "fn docs only")
        .await
        .unwrap();

    // grep on a single file path works (Linux `grep pattern file`)
    let matches = fs
        .grep(&ctx(), "println", Some("/src/main.rs"), None)
        .await
        .unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].line, 2);

    // pattern is a regex: `fn\s+main` matches "fn main("
    let matches = fs
        .grep(&ctx(), r"fn\s+main", Some("/src"), None)
        .await
        .unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].path, "/src/main.rs");

    // invalid regex falls back to literal substring matching
    let matches = fs.grep(&ctx(), "fn (", Some("/src"), None).await.unwrap();
    assert_eq!(matches.len(), 0);

    // inline (?i) flag is honored
    let matches = fs
        .grep(&ctx(), "(?i)PRINTLN", Some("/src/main.rs"), None)
        .await
        .unwrap();
    assert_eq!(matches.len(), 1);
}

#[tokio::test]
async fn grep_glob_filter_matches_basename() {
    let dir = temp_dir("grep_glob").await;
    let fs = LocalFilesystem::sandboxed(&dir);
    fs.write(&ctx(), "/src/main.rs", "fn main() {}")
        .await
        .unwrap();
    fs.write(&ctx(), "/src/lib.rs", "pub fn lib()")
        .await
        .unwrap();
    fs.write(&ctx(), "/README.md", "fn docs").await.unwrap();

    // glob filter is matched against the file name (like grep --include=*.rs)
    let matches = fs
        .grep(&ctx(), "fn", Some("/"), Some("*.rs"))
        .await
        .unwrap();
    assert_eq!(matches.len(), 2);
    assert!(matches.iter().all(|m| m.path.ends_with(".rs")));
}

#[tokio::test]
async fn glob_pattern_is_relative_to_base() {
    let dir = temp_dir("glob_base").await;
    let fs = LocalFilesystem::sandboxed(&dir);
    fs.write(&ctx(), "/src/main.rs", "fn main() {}")
        .await
        .unwrap();
    fs.write(&ctx(), "/src/lib.rs", "pub fn lib()")
        .await
        .unwrap();
    fs.write(&ctx(), "/src/nested/extra.rs", "fn extra()")
        .await
        .unwrap();
    fs.write(&ctx(), "/README.md", "docs").await.unwrap();

    // `*.rs` matches files directly under /src (like `cd /src && ls *.rs`)
    let files = fs.glob(&ctx(), "*.rs", "/src").await.unwrap();
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|f| !f.path.contains("/nested/")));

    // `**/*.rs` recurses from /src and includes nested files
    let files = fs.glob(&ctx(), "**/*.rs", "/src").await.unwrap();
    assert_eq!(files.len(), 3);

    // patterns may include a leading directory component relative to the base
    let files = fs.glob(&ctx(), "nested/*.rs", "/src").await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "/src/nested/extra.rs");
}

#[tokio::test]
async fn grep_and_glob_skip_generated_dirs() {
    let dir = temp_dir("skip_generated").await;
    let fs = LocalFilesystem::sandboxed(&dir);
    fs.write(&ctx(), "/src/main.rs", "fn main() {}")
        .await
        .unwrap();
    fs.write(&ctx(), "/target/gen.rs", "fn main() in target")
        .await
        .unwrap();
    fs.write(
        &ctx(),
        "/node_modules/pkg/index.rs",
        "fn main() in node_modules",
    )
    .await
    .unwrap();

    // glob does not descend into generated dirs
    let files = fs.glob(&ctx(), "**/*.rs", "/").await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "/src/main.rs");

    // grep does not search generated dirs
    let matches = fs.grep(&ctx(), "fn main", Some("/"), None).await.unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].path, "/src/main.rs");

    // searching a generated dir directly still works (base itself is walked)
    let files = fs.glob(&ctx(), "**/*.rs", "/target").await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "/target/gen.rs");
}

#[tokio::test]
async fn overlay_copy_on_write() {
    let upper_dir = temp_dir("overlay_upper").await;
    let lower_dir = temp_dir("overlay_lower").await;
    let upper = Arc::new(LocalFilesystem::sandboxed(&upper_dir));
    let lower = Arc::new(LocalFilesystem::sandboxed(&lower_dir));

    // shared file in the lower layer
    lower
        .write(&ctx(), "/shared.md", "shared original")
        .await
        .unwrap();
    let overlay = OverlayFilesystem::new(upper.clone(), lower.clone());

    // read falls through to lower
    let data = overlay.read(&ctx(), "/shared.md", 0, 0).await.unwrap();
    assert_eq!(data.content, "shared original");

    // editing a shared file triggers copy-on-write into upper; lower stays intact
    let n = overlay
        .edit(&ctx(), "/shared.md", "original", "edited", false)
        .await
        .unwrap();
    assert_eq!(n, 1);
    let upper_data = upper.read(&ctx(), "/shared.md", 0, 0).await.unwrap();
    assert_eq!(upper_data.content, "shared edited");
    let lower_data = lower.read(&ctx(), "/shared.md", 0, 0).await.unwrap();
    assert_eq!(lower_data.content, "shared original");

    // writing a fresh file goes to upper only
    overlay.write(&ctx(), "/new.txt", "new").await.unwrap();
    assert!(upper.exists(&ctx(), "/new.txt").await.unwrap());
}

#[tokio::test]
async fn composite_routes_by_prefix() {
    let default_dir = temp_dir("composite_default").await;
    let store_dir = temp_dir("composite_store").await;
    let default = LocalFilesystem::sandboxed(&default_dir);
    let store = LocalFilesystem::sandboxed(&store_dir);
    let composite = CompositeFilesystem::new(
        Arc::new(default),
        vec![("/memories/".to_string(), Arc::new(store))],
    );

    // routed to the store backend
    composite
        .write(&ctx(), "/memories/notes.md", "note")
        .await
        .unwrap();
    let data = composite
        .read(&ctx(), "/memories/notes.md", 0, 0)
        .await
        .unwrap();
    assert_eq!(data.content, "note");

    // default backend handles unmatched paths
    composite
        .write(&ctx(), "/main.rs", "fn main() {}")
        .await
        .unwrap();
    let data = composite.read(&ctx(), "/main.rs", 0, 0).await.unwrap();
    assert_eq!(data.content, "fn main() {}");
}

#[tokio::test]
async fn sandbox_executes_shell() {
    let dir = temp_dir("sandbox_exec").await;
    let inner = LocalFilesystem::sandboxed(&dir);
    let sandbox = SandboxBackedFilesystem::new(Arc::new(inner), dir);
    let result = sandbox
        .execute(&ctx(), "echo hello-from-shell", 10)
        .await
        .unwrap();
    assert!(result.is_success());
    assert!(result.output.contains("hello-from-shell"));
}

#[tokio::test]
async fn a_command_that_exceeds_its_timeout_is_killed_and_returns() {
    // A timed-out shell command used to be able to hang the caller: the timeout path kills the
    // process tree synchronously, and a stuck kill left the run (and the session's `active_run`
    // slot) occupied forever, so every later prompt queued until the host's admission deadline.
    let dir = temp_dir("sandbox_timeout").await;
    let inner = LocalFilesystem::sandboxed(&dir);
    let sandbox = SandboxBackedFilesystem::new(Arc::new(inner), dir);
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        sandbox.execute(&ctx(), long_running_command(), 1),
    )
    .await
    .expect("a timed-out command must not hang the caller")
    .unwrap();
    assert_eq!(result.exit_code, ExecuteResponse::TIMEOUT_EXIT_CODE);
    assert!(
        result.timed_out(),
        "a timeout must be identifiable: {result:?}"
    );
    assert!(result.output.contains("timed out"), "{}", result.output);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "the timeout must return in time, took {:?}",
        started.elapsed()
    );
}

#[cfg(windows)]
fn long_running_command() -> &'static str {
    "Start-Sleep -Seconds 30"
}

#[cfg(not(windows))]
fn long_running_command() -> &'static str {
    "sleep 30"
}

#[tokio::test]
async fn ls_reports_errors_instead_of_silent_empty() {
    let dir = temp_dir("ls_errors").await;
    let fs = LocalFilesystem::sandboxed(&dir);

    // non-existent path -> error (not silent empty)
    let err = fs.ls(&ctx(), "/nope").await.unwrap_err().to_string();
    assert!(err.contains("Path does not exist"), "got: {err}");

    // a file (not a directory) -> error
    fs.write(&ctx(), "/file.txt", "x").await.unwrap();
    let err = fs.ls(&ctx(), "/file.txt").await.unwrap_err().to_string();
    assert!(err.contains("Not a directory"), "got: {err}");
}

#[tokio::test]
async fn search_tools_bound_output_by_limit() {
    use serde_json::json;

    let dir = temp_dir("search_limit").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    for i in 0..50 {
        fs.write(&ctx(), &format!("/f{i}.rs"), "pub fn f()")
            .await
            .unwrap();
    }

    let glob = FilesystemTool::glob_files(Arc::clone(&fs));
    let out = glob
        .call(
            json!({ "pattern": "**/*.rs", "path": "/", "limit": 10 })
                .as_object()
                .unwrap()
                .clone(),
            tool_ctx(),
        )
        .await
        .unwrap();
    let text = tool_text(&out);
    assert!(
        text.contains("Truncated"),
        "expected truncation notice, got: {text}"
    );
    assert_eq!(text.lines().count(), 12, "10 entries + 2 notice lines");

    // limit < 1 is rejected as an Err
    let err = glob
        .call(
            json!({ "pattern": "**/*.rs", "path": "/", "limit": 0 })
                .as_object()
                .unwrap()
                .clone(),
            tool_ctx(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("must be greater than 0"));
}

fn tool_text(block: &ToolResultBlock) -> String {
    block
        .output
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn grep_glob_with_slash_matches_relative_path() {
    let dir = temp_dir("grep_glob_path").await;
    let fs = LocalFilesystem::sandboxed(&dir);
    fs.write(&ctx(), "/src/main.rs", "fn main() {}")
        .await
        .unwrap();
    fs.write(&ctx(), "/src/nested/extra.rs", "fn extra() {}")
        .await
        .unwrap();
    fs.write(&ctx(), "/other/keep.rs", "fn keep() {}")
        .await
        .unwrap();

    // No slash: matched against the file name, so it stays recursive.
    let matches = fs
        .grep(&ctx(), "fn", Some("/"), Some("*.rs"))
        .await
        .unwrap();
    assert_eq!(matches.len(), 3);

    // A slash makes the pattern relative to the search base.
    let matches = fs
        .grep(&ctx(), "fn", Some("/"), Some("src/*.rs"))
        .await
        .unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].path, "/src/main.rs");

    let matches = fs
        .grep(&ctx(), "fn", Some("/"), Some("src/**/*.rs"))
        .await
        .unwrap();
    assert_eq!(matches.len(), 2);

    let matches = fs
        .grep(&ctx(), "fn", Some("/src"), Some("nested/*.rs"))
        .await
        .unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].path, "/src/nested/extra.rs");
}

#[tokio::test]
async fn grep_and_glob_respect_gitignore() {
    let dir = temp_dir("gitignore").await;
    let fs = LocalFilesystem::sandboxed(&dir);
    fs.write(&ctx(), "/.gitignore", "ignored/\n*.log\n")
        .await
        .unwrap();
    fs.write(&ctx(), "/keep.rs", "fn keep() {}").await.unwrap();
    fs.write(&ctx(), "/ignored/skip.rs", "fn skip() {}")
        .await
        .unwrap();
    fs.write(&ctx(), "/debug.log", "fn logged").await.unwrap();

    let matches = fs.grep(&ctx(), "fn", Some("/"), None).await.unwrap();
    assert_eq!(
        matches.len(),
        1,
        "gitignored files must be skipped: {matches:?}"
    );
    assert_eq!(matches[0].path, "/keep.rs");

    let files = fs.glob(&ctx(), "**/*.rs", "/").await.unwrap();
    assert_eq!(files.len(), 1, "expected only keep.rs: {files:?}");
    assert_eq!(files[0].path, "/keep.rs");
}

#[tokio::test]
async fn grep_skips_binary_and_keeps_non_utf8_text() {
    let dir = temp_dir("grep_binary").await;
    let fs = LocalFilesystem::sandboxed(&dir);
    // NUL byte -> treated as binary and skipped.
    tokio::fs::write(dir.join("blob.dat"), [0u8, 1, 2, b'f', b'n'])
        .await
        .unwrap();
    // Latin-1 bytes: not valid UTF-8, but still text -> searched lossily.
    tokio::fs::write(
        dir.join("latin1.txt"),
        [b'c', b'a', b'f', 0xe9, b' ', b'f', b'n'],
    )
    .await
    .unwrap();

    let matches = fs.grep(&ctx(), "fn", Some("/"), None).await.unwrap();
    assert_eq!(
        matches.len(),
        1,
        "binary skipped, non-UTF-8 text kept: {matches:?}"
    );
    assert_eq!(matches[0].path, "/latin1.txt");
}

#[tokio::test]
async fn grep_tool_flags_and_output_modes() {
    use serde_json::json;

    let dir = temp_dir("grep_flags").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    fs.write(&ctx(), "/a.rs", "fn a()").await.unwrap();
    fs.write(&ctx(), "/b.rs", "FN b()").await.unwrap();
    fs.write(&ctx(), "/c.md", "fn c()").await.unwrap();
    let grep = FilesystemTool::grep_files(Arc::clone(&fs));

    let call = |args: serde_json::Value| {
        let grep = &grep;
        async move {
            grep.call(args.as_object().unwrap().clone(), tool_ctx())
                .await
        }
    };

    // ignore_case (-i): all three files match.
    let out = call(json!({"pattern": "fn", "path": "/", "ignore_case": true}))
        .await
        .unwrap();
    assert_eq!(tool_text(&out).lines().count(), 3);

    // type filter (-t rust): only the lowercase hit in a.rs remains.
    let out = call(json!({"pattern": "fn", "path": "/", "type": "rust"}))
        .await
        .unwrap();
    let text = tool_text(&out);
    assert_eq!(text.lines().count(), 1, "got: {text}");
    assert!(text.contains("a.rs"), "got: {text}");

    // -c: one line per file with a hit count.
    let out = call(json!({"pattern": "fn", "path": "/", "output_mode": "count"}))
        .await
        .unwrap();
    let text = tool_text(&out);
    assert_eq!(text.lines().count(), 2, "got: {text}");
    assert!(text.contains("a.rs:1"), "got: {text}");

    // -l: unique paths only.
    let out = call(json!({"pattern": "fn", "path": "/", "output_mode": "files_with_matches"}))
        .await
        .unwrap();
    let text = tool_text(&out);
    assert_eq!(text.lines().count(), 2, "got: {text}");
    assert!(!text.contains(":1:"), "paths only, got: {text}");

    // Explicit file list instead of a single path.
    let out = call(json!({"pattern": "fn", "paths": ["/a.rs", "/c.md"]}))
        .await
        .unwrap();
    assert_eq!(tool_text(&out).lines().count(), 2);

    // glob and type are mutually exclusive.
    let err = call(json!({"pattern": "fn", "glob": "*.rs", "type": "rust"}))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("either glob or type"),
        "got: {err}"
    );

    // Unknown type names are reported with the known set.
    let err = call(json!({"pattern": "fn", "type": "cobol"}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Unknown type"), "got: {err}");
}

#[tokio::test]
async fn grep_content_output_is_compacted_into_path_headings() {
    use serde_json::json;

    let dir = temp_dir("grep_compact").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let storage = "/crates/agentscope-harness/src/agent/session/storage.rs";
    let sqlite = "/crates/agentscope-harness/src/agent/session/sqlite.rs";
    fs.write(
        &ctx(),
        storage,
        "fn alpha() {}\nfn beta() {}\nfn gamma() {}",
    )
    .await
    .unwrap();
    fs.write(&ctx(), sqlite, "fn delta() {}\nfn epsilon() {}")
        .await
        .unwrap();

    let tool = FilesystemTool::grep_files(Arc::clone(&fs));
    let out = tool
        .call(
            json!({"pattern": "fn", "path": "/"})
                .as_object()
                .unwrap()
                .clone(),
            tool_ctx(),
        )
        .await
        .unwrap();
    let text = tool_text(&out);
    assert!(
        text.starts_with("[paths grouped; N:<text> = line N of the path above]\n"),
        "got: {text}"
    );
    // The path is a heading seen once; its matches keep only `line:text`.
    assert_eq!(text.matches(storage).count(), 1, "got: {text}");
    assert!(
        text.contains(&format!(
            "{storage}\n1:fn alpha() {{}}\n2:fn beta() {{}}\n3:fn gamma() {{}}"
        )),
        "got: {text}"
    );
    assert!(
        text.contains(&format!("{sqlite}\n1:fn delta() {{}}\n2:fn epsilon() {{}}")),
        "got: {text}"
    );
    assert_eq!(
        out.metadata.get("agentscope.output_compact.hoistedFiles"),
        Some(&json!(2))
    );
    assert_eq!(
        out.metadata.get("agentscope.output_compact.hoistedLines"),
        Some(&json!(5))
    );
    assert!(out
        .metadata
        .get("agentscope.output_compact.savedChars")
        .is_some());
}

#[tokio::test]
async fn grep_output_compaction_can_be_disabled() {
    use agentscope_harness::CompactConfig;
    use serde_json::json;

    let dir = temp_dir("grep_compact_off").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let storage = "/crates/agentscope-harness/src/agent/session/storage.rs";
    fs.write(
        &ctx(),
        storage,
        "fn alpha() {}\nfn beta() {}\nfn gamma() {}",
    )
    .await
    .unwrap();
    let args = json!({"pattern": "fn", "path": "/"})
        .as_object()
        .unwrap()
        .clone();

    let tool =
        FilesystemTool::grep_files_with_compaction(Arc::clone(&fs), CompactConfig::disabled());
    let out = tool.call(args.clone(), tool_ctx()).await.unwrap();
    let text = tool_text(&out);
    assert!(
        text.starts_with(&format!("{storage}:1:fn alpha() {{}}")),
        "raw `path:line:text` expected, got: {text}"
    );
    assert!(out.metadata.is_empty(), "got: {:?}", out.metadata);

    // `count` mode prints `path:count`, which has no repeated prefix to fold away.
    let tool = FilesystemTool::grep_files(Arc::clone(&fs));
    let out = tool
        .call(
            json!({"pattern": "fn", "path": "/", "output_mode": "count"})
                .as_object()
                .unwrap()
                .clone(),
            tool_ctx(),
        )
        .await
        .unwrap();
    let text = tool_text(&out);
    assert_eq!(text.lines().count(), 1, "got: {text}");
    assert!(text.contains(&format!("{storage}:3")), "got: {text}");
    assert!(out.metadata.is_empty(), "got: {:?}", out.metadata);
}

#[tokio::test]
async fn grep_ignore_case_survives_literal_fallback() {
    let dir = temp_dir("grep_literal_ci").await;
    let fs = LocalFilesystem::sandboxed(&dir);
    // `fn (` is not a valid regex, so the matcher falls back to literal substring.
    fs.write(&ctx(), "/a.txt", "see FN ( here").await.unwrap();
    fs.write(&ctx(), "/b.txt", "nothing").await.unwrap();

    let matches = fs.grep(&ctx(), "fn (", Some("/"), None).await.unwrap();
    assert!(matches.is_empty(), "case-sensitive literal: {matches:?}");

    let matches = fs.grep(&ctx(), "(?i)fn (", Some("/"), None).await.unwrap();
    assert_eq!(
        matches.len(),
        1,
        "(?i) must still apply on the literal fallback path: {matches:?}"
    );
    assert_eq!(matches[0].path, "/a.txt");
}

#[tokio::test]
async fn generated_dir_names_do_not_prune_regular_files() {
    let dir = temp_dir("generated_name_file").await;
    let fs = LocalFilesystem::sandboxed(&dir);
    // `build` is a generated *directory* name; a regular file of that name must
    // still be searched.
    fs.write(&ctx(), "/build", "fn in_plain_file")
        .await
        .unwrap();
    fs.write(&ctx(), "/node_modules/skip.rs", "fn in_build_dir")
        .await
        .unwrap();

    let matches = fs.grep(&ctx(), "fn", Some("/"), None).await.unwrap();
    assert_eq!(
        matches.len(),
        1,
        "only the plain file should match: {matches:?}"
    );
    assert_eq!(matches[0].path, "/build");
}

/// GAP-12: `read` must sniff only the first 8 KiB for NUL, so a large text file
/// that happens to contain a NUL byte past that point is still returned as text.
#[tokio::test]
async fn read_treats_nul_after_sniff_window_as_text() {
    let dir = temp_dir("read_nul_late").await;
    let fs = LocalFilesystem::sandboxed(&dir);
    let mut bytes = b"first line\n".repeat(1000); // 11 000 bytes: past the 8 KiB window
    bytes.push(0);
    bytes.extend_from_slice(b"\ntail line\n");
    tokio::fs::write(dir.join("late_nul.txt"), &bytes)
        .await
        .unwrap();

    let data = fs.read(&ctx(), "/late_nul.txt", 0, 0).await.unwrap();
    assert_eq!(
        data.encoding, "utf-8",
        "a NUL after the 8 KiB sniff window must not mark the file binary"
    );
    assert!(
        data.content.starts_with("first line\n"),
        "content should be raw text, not base64"
    );
}

/// GAP-12: a NUL inside the first 8 KiB still means binary (base64 result).
#[tokio::test]
async fn read_treats_nul_within_sniff_window_as_binary() {
    let dir = temp_dir("read_nul_early").await;
    let fs = LocalFilesystem::sandboxed(&dir);
    tokio::fs::write(dir.join("blob.dat"), [b'a', b'b', 0u8, b'c', b'd'])
        .await
        .unwrap();

    let data = fs.read(&ctx(), "/blob.dat", 0, 0).await.unwrap();
    assert_eq!(data.encoding, "base64");
    assert_eq!(data.content, "YWIAY2Q="); // base64("ab\0cd")
}

/// GAP-12: a large pure-text file stays text.
#[tokio::test]
async fn read_large_text_file_is_text() {
    let dir = temp_dir("read_large_text").await;
    let fs = LocalFilesystem::sandboxed(&dir);
    let body = "x".repeat(600) + "\n";
    let content = body.repeat(60); // ~36 KiB, well beyond the sniff window
    fs.write(&ctx(), "/big.txt", &content).await.unwrap();

    let data = fs.read(&ctx(), "/big.txt", 0, 0).await.unwrap();
    assert_eq!(data.encoding, "utf-8");
    assert_eq!(data.content, content);
}
