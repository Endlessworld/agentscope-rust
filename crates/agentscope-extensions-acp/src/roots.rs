//! Workspace roots declared by the ACP client (`_meta["io.animus/roots"]`).
//!
//! A remote client has two filesystems: the agent host (reachable with the built-in tools) and
//! its own device (reachable only through the MCP tools the client injects). The standard
//! `additionalDirectories` field carries both kinds of path, so every path needs an origin: a
//! `server` root widens the agent's sandbox, a `client` root is only described to the model.

use std::path::{Path, PathBuf};

use agent_client_protocol::v1::schema::Meta;

/// `_meta` key carrying the origin of each declared root.
pub const ROOTS_META_KEY: &str = "io.animus/roots";

/// Where a declared directory lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootOrigin {
    /// On the agent host: the built-in tools may open it.
    Server,
    /// On the client device: only the client-injected MCP tools reach it.
    Client,
}

/// A directory the client authorized, with the extra context it attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientRoot {
    pub path: PathBuf,
    pub label: Option<String>,
    pub tools: Option<String>,
    pub platform: Option<String>,
}

/// One entry as it appears on the wire.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RawRoot {
    pub path: String,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub tools: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
}

/// The declaration carried by `session/new` `_meta[ROOTS_META_KEY]`.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct RawDeclaration {
    #[serde(default)]
    pub cwd: Option<RawRoot>,
    #[serde(default)]
    pub additional: Vec<RawRoot>,
}

/// Origin of every root the client declared for one session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootsDeclaration {
    /// Paths the client marked as living on the agent host.
    server: Vec<String>,
    /// Paths the client marked as living on its own device.
    clients: Vec<ClientRoot>,
}

impl RootsDeclaration {
    /// Reads the declaration; an absent or malformed entry means "no declaration".
    pub fn from_meta(meta: Option<&Meta>) -> Self {
        let Some(raw) = meta.and_then(|map| map.get(ROOTS_META_KEY)) else {
            return Self::default();
        };
        let Ok(declared) = serde_json::from_value::<RawDeclaration>(raw.clone()) else {
            return Self::default();
        };
        let mut declaration = Self::default();
        let entries = declared
            .cwd
            .into_iter()
            .chain(declared.additional.into_iter());
        for root in entries {
            let origin = match root.origin.as_deref() {
                Some("client") => RootOrigin::Client,
                _ => RootOrigin::Server,
            };
            declaration.push(root, origin);
        }
        declaration
    }

    fn push(&mut self, root: RawRoot, origin: RootOrigin) {
        let path = root.path.trim().to_string();
        if path.is_empty() {
            return;
        }
        match origin {
            RootOrigin::Client => {
                let key = path_key(&path);
                if self.clients.iter().any(|root| path_key(&root.path.to_string_lossy()) == key) {
                    return;
                }
                self.clients.push(ClientRoot {
                    path: PathBuf::from(path),
                    label: non_empty(root.label),
                    tools: non_empty(root.tools),
                    platform: non_empty(root.platform),
                });
            }
            RootOrigin::Server => {
                let key = path_key(&path);
                if !self.server.iter().any(|known| path_key(known) == key) {
                    self.server.push(path);
                }
            }
        }
    }

    /// Splits `additionalDirectories` by declared origin.
    ///
    /// A path without a declaration is assumed to live on the agent host: that is what a local
    /// client means, and it keeps the historical behaviour when `_meta` is absent.
    pub fn split(&self, paths: &[PathBuf]) -> (Vec<PathBuf>, Vec<ClientRoot>) {
        let mut server = Vec::new();
        let mut clients: Vec<ClientRoot> = Vec::new();
        for path in paths {
            let key = path_key(&path.to_string_lossy());
            let declared_server = self.server.iter().any(|known| path_key(known) == key);
            match self
                .clients
                .iter()
                .find(|root| path_key(&root.path.to_string_lossy()) == key && !declared_server)
            {
                Some(root) => clients.push(root.clone()),
                None => server.push(path.clone()),
            }
        }
        (server, clients)
    }

    /// Splits by probing the local disk instead of a declaration.
    ///
    /// `session/load|resume` carry `additionalDirectories` but no `_meta` (schema 1.22 has no
    /// meta on those requests), so a path that does not exist here can only live elsewhere.
    pub fn split_by_existence(paths: &[PathBuf]) -> (Vec<PathBuf>, Vec<ClientRoot>) {
        let mut server = Vec::new();
        let mut clients = Vec::new();
        for path in paths {
            if path.is_dir() {
                server.push(path.clone());
            } else {
                clients.push(ClientRoot {
                    path: path.clone(),
                    label: None,
                    tools: None,
                    platform: None,
                });
            }
        }
        (server, clients)
    }

    /// Client-side roots of the declaration (used by tests and by callers that only need them).
    pub fn client_roots(&self) -> &[ClientRoot] {
        &self.clients
    }
}

/// Appends the boundary note to the client's system prompt.
pub fn with_boundary_note(
    prompt: Option<String>,
    cwd: &Path,
    client_roots: &[ClientRoot],
) -> Option<String> {
    let Some(note) = boundary_note(cwd, client_roots) else {
        return prompt;
    };
    Some(match prompt {
        Some(existing) if !existing.trim().is_empty() => format!("{existing}\n\n{note}"),
        _ => note,
    })
}

