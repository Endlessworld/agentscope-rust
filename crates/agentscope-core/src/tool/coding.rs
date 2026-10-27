//! Command validation for coding tools, mirroring the tree-sitter based Bash
//! safety analysis. This lightweight validator uses an allowlist/denylist
//! strategy instead of a full Bash AST.
use crate::error::{Error, Result};

/// Commands that are always rejected.
const DENYLIST: &[&str] = &[
    "rm -rf /",
    "sudo",
    "mkfs",
    ":(){",
    "dd if=",
    "> /dev/sda",
    "shutdown",
    "reboot",
    "halt",
    "chmod -R 777 /",
    "mv /* ",
    "kill -9 1",
    "init 0",
];

/// Shell metacharacters that are banned unless quoted safely.
const FORBIDDEN_META: &[char] = &[';', '&', '|', '`', '$', '<', '>'];

/// The result of a command validation check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandCheck {
    pub allowed: bool,
    pub reason: Option<String>,
}

impl CommandCheck {
    pub fn allow() -> Self {
        Self {
            allowed: true,
            reason: None,
        }
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            allowed: false,
            reason: Some(reason.into()),
        }
    }
}

/// Validates a shell command string for dangerous patterns.
pub fn validate_command(command: &str) -> Result<CommandCheck> {
    if command.trim().is_empty() {
        return Err(Error::Tool("command must not be empty".into()));
    }
    for pattern in DENYLIST {
        if command.contains(pattern) {
            return Ok(CommandCheck::deny(format!(
                "command contains forbidden pattern '{}'",
                pattern
            )));
        }
    }
    // A naive metacharacter scan: commands run through this validator should
    // be single commands (no chaining/redirection).
    if command.chars().any(|c| FORBIDDEN_META.contains(&c)) {
        return Ok(CommandCheck::deny(
            "command contains shell metacharacters; use a single command",
        ));
    }
    Ok(CommandCheck::allow())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_simple_command() {
        let check = validate_command("ls -la").unwrap();
        assert!(check.allowed);
    }

    #[test]
    fn denies_denylisted() {
        let check = validate_command("sudo rm -rf /").unwrap();
        assert!(!check.allowed);
    }

    #[test]
    fn denies_metacharacters() {
        let check = validate_command("echo hi; rm -rf /").unwrap();
        assert!(!check.allowed);
    }
}
