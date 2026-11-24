//! Workspace task runner.
//!
//! ```text
//! cargo xtask acp build <variant> [--profile release|embedded]
//! cargo xtask acp upx   <variant> [--profile release|embedded]
//! cargo xtask acp registry <artifacts-dir> [--out <file>]
//! cargo xtask publish   [--dry-run] [--sleep <secs>]
//! ```
//!
//! A *variant* is a feature set of `agentscope-extensions-acp`:
//!
//! | variant   | feature suffix   | runtime                      |
//! |-----------|------------------|------------------------------|
//! | `native`  | (none)           | native driver only           |
//! | `durable` | `durable`        | durable driver, file store   |
//! | `sqlite`  | `durable-sqlite` | durable driver, SQLite store |
//!
//! `durable-sqlite` already implies `durable`, so both drivers are compiled in
//! for the `sqlite` variant; `AGENTSCOPE_TURN_DRIVER` picks the driver at run
//! time. `all` builds every variant.
//!
//! The *base* feature is `rusqlite` for the release profile and `lite` for the
//! embedded one, mirroring the hand-written scripts this replaces. Both
//! binaries of a variant land in `target/acp/<profile>/<variant>/`; they must
//! keep their names side by side because `acp_agent_cli` spawns
//! `acp_harness_agent` from its own directory.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

const USAGE: &str = "\
xtask -- workspace task runner

usage:
  cargo xtask acp build <variant> [--profile release|embedded]
  cargo xtask acp upx   <variant> [--profile release|embedded]
  cargo xtask acp registry <artifacts-dir> [--out <file>]
  cargo xtask publish   [--dry-run] [--sleep <secs>]

registry:
  Turns the `SHA256SUMS.txt` of a release (the assets downloaded from the
  GitHub release, or the `artifacts/` dir of the workflow) into the ACP
  Registry `agent.json`, with `version` taken from `[workspace.package]`.
  Default output is `.acp-registry/agentscope-rust/agent.json`.

variants:
  native    native driver only (no durable runtime compiled in)
  durable   durable driver, file store (same features as `default`)
  sqlite    durable driver, SQLite store
  all       native + durable + sqlite

profiles:
  release   default profile, stable toolchain
  embedded  size-optimised profile, requires nightly + rust-src (build-std)

artifacts:
  target/acp/<profile>/<variant>/acp_harness_agent[.exe]
  target/acp/<profile>/<variant>/acp_agent_cli[.exe]

environment:
  UPX_EXE   path to upx[.exe] for `acp upx` (defaults to `upx` on PATH)

publish:
  Uploads every publishable crate in dependency order, stopping at the first
  failure. `--sleep <secs>` (default 30) waits for the crates.io index to show
  the crate just uploaded before the next one; `--dry-run` packages and verifies
  without uploading (verification still resolves dependencies from the index, so
  it only succeeds once the dependencies are published).
";

const PACKAGE: &str = "agentscope-extensions-acp";
const BINS: [&str; 2] = ["acp_harness_agent", "acp_agent_cli"];

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("[xtask] {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut profile = Profile::Release;
    let mut dry_run = false;
    let mut out: Option<PathBuf> = None;
    let mut sleep = Duration::from_secs(30);
    let mut words: Vec<String> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--profile" => {
                let value = args.next().ok_or("--profile needs a value")?;
                profile = Profile::parse(&value)?;
            }
            "--dry-run" => dry_run = true,
            "--out" => {
                let value = args.next().ok_or("--out needs a value")?;
                out = Some(PathBuf::from(value));
            }
            "--sleep" => {
                let value = args.next().ok_or("--sleep needs a value")?;
                let secs = value
                    .parse::<u64>()
                    .map_err(|_| format!("`{value}` is not a number of seconds"))?;
                sleep = Duration::from_secs(secs);
            }
            "--help" | "-h" => {
                print!("{USAGE}");
                return Ok(());
            }
            _ if arg.starts_with('-') => {
                return Err(format!("unknown flag `{arg}`\n\n{USAGE}"));
            }
            _ => words.push(arg),
        }
    }

    let task = words.iter().map(String::as_str).collect::<Vec<_>>();
    match task.as_slice() {
        [] => {
            print!("{USAGE}");
            Ok(())
        }
        ["acp", "build", variant] => variants(variant)?
            .into_iter()
            .try_for_each(|variant| build(profile, variant)),
        ["acp", "upx", variant] => variants(variant)?
            .into_iter()
            .try_for_each(|variant| upx(profile, variant)),
        ["acp", "registry", artifacts] => registry(artifacts, out),
        ["publish"] => publish(dry_run, sleep),
        _ => Err(format!("unknown task `{}`\n\n{USAGE}", task.join(" "))),
    }
}

