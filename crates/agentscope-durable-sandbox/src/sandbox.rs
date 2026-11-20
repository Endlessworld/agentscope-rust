//! The process port: the three things a sandboxed runner has to answer.

use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Bytes kept per stream when the call does not ask for a different bound.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// One process to run inside a sandbox.
///
/// The fields are the call's own values, unchanged: a sandbox adapter never rewrites a command,
/// reorders arguments, normalises a working directory or injects an environment variable. Whatever
/// the model asked for is what the sandbox is asked to run, because the effect that gets recorded
/// must be the effect that was intended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecSpec {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    /// Wall-clock budget for the whole execution; `None` means "no deadline".
    pub timeout_ms: Option<u64>,
    /// Bytes kept per output stream (see [`crate::capture`]).
    pub max_output_bytes: usize,
}

impl ExecSpec {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            timeout_ms: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    /// Reads the spec a tool call carries, without touching any of it.
    ///
    /// The names are the durable JSON conventions (`timeoutMs`, `maxOutputBytes`) with the
    /// snake-case spellings accepted as aliases, so a hand-written call and a generated one both
    /// work. Unknown keys are ignored: a call may carry model-facing hints the sandbox has no
    /// business persisting.
    pub fn from_arguments(arguments: &Value) -> Result<Self, SandboxError> {
        let object = arguments
            .as_object()
            .ok_or_else(|| SandboxError::invalid("sandbox call arguments must be a JSON object"))?;
        let command = text(object, &["command"], "command")?
            .ok_or_else(|| SandboxError::invalid("a sandbox call needs a `command`"))?;
        if command.trim().is_empty() {
            return Err(SandboxError::invalid("`command` must not be blank"));
        }
        let args = match lookup(object, &["args"]) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str().map(str::to_string).ok_or_else(|| {
                        SandboxError::invalid("every element of `args` must be a string")
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => {
                return Err(SandboxError::invalid("`args` must be an array of strings"));
            }
        };
        let cwd = text(object, &["cwd"], "cwd")?;
        let env = match lookup(object, &["env"]) {
            None | Some(Value::Null) => BTreeMap::new(),
            Some(Value::Object(entries)) => {
                let mut env = BTreeMap::new();
                for (key, value) in entries {
                    let value = value.as_str().ok_or_else(|| {
                        SandboxError::invalid(format!(
                            "the environment variable `{key}` must be a string"
                        ))
                    })?;
                    env.insert(key.clone(), value.to_string());
                }
                env
            }
            Some(_) => {
                return Err(SandboxError::invalid(
                    "`env` must be an object of string values",
                ));
            }
        };
        let timeout_ms = number(object, &["timeoutMs", "timeout_ms"], "timeoutMs")?;
        let max_output_bytes = number(
            object,
            &["maxOutputBytes", "max_output_bytes"],
            "maxOutputBytes",
        )?
        .map(|value| usize::try_from(value).unwrap_or(usize::MAX))
        .unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);
        Ok(Self {
            command,
            args,
            cwd,
            env,
            timeout_ms,
            max_output_bytes,
        })
    }
}

fn lookup<'a>(object: &'a Map<String, Value>, names: &[&str]) -> Option<&'a Value> {
    names.iter().find_map(|name| object.get(*name))
}

fn text(
    object: &Map<String, Value>,
    names: &[&str],
    what: &str,
) -> Result<Option<String>, SandboxError> {
    match lookup(object, names) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(SandboxError::invalid(format!("`{what}` must be a string"))),
    }
}

fn number(
    object: &Map<String, Value>,
    names: &[&str],
    what: &str,
) -> Result<Option<u64>, SandboxError> {
    match lookup(object, names) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => value.as_u64().map(Some).ok_or_else(|| {
            SandboxError::invalid(format!("`{what}` must be a non-negative integer"))
        }),
        Some(_) => Err(SandboxError::invalid(format!(
            "`{what}` must be a non-negative integer"
        ))),
    }
}

/// The sandbox's name for one execution. Opaque to the runtime: it is carried in the durable
/// result verbatim so a later query can be answered by whoever holds the sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handle {
    pub id: String,
}

impl Handle {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

/// What a sandbox knows about an execution right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    /// No verdict yet. A percentage is reported when the backend has one; `0` otherwise.
    Running { progress: u8 },
    /// The process is gone. `truncated` says the backend itself dropped bytes; the adapter also
    /// applies [`ExecSpec::max_output_bytes`] on what it receives.
    Exited {
        code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        truncated: bool,
    },
}

impl Poll {
    pub fn running(progress: u8) -> Self {
        Self::Running { progress }
    }

    pub fn exited(code: i32, stdout: impl Into<Vec<u8>>, stderr: impl Into<Vec<u8>>) -> Self {
        Self::Exited {
            code,
            stdout: stdout.into(),
            stderr: stderr.into(),
            truncated: false,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

/// Runs processes in a sandbox. Implementations own the outside world; this crate owns the
/// translation into durable facts.
///
/// Two rules make the translation trustworthy:
///
/// * `poll` reports [`Poll::Running`] only when there is genuinely no verdict. A backend that
///   lost track of an execution returns a [`SandboxErrorKind::NotFound`] error, which becomes
///   `UNKNOWN`, never a success.
/// * `poll` must return within the spec's deadline; the adapter also enforces it, and a kill is
///   the adapter's answer to a backend that misses it.
#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn spawn(&self, spec: ExecSpec) -> Result<Handle, SandboxError>;
    async fn poll(&self, handle: &Handle) -> Result<Poll, SandboxError>;
    async fn kill(&self, handle: &Handle) -> Result<(), SandboxError>;
}

/// Why a sandbox call failed. The kind decides how the durable runtime treats the effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxErrorKind {
    /// The request was rejected before anything ran: no effect can exist.
    Invalid,
    /// The sandbox has no record of the handle; whether the effect happened is not knowable here.
    NotFound,
    /// The sandbox could not be reached or is temporarily failing.
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxError {
    kind: SandboxErrorKind,
    message: String,
}

impl SandboxError {
    pub fn new(kind: SandboxErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(SandboxErrorKind::Invalid, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(SandboxErrorKind::NotFound, message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(SandboxErrorKind::Unavailable, message)
    }

    pub fn kind(&self) -> SandboxErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SandboxError {}