/// The prompt block that tells the model which tool reaches which directory.
///
/// Without client roots the agent is the only filesystem and the block is unnecessary: the
/// built-in tools already describe themselves.
pub fn boundary_note(cwd: &Path, client_roots: &[ClientRoot]) -> Option<String> {
    if client_roots.is_empty() {
        return None;
    }
    let mut note = String::from("## 文件系统边界（本会话）\n");
    note.push_str(&format!(
        "- 服务端工作目录 cwd：{}。内置工具（read_file / write_file / edit_file / list_files / \
         glob_files / grep_files / execute）只能访问服务端路径，相对路径以它为基准。\n",
        cwd.display()
    ));
    note.push_str("- 客户端目录（另一台设备，必须用下列工具访问，参数写客户端绝对路径）：\n");
    for root in client_roots {
        let tools = root.tools.as_deref().unwrap_or("client_fs_* / client_execute");
        match root.label.as_deref() {
            Some(label) if !label.trim().is_empty() => note.push_str(&format!(
                "  - {}（{}）-> 用 {}、client_execute\n",
                root.path.display(),
                label,
                tools
            )),
            _ => note.push_str(&format!(
                "  - {} -> 用 {}、client_execute\n",
                root.path.display(),
                tools
            )),
        }
    }
    note.push_str(
        "不要用内置工具打开客户端路径，也不要把服务端路径传给 client_fs_* / client_execute。\n",
    );
    Some(note)
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.trim().is_empty())
}

/// Comparison key for a declared path: trailing separators dropped, case folded on Windows.
fn path_key(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_tail = trimmed.trim_end_matches(['/', '\\']);
    let base = if without_tail.is_empty() { trimmed } else { without_tail };
    if cfg!(windows) {
        base.to_lowercase()
    } else {
        base.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta(value: serde_json::Value) -> Meta {
        let mut map = Meta::new();
        map.insert(ROOTS_META_KEY.to_string(), value);
        map
    }

    #[test]
    fn a_declaration_splits_server_and_client_roots() {
        let declaration = RootsDeclaration::from_meta(Some(&meta(json!({
            "cwd": {"path": "/srv/repo", "origin": "server", "label": "仓库"},
            "additional": [
                {"path": "/srv/shared", "origin": "server"},
                {"path": "D:/work/proj", "origin": "client", "label": "本机", "tools": "animus-client-fs", "platform": "windows"}
            ]
        }))));
        let paths = vec![
            PathBuf::from("/srv/shared"),
            PathBuf::from("D:/work/proj"),
        ];
        let (server, clients) = declaration.split(&paths);
        assert_eq!(server, vec![PathBuf::from("/srv/shared")]);
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].path, PathBuf::from("D:/work/proj"));
        assert_eq!(clients[0].label.as_deref(), Some("本机"));
    }

    #[test]
    fn without_a_declaration_every_path_stays_on_the_agent_host() {
        let declaration = RootsDeclaration::from_meta(None);
        let paths = vec![PathBuf::from("/srv/shared"), PathBuf::from("/srv/other")];
        let (server, clients) = declaration.split(&paths);
        assert_eq!(server, paths);
        assert!(clients.is_empty());
    }

    #[test]
    fn a_malformed_declaration_degrades_to_no_declaration() {
        let declaration = RootsDeclaration::from_meta(Some(&meta(json!({"additional": 3}))));
        let (server, clients) = declaration.split(&[PathBuf::from("/srv/repo")]);
        assert_eq!(server, vec![PathBuf::from("/srv/repo")]);
        assert!(clients.is_empty());
    }

    #[test]
    fn a_missing_directory_is_treated_as_a_client_root() {
        let mut missing = std::env::temp_dir();
        missing.push(format!("agentscope-roots-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let existing = std::env::temp_dir();
        let (server, clients) =
            RootsDeclaration::split_by_existence(&[existing.clone(), missing.clone()]);
        assert_eq!(server, vec![existing]);
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].path, missing);
    }

    #[test]
    fn the_boundary_note_names_the_client_tools() {
        assert!(boundary_note(Path::new("/srv/repo"), &[]).is_none());
        let roots = vec![ClientRoot {
            path: PathBuf::from("D:/work/proj"),
            label: Some("本机".to_string()),
            tools: Some("animus-client-fs".to_string()),
            platform: Some("windows".to_string()),
        }];
        let note = boundary_note(Path::new("/srv/repo"), &roots).expect("a note");
        assert!(note.contains("D:/work/proj"));
        assert!(note.contains("client_execute"));
        assert!(note.contains("/srv/repo"));
    }

    #[test]
    fn the_note_is_appended_after_the_client_prompt() {
        let roots = vec![ClientRoot {
            path: PathBuf::from("D:/work/proj"),
            label: None,
            tools: None,
            platform: None,
        }];
        let merged = with_boundary_note(Some("keep it short".to_string()), Path::new("/srv"), &roots)
            .expect("merged prompt");
        assert!(merged.starts_with("keep it short"));
        assert!(merged.contains("D:/work/proj"));
    }
}