/// Release keeps the bundled SQLite index; embedded drops it, exactly like the
/// hand-written scripts this replaces.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Profile {
    Release,
    Embedded,
}

impl Profile {
    fn name(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Embedded => "embedded",
        }
    }

    fn base_features(self) -> &'static str {
        match self {
            Self::Release => "rusqlite",
            Self::Embedded => "lite",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "release" => Ok(Self::Release),
            "embedded" => Ok(Self::Embedded),
            _ => Err(format!(
                "unknown profile `{value}`, expected `release` or `embedded`"
            )),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Variant {
    Native,
    Durable,
    Sqlite,
}

impl Variant {
    fn name(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Durable => "durable",
            Self::Sqlite => "sqlite",
        }
    }

    /// Suffix appended to the profile's base feature. Empty for `native`.
    fn feature_suffix(self) -> &'static str {
        match self {
            Self::Native => "",
            Self::Durable => ",durable",
            Self::Sqlite => ",durable-sqlite",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "native" => Ok(Self::Native),
            "durable" => Ok(Self::Durable),
            "sqlite" => Ok(Self::Sqlite),
            _ => Err(format!(
                "unknown variant `{value}`, expected `native`, `durable`, `sqlite` or `all`"
            )),
        }
    }
}

fn variants(name: &str) -> Result<Vec<Variant>, String> {
    if name == "all" {
        Ok(vec![Variant::Native, Variant::Durable, Variant::Sqlite])
    } else {
        Ok(vec![Variant::parse(name)?])
    }
}

fn build(profile: Profile, variant: Variant) -> Result<(), String> {
    let root = workspace_root();
    let features = format!("{}{}", profile.base_features(), variant.feature_suffix());
    let out_dir = artifact_dir(&root, profile, variant);

    let mut command = Command::new("cargo");
    command.current_dir(&root);
    if profile == Profile::Embedded {
        command.args(["+nightly", "--config", ".cargo/config.embedded.toml"]);
    }
    command.args([
        "build",
        "--profile",
        profile.name(),
        "-p",
        PACKAGE,
        "--no-default-features",
        "--features",
        &features,
        "--bins",
    ]);

    eprintln!(
        "[xtask] acp build {}/{} (features: {features})",
        profile.name(),
        variant.name()
    );
    let status = command
        .status()
        .map_err(|error| format!("cannot run cargo: {error}"))?;
    if !status.success() {
        return Err(format!(
            "cargo build failed for {}/{}: {status}",
            profile.name(),
            variant.name()
        ));
    }

    std::fs::create_dir_all(&out_dir)
        .map_err(|error| format!("cannot create {}: {error}", out_dir.display()))?;
    for bin in BINS {
        let file = exe_name(bin);
        let source = root.join("target").join(profile.name()).join(&file);
        let target = out_dir.join(&file);
        std::fs::copy(&source, &target)
            .map_err(|error| format!("cannot copy {}: {error}", source.display()))?;
        println!("  {}  {} bytes", target.display(), file_size(&target)?);
    }
    Ok(())
}

