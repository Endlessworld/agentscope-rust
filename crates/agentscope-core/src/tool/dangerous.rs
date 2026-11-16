//! Dangerous path / command constants and self-check helpers.
//! Mirrors `io.agentscope.core.tool.ToolDangerousPathConstants` and the related
//! `isDangerousPath` logic on `ToolBase`.
//!
//! These lists guard sensitive configuration files (shell rc files, SSH config,
//! credentials) and disruptive shell commands. Tools that override the fields
//! replace the defaults entirely rather than appending to them.

/// Sensitive files that should never be auto-edited.
pub const DEFAULT_DANGEROUS_FILES: &[&str] = &[
    ".gitconfig",
    ".gitmodules",
    ".bashrc",
    ".bash_profile",
    ".zshrc",
    ".zprofile",
    ".profile",
    ".ssh/config",
    ".ssh/authorized_keys",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".env",
    ".envrc",
    ".env.local",
    ".env.development",
    ".env.development.local",
    ".env.test",
    ".env.test.local",
    ".env.staging",
    ".env.production",
    ".env.production.local",
];

/// Directory names whose presence anywhere in a path marks it sensitive.
pub const DEFAULT_DANGEROUS_DIRECTORIES: &[&str] = &[".git", ".vscode", ".idea", ".ssh"];

/// Disruptive commands that always require explicit user approval.
///
/// The entries are matched as whole command words, never as substrings: a plain `contains`
/// scan used to reject ordinary commands such as `git add` (`dd`) or `cargo fmt --format`
/// (`format`). Recursive force `rm` and recursive permissive `chmod` are matched
/// structurally in [`is_dangerous_command`] because they are only destructive for some
/// targets (`rm -rf victim` inside the workspace is fine, `rm -rf .agentscope` is not).
pub const DANGEROUS_COMMANDS: &[&str] = &[
    "mkfs", "mke2fs", "wipefs", "fdisk", "shutdown", "reboot", "poweroff", "halt",
];

/// Device names that are never a disk (so redirecting into them is harmless).
const HARMLESS_DEVICE_PREFIXES: &[&str] = &[
    "null", "zero", "one", "full", "random", "urandom", "stdin", "stdout", "stderr", "tty", "ptmx",
    "pts/", "console", "fd/",
];

/// Directory names that hold the whole workspace or its version history.
const PROTECTED_DIRS: &[&str] = &[".agentscope", ".git"];

/// Shell wrappers that only prefix the command that actually runs.
const WRAPPERS: &[&str] = &["sudo", "doas", "env", "nohup", "command", "time"];

/// Normalizes path separators and a leading `./` for matching.
fn normalize(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches("./").to_string()
}

/// Returns `true` if `path` targets a sensitive file or lies inside a sensitive
/// directory. Matches on normalized (forward-slash) segments; a bare leading
/// `./` and a leading tilde are ignored for the directory-segment check.
pub fn is_dangerous_path(path: &str) -> bool {
    let normalized = normalize(path);
    if normalized.is_empty() {
        return false;
    }
    // Dangerous directory present as any path segment.
    for seg in normalized.split('/') {
        if DEFAULT_DANGEROUS_DIRECTORIES.contains(&seg) {
            return true;
        }
    }
    // Dangerous file matches the basename or a full/relative tail.
    let bare = normalized.trim_start_matches('~');
    DEFAULT_DANGEROUS_FILES
        .iter()
        .any(|f| bare == *f || bare.ends_with(&format!("/{f}")))
}

/// Returns `true` if the command destroys a whole system or a whole workspace.
///
/// Deliberately narrow: only system-level and whole-workspace forms are flagged, so a plain
/// `rm file.txt` or a recursive delete of a workspace child stays executable and the
/// approval path (not a block list) remains the general gate for everything else.
pub fn is_dangerous_command(command: &str) -> bool {
    let command = command.trim();
    if command.is_empty() {
        return false;
    }
    let lower = command.to_lowercase();
    // Fork bombs are matched on the whitespace-free form: `:(){ :|:& };:` and `:(){:|:&};:`
    // are the same program.
    let compact: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.contains(":(){:|:&};:") {
        return true;
    }
    // Writing to a raw device destroys a disk, whatever the command is (`dd of=…`,
    // `cat x > /dev/sda`).
    if writes_to_a_raw_device(&lower) {
        return true;
    }
    lower
        .split([';', '&', '|', '\n', '\r'])
        .any(segment_is_destructive)
}

