//! Integration tests for the harness filesystem backends.
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_harness::{
    CompositeFilesystem, Filesystem, LocalFilesystem, OverlayFilesystem, SandboxBackedFilesystem,
    SandboxFilesystem,
};

fn ctx() -> RuntimeContext {
    RuntimeContext::default()
}

async fn temp_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "agentscope_harness_{tag}_{}",
        std::process::id()
    ));
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

    let n = fs.edit(&ctx(), "/a.txt", "world", "rust", false).await.unwrap();
    assert_eq!(n, 1);
    let data = fs.read(&ctx(), "/a.txt", 0, 0).await.unwrap();
    assert_eq!(data.content, "hello\nrust");

    // write to existing path fails
    assert!(fs.write(&ctx(), "/a.txt", "x").await.is_err());

    fs.delete(&ctx(), "/a.txt").await.unwrap();
    assert!(!fs.exists(&ctx(), "/a.txt").await.unwrap());
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
    fs.write(&ctx(), "/src/main.rs", "fn main() {}").await.unwrap();
    fs.write(&ctx(), "/src/lib.rs", "pub fn lib()").await.unwrap();
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
async fn overlay_copy_on_write() {
    let upper_dir = temp_dir("overlay_upper").await;
    let lower_dir = temp_dir("overlay_lower").await;
    let upper = Arc::new(LocalFilesystem::sandboxed(&upper_dir));
    let lower = Arc::new(LocalFilesystem::sandboxed(&lower_dir));

    // shared file in the lower layer
    lower.write(&ctx(), "/shared.md", "shared original").await.unwrap();
    let overlay = OverlayFilesystem::new(upper.clone(), lower.clone());

    // read falls through to lower
    let data = overlay.read(&ctx(), "/shared.md", 0, 0).await.unwrap();
    assert_eq!(data.content, "shared original");

    // editing a shared file triggers copy-on-write into upper; lower stays intact
    let n = overlay.edit(&ctx(), "/shared.md", "original", "edited", false).await.unwrap();
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
    let data = composite.read(&ctx(), "/memories/notes.md", 0, 0).await.unwrap();
    assert_eq!(data.content, "note");

    // default backend handles unmatched paths
    composite.write(&ctx(), "/main.rs", "fn main() {}").await.unwrap();
    let data = composite.read(&ctx(), "/main.rs", 0, 0).await.unwrap();
    assert_eq!(data.content, "fn main() {}");
}

#[tokio::test]
async fn sandbox_executes_shell() {
    let dir = temp_dir("sandbox_exec").await;
    let inner = LocalFilesystem::sandboxed(&dir);
    let sandbox = SandboxBackedFilesystem::new(Arc::new(inner), dir);
    let result = sandbox.execute(&ctx(), "echo hello-from-shell", 10).await.unwrap();
    assert!(result.is_success());
    assert!(result.output.contains("hello-from-shell"));
}