fn upx(profile: Profile, variant: Variant) -> Result<(), String> {
    let root = workspace_root();
    let source_dir = artifact_dir(&root, profile, variant);
    if !source_dir.exists() {
        return Err(format!(
            "{} not found; run `cargo xtask acp build {} --profile {}` first",
            source_dir.display(),
            variant.name(),
            profile.name()
        ));
    }

    let upx_bin = std::env::var("UPX_EXE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "upx".to_string());
    let available = matches!(
        Command::new(&upx_bin).arg("--version").output(),
        Ok(output) if output.status.success()
    );
    if !available {
        return Err(format!(
            "upx not found (tried `{upx_bin}`): install UPX 5.x and add it to PATH, \
             or set UPX_EXE=C:\\path\\to\\upx.exe\n\
             downloads: https://github.com/upx/upx/releases"
        ));
    }

    let out_dir = source_dir.join("upx");
    std::fs::create_dir_all(&out_dir)
        .map_err(|error| format!("cannot create {}: {error}", out_dir.display()))?;
    for bin in BINS {
        let file = exe_name(bin);
        let source = source_dir.join(&file);
        let target = out_dir.join(&file);
        if target.exists() {
            std::fs::remove_file(&target)
                .map_err(|error| format!("cannot remove {}: {error}", target.display()))?;
        }
        eprintln!("[xtask] packing {file} ...");
        let pack = Command::new(&upx_bin)
            .args(["--best", "--lzma", "-q", "-o"])
            .arg(&target)
            .arg(&source)
            .status()
            .map_err(|error| format!("cannot run {upx_bin}: {error}"))?;
        if !pack.success() {
            return Err(format!("upx pack failed on {file}"));
        }
        let test = Command::new(&upx_bin)
            .args(["-t", "-q"])
            .arg(&target)
            .status()
            .map_err(|error| format!("cannot run {upx_bin}: {error}"))?;
        if !test.success() {
            return Err(format!("upx integrity test failed on {file}"));
        }
        println!(
            "  {file}: {} bytes -> {} bytes",
            file_size(&source)?,
            file_size(&target)?
        );
    }
    Ok(())
}

/// crates.io order for a release: every package comes after everything it
/// depends on, so each upload finds its dependencies on the index. `xtask` and
/// `examples/durable-quickstart` are `publish = false` and are not listed.
///
/// Versioned `[dev-dependencies]` count too: `cargo package` resolves them, so
/// `agentscope-macros` sits after `agentscope-core` even though it only needs it
/// for its own tests. The two version-less dev links (`durable` →
/// `durable-sqlite`, `rocketmq` → `durable-worker`) are dropped while packaging
/// and therefore not edges here.
///
/// Derived from `cargo metadata`; if a crate gains an internal dependency, the
/// new edge has to be reflected here (and in `docs/MIGRATION_0.2.0.md`).
const PUBLISH_ORDER: &[&str] = &[
    "agentscope-core",
    "agentscope-extensions-model",
    "agentscope-harness",
    "agentscope-macros",
    "agentscope-durable",
    "agentscope-durable-sqlite",
    "agentscope-durable-worker",
    "agentscope-durable-jdbc",
    "agentscope-durable-objects",
    "agentscope-durable-http",
    "agentscope-durable-mcp",
    "agentscope-durable-rocketmq",
    "agentscope-durable-sandbox",
    "agentscope-durable-adapter-agentscope",
    "agentscope-extensions-a2a-client",
    "agentscope-extensions-chat-completions-web",
    "agentscope-extensions-mem",
    "agentscope-extensions-scheduler",
    "agentscope-extensions-a2a-server",
    "agentscope-extensions-agent-protocol",
    "agentscope-extensions-agui",
    "agentscope-extensions-channel",
    "agentscope-extensions-cos",
    "agentscope-extensions-higress",
    "agentscope-extensions-judge",
    "agentscope-extensions-mongodb",
    "agentscope-extensions-oss",
    "agentscope-extensions-postgresql",
    "agentscope-extensions-rag",
    "agentscope-extensions-redis",
    "agentscope-extensions-sandbox",
    "agentscope-extensions-skills",
    "agentscope-extensions-studio",
    "agentscope-extensions-training",
    "agentscope-extensions-protocol",
    "agentscope-extensions-jdbc",
    "agentscope-extensions-mysql",
    "agentscope-extensions-nacos",
    "agentscope-extensions-acp",
    "agentscope-rs",
];

fn publish(dry_run: bool, sleep: Duration) -> Result<(), String> {
    let root = workspace_root();
    let total = PUBLISH_ORDER.len();
    for (index, name) in PUBLISH_ORDER.iter().enumerate() {
        // The index is eventually consistent: give it time to show the crate
        // uploaded on the previous iteration.
        if index > 0 && !dry_run && !sleep.is_zero() {
            std::thread::sleep(sleep);
        }
        eprintln!("[xtask] publish {name} ({}/{total})", index + 1);

        let mut command = Command::new("cargo");
        command.current_dir(&root).args(["publish", "-p", name]);
        if dry_run {
            command.arg("--dry-run");
        }
        let status = command
            .status()
            .map_err(|error| format!("cannot run cargo: {error}"))?;
        if !status.success() {
            return Err(format!("cargo publish failed for {name}: {status}"));
        }
    }
    Ok(())
}

const ACP_ARTIFACT_PREFIX: &str = "agentscope-acp";
const ACP_REGISTRY_ID: &str = "agentscope-rust";
const ACP_REGISTRY_NAME: &str = "AgentScope Rust";
const ACP_REGISTRY_DESCRIPTION: &str =
    "ACP-native coding agent with multi-provider models, tool permissions, session persistence and subagents.";
const ACP_REGISTRY_AUTHOR: &str = "Endlessworld";

/// ACP Registry 认的平台键、压缩包扩展名与压缩包内 agent 的相对路径。
/// Android 产物不在内：它不是 Registry 平台。
const ACP_REGISTRY_PLATFORMS: [(&str, &str, &str); 6] = [
    ("darwin-aarch64", "tar.gz", "./acp_harness_agent"),
    ("darwin-x86_64", "tar.gz", "./acp_harness_agent"),
    ("linux-aarch64", "tar.gz", "./acp_harness_agent"),
    ("linux-x86_64", "tar.gz", "./acp_harness_agent"),
    ("windows-aarch64", "zip", "./acp_harness_agent.exe"),
    ("windows-x86_64", "zip", "./acp_harness_agent.exe"),
];

/// Build the ACP Registry `agent.json` from a release's `SHA256SUMS.txt`.
///
/// The version (and therefore every archive URL) comes from
/// `[workspace.package] version`, so the metadata cannot drift from the tag the
/// workflow checked out.
fn registry(artifacts: &str, out: Option<PathBuf>) -> Result<(), String> {
    let root = workspace_root();
    let dir = root.join(artifacts);
    let checksums = read_checksums(&dir.join("SHA256SUMS.txt"))?;
    let version = workspace_package_field("version")?;
    let repository = workspace_package_field("repository")?;
    let json = registry_json(&version, &repository, &checksums)?;

    let out = out.unwrap_or_else(|| {
        root.join(".acp-registry")
            .join(ACP_REGISTRY_ID)
            .join("agent.json")
    });
    std::fs::write(&out, &json)
        .map_err(|error| format!("cannot write {}: {error}", out.display()))?;
    eprintln!(
        "[xtask] {version} -> {} ({} bytes)",
        out.display(),
        json.len()
    );
    print!("{json}");
    Ok(())
}

/// `sha256sum` output: `<hex>  <file name>` per line.
fn read_checksums(path: &Path) -> Result<Vec<(String, String)>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut entries: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(digest), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        entries.push((digest.to_string(), name.to_string()));
    }
    if entries.is_empty() {
        return Err(format!("{} has no `<sha256> <file>` lines", path.display()));
    }
    Ok(entries)
}

