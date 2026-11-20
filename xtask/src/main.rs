//! Workspace task runner.
//!
//! ```text
//! cargo xtask acp build <variant> [--profile release|embedded]
//! cargo xtask acp upx   <variant> [--profile release|embedded]
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

const USAGE: &str = "\
xtask -- workspace task runner

usage:
  cargo xtask acp build <variant> [--profile release|embedded]
  cargo xtask acp upx   <variant> [--profile release|embedded]

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
    let mut words: Vec<String> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--profile" => {
                let value = args.next().ok_or("--profile needs a value")?;
                profile = Profile::parse(&value)?;
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