/// Whether one `;` / `&` / `|` separated command destroys a whole system or workspace.
fn segment_is_destructive(segment: &str) -> bool {
    let words: Vec<&str> = segment.split_whitespace().collect();
    // Skip wrappers (`sudo rm …`, `FOO=1 rm …`) so the real verb is inspected.
    let mut index = 0;
    while index < words.len() {
        let word = words[index];
        let wrapped = WRAPPERS.contains(&program_name(word))
            || (!word.starts_with('-') && word.contains('='));
        if !wrapped {
            break;
        }
        index += 1;
    }
    let Some(program) = words.get(index).copied().map(program_name) else {
        return false;
    };
    let arguments = &words[index + 1..];
    match program {
        "rm" => rm_is_destructive(arguments),
        // The PowerShell spelling of `rm -rf` (`execute` runs pwsh on Windows).
        "remove-item" => remove_item_is_destructive(arguments),
        "chmod" => chmod_is_destructive(arguments),
        // `systemctl reboot`, `service nginx halt`.
        "systemctl" | "service" => DANGEROUS_COMMANDS
            .iter()
            .any(|verb| arguments.contains(verb)),
        // `mkfs.ext4`, `mkfs.xfs`, …
        program => DANGEROUS_COMMANDS.contains(&program) || program.starts_with("mkfs."),
    }
}

/// Basename of a command word without a path or `.exe` suffix.
fn program_name(word: &str) -> &str {
    let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
    name.strip_suffix(".exe").unwrap_or(name)
}

/// Recursive force `rm` against a system-level or whole-workspace target.
fn rm_is_destructive(arguments: &[&str]) -> bool {
    let mut recursive = false;
    let mut force = false;
    let mut targets: Vec<&str> = Vec::new();
    for argument in arguments.iter().copied() {
        if let Some(long) = argument.strip_prefix("--") {
            match long {
                "recursive" => recursive = true,
                "force" => force = true,
                _ => {}
            }
        } else if let Some(flags) = argument.strip_prefix('-').filter(|f| !f.is_empty()) {
            recursive |= flags.contains('r') || flags.contains('R');
            force |= flags.contains('f');
        } else {
            targets.push(argument);
        }
    }
    recursive && force && targets.iter().copied().any(is_catastrophic_target)
}

/// Whether deleting `target` removes more than a file inside the workspace.
fn is_catastrophic_target(target: &str) -> bool {
    let target = target.trim_matches(|c| c == '"' || c == '\'' || c == '`');
    if target.is_empty() || target == "*" || target == "/*" {
        return true;
    }
    let trimmed = target.trim_end_matches(['/', '\\']);
    let trimmed = if trimmed.is_empty() { "/" } else { trimmed };
    // Home / variable expansion, absolute paths (they leave the workspace) …
    if trimmed.starts_with('~') || trimmed.starts_with('$') || trimmed.starts_with('%') {
        return true;
    }
    let bytes = trimmed.as_bytes();
    let drive_rooted = bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic();
    if trimmed.starts_with('/') || trimmed.starts_with('\\') || drive_rooted {
        return true;
    }
    // … the current directory itself, the parent, or the workspace root / history.
    trimmed == "."
        || trimmed
            .split(['/', '\\'])
            .any(|segment| segment == ".." || PROTECTED_DIRS.contains(&segment))
}

/// PowerShell `Remove-Item -Recurse -Force` against a system-level or whole-workspace target.
fn remove_item_is_destructive(arguments: &[&str]) -> bool {
    let mut recursive = false;
    let mut force = false;
    let mut targets: Vec<&str> = Vec::new();
    for argument in arguments.iter().copied() {
        match argument.strip_prefix('-').map(str::to_lowercase).as_deref() {
            Some("recurse") => recursive = true,
            Some("force") => force = true,
            Some(_) => {}
            None => targets.push(argument),
        }
    }
    recursive && force && targets.iter().copied().any(is_catastrophic_target)
}

/// Recursive `chmod` that makes the filesystem root world-writable.
fn chmod_is_destructive(arguments: &[&str]) -> bool {
    let mut recursive = false;
    let mut mode: Option<&str> = None;
    let mut targets: Vec<&str> = Vec::new();
    for argument in arguments.iter().copied() {
        if argument == "--recursive" {
            recursive = true;
        } else if let Some(flags) = argument.strip_prefix('-').filter(|f| !f.is_empty()) {
            // The command arrives lower-cased, so `-R` and `-r` are indistinguishable here;
            // both are treated as recursive because the target check still guards the verdict.
            recursive |= flags.contains('R') || flags.contains('r');
        } else if mode.is_none() && argument.chars().all(|c| c.is_ascii_digit()) {
            mode = Some(argument);
        } else {
            targets.push(argument);
        }
    }
    let permissive = matches!(mode, Some("777" | "0777" | "666" | "0666"));
    recursive
        && permissive
        && targets
            .iter()
            .any(|target| *target == "/" || *target == "/*")
}

/// Whether the command mentions a `/dev/` node that is not a harmless pseudo device.
fn writes_to_a_raw_device(command: &str) -> bool {
    for (index, _) in command.match_indices("/dev/") {
        let rest = &command[index + "/dev/".len()..];
        let name = rest
            .split(|c: char| {
                c.is_whitespace() || matches!(c, ';' | '|' | '&' | '"' | '\'' | ')' | '`')
            })
            .next()
            .unwrap_or("");
        if name.is_empty() {
            continue;
        }
        if !HARMLESS_DEVICE_PREFIXES
            .iter()
            .any(|safe| name.starts_with(safe))
        {
            return true;
        }
    }
    false
}