fn registry_json(
    version: &str,
    repository: &str,
    checksums: &[(String, String)],
) -> Result<String, String> {
    let mut binaries = String::new();
    for (index, (platform, extension, cmd)) in ACP_REGISTRY_PLATFORMS.iter().enumerate() {
        let asset = format!("{ACP_ARTIFACT_PREFIX}-{platform}.{extension}");
        let digest = checksums
            .iter()
            .find(|(_, name)| name == &asset)
            .map(|(digest, _)| digest.as_str())
            .ok_or_else(|| format!("SHA256SUMS.txt has no entry for {asset}"))?;
        let tail = if index + 1 == ACP_REGISTRY_PLATFORMS.len() {
            ""
        } else {
            ","
        };
        binaries.push_str(&format!(
            "      \"{platform}\": {{\n        \"archive\": \"{repository}/releases/download/v{version}/{asset}\",\n        \"cmd\": \"{cmd}\",\n        \"sha256\": \"{digest}\"\n      }}{tail}\n"
        ));
    }

    Ok(format!(
        "{{\n  \"id\": \"{ACP_REGISTRY_ID}\",\n  \"name\": \"{ACP_REGISTRY_NAME}\",\n  \"version\": \"{version}\",\n  \"description\": \"{ACP_REGISTRY_DESCRIPTION}\",\n  \"repository\": \"{repository}\",\n  \"website\": \"{repository}/blob/main/crates/agentscope-extensions-acp/README.md\",\n  \"authors\": [\n    \"{ACP_REGISTRY_AUTHOR}\"\n  ],\n  \"license\": \"Apache-2.0\",\n  \"license_url\": \"{repository}/blob/main/LICENSE\",\n  \"distribution\": {{\n    \"binary\": {{\n{binaries}    }}\n  }}\n}}\n"
    ))
}

/// Read one `key = "value"` line from `[workspace.package]` in the root manifest.
///
/// Deliberately a hand-rolled scan: xtask must stay dependency-free, and the
/// root manifest is ours.
fn workspace_package_field(field: &str) -> Result<String, String> {
    let path = workspace_root().join("Cargo.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside = line == "[workspace.package]";
            continue;
        }
        if !inside {
            continue;
        }
        let Some(rest) = line.strip_prefix(field) else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        if !value.is_empty() {
            return Ok(value.to_string());
        }
    }
    Err(format!("`{field}` not found in [workspace.package]"))
}

fn workspace_root() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `<root>/xtask` for this package.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must live directly inside the workspace")
        .to_path_buf()
}

fn artifact_dir(root: &Path, profile: Profile, variant: Variant) -> PathBuf {
    root.join("target")
        .join("acp")
        .join(profile.name())
        .join(variant.name())
}

fn exe_name(bin: &str) -> String {
    if cfg!(windows) {
        format!("{bin}.exe")
    } else {
        bin.to_string()
    }
}

fn file_size(path: &Path) -> Result<u64, String> {
    std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|error| format!("cannot stat {}: {error}", path.display()))
}
