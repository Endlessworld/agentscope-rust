//! Routes file operations to different `Filesystem` stores by path prefix, mirroring
//! `CompositeFilesystem` (unified non-sandbox view blending local + remote stores).
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::{Error, Result};
use async_trait::async_trait;

use super::{FileData, FileInfo, Filesystem, GrepMatch};

/// Routes file operations by path prefix (longest prefix wins), falling back to a default store.
pub struct CompositeFilesystem {
    default_backend: Arc<dyn Filesystem>,
    routes: Vec<(String, Arc<dyn Filesystem>)>,
}

struct Route {
    backend: Arc<dyn Filesystem>,
    backend_path: String,
    prefix: Option<String>,
}

fn strip_leading_slash(s: &str) -> &str {
    s.trim_start_matches('/')
}

/// Canonical logical form of an agent path.
///
/// Agent paths are workspace-relative, so a leading `/` is a separator of the logical tree and
/// not the host root: `/memory/x`, `./memory/x` and `memory//x/` all name the same entry. The
/// empty string is the workspace root (#3378).
fn normalize_logical_path(path: &str) -> String {
    let unified = path.replace('\\', "/");
    let mut segments: Vec<&str> = Vec::new();
    for segment in unified.split('/') {
        if !segment.is_empty() && segment != "." {
            segments.push(segment);
        }
    }
    segments.join("/")
}

/// Whether `path` is spelled as a host path (`C:\x`, `\\server\share`) instead of a
/// workspace path.
///
/// A leading `/` is deliberately *not* treated as host-specific: agent paths are
/// workspace-relative, so `/knowledge/x` is the logical path `knowledge/x` (#2690).
fn is_host_spelled(path: &str) -> bool {
    let bytes = path.as_bytes();
    let drive_rooted = bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic();
    drive_rooted || path.starts_with("\\\\")
}
impl CompositeFilesystem {
    pub fn new(
        default_backend: Arc<dyn Filesystem>,
        routes: Vec<(String, Arc<dyn Filesystem>)>,
    ) -> Self {
        let mut routes = routes;
        routes.sort_by_key(|r| std::cmp::Reverse(r.0.len()));
        Self {
            default_backend,
            routes,
        }
    }

    fn route_for_path(&self, path: &str) -> Route {
        let logical = normalize_logical_path(path);
        if is_host_spelled(path) {
            // A host-spelled path is not a workspace path: it must not be captured by a route
            // whose prefix merely happens to appear in it (#2690).
            tracing::warn!(
                path = %path,
                "filesystem path is spelled as a host path; routing it to the default backend"
            );
            return Route {
                backend: self.default_backend.clone(),
                backend_path: path.to_string(),
                prefix: None,
            };
        }
        for (prefix, backend) in &self.routes {
            let canonical = strip_leading_slash(prefix);
            let prefix_no_slash = canonical.strip_suffix('/').unwrap_or(canonical);
            if logical == prefix_no_slash {
                let backend_path = if canonical.ends_with('/') {
                    "/".to_string()
                } else {
                    format!("/{logical}")
                };
                return Route {
                    backend: backend.clone(),
                    backend_path,
                    prefix: Some(prefix.clone()),
                };
            }
            let normalized = if canonical.ends_with('/') {
                canonical.to_string()
            } else {
                format!("{canonical}/")
            };
            if let Some(suffix) = logical.strip_prefix(&normalized) {
                let backend_path = if suffix.is_empty() {
                    "/".to_string()
                } else {
                    format!("/{suffix}")
                };
                return Route {
                    backend: backend.clone(),
                    backend_path,
                    prefix: Some(prefix.clone()),
                };
            }
        }
        Route {
            backend: self.default_backend.clone(),
            // The default backend always receives a workspace-relative logical path: a bare
            // "/" made an unrestricted local backend enumerate (and `grep`/`glob` walk) the
            // host filesystem root, and "." is how a local backend names its own root
            // (#3253/#770).
            backend_path: if logical.is_empty() {
                ".".to_string()
            } else {
                logical
            },
            prefix: None,
        }
    }

    fn remap(&self, prefix: &str, path: &str) -> String {
        if !prefix.ends_with('/') {
            return prefix.to_string();
        }
        let base = &prefix[..prefix.len() - 1];
        format!("{base}{path}")
    }
}

#[async_trait]
impl Filesystem for CompositeFilesystem {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn kind(&self) -> super::FilesystemKind {
        super::FilesystemKind::Composite
    }

    async fn ls(&self, ctx: &RuntimeContext, path: &str) -> Result<Vec<FileInfo>> {
        let route = self.route_for_path(path);
        if let Some(prefix) = &route.prefix {
            let entries = route.backend.ls(ctx, &route.backend_path).await?;
            return Ok(entries
                .into_iter()
                .map(|mut fi| {
                    fi.path = self.remap(prefix, &fi.path);
                    fi
                })
                .collect());
        }
        // A root-equivalent request (`/`, `.`, ``) is served by the default backend's own root
        // plus one entry per routed directory.
        if route.backend_path == "." {
            let mut out = self.default_backend.ls(ctx, ".").await.unwrap_or_default();
            for (prefix, _) in &self.routes {
                if prefix.ends_with('/') {
                    out.push(FileInfo::dir(prefix.clone()));
                }
            }
            out.sort_by_key(|f| f.path.clone());
            return Ok(out);
        }
        self.default_backend.ls(ctx, &route.backend_path).await
    }

    async fn read(
        &self,
        ctx: &RuntimeContext,
        path: &str,
        offset: usize,
        limit: usize,
    ) -> Result<FileData> {
        let route = self.route_for_path(path);
        route
            .backend
            .read(ctx, &route.backend_path, offset, limit)
            .await
    }

    async fn write(&self, ctx: &RuntimeContext, path: &str, content: &str) -> Result<()> {
        let route = self.route_for_path(path);
        route.backend.write(ctx, &route.backend_path, content).await
    }

    async fn edit(
        &self,
        ctx: &RuntimeContext,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<usize> {
        let route = self.route_for_path(path);
        route
            .backend
            .edit(ctx, &route.backend_path, old, new, replace_all)
            .await
    }

    async fn grep(
        &self,
        ctx: &RuntimeContext,
        pattern: &str,
        path: Option<&str>,
        glob: Option<&str>,
    ) -> Result<Vec<GrepMatch>> {
        let Some(p) = path else {
            return self.default_backend.grep(ctx, pattern, None, glob).await;
        };
        let route = self.route_for_path(p);
        if let Some(prefix) = &route.prefix {
            let matches = route
                .backend
                .grep(ctx, pattern, Some(&route.backend_path), glob)
                .await?;
            return Ok(matches
                .into_iter()
                .map(|m| GrepMatch {
                    path: self.remap(prefix, &m.path),
                    line: m.line,
                    text: m.text,
                })
                .collect());
        }
        // Non-routed paths search the anchored base (`.` for the root): forwarding the bare
        // "/" made an unrestricted backend walk the host filesystem (#3253).
        self.default_backend
            .grep(ctx, pattern, Some(&route.backend_path), glob)
            .await
    }

    async fn glob(&self, ctx: &RuntimeContext, pattern: &str, path: &str) -> Result<Vec<FileInfo>> {
        let route = self.route_for_path(path);
        if let Some(prefix) = &route.prefix {
            let entries = route
                .backend
                .glob(ctx, pattern, &route.backend_path)
                .await?;
            return Ok(entries
                .into_iter()
                .map(|mut fi| {
                    fi.path = self.remap(prefix, &fi.path);
                    fi
                })
                .collect());
        }
        self.default_backend
            .glob(ctx, pattern, &route.backend_path)
            .await
    }

    async fn delete(&self, ctx: &RuntimeContext, path: &str) -> Result<()> {
        let route = self.route_for_path(path);
        route.backend.delete(ctx, &route.backend_path).await
    }

    async fn move_path(&self, ctx: &RuntimeContext, from: &str, to: &str) -> Result<()> {
        let src = self.route_for_path(from);
        let dst = self.route_for_path(to);
        if Arc::ptr_eq(&src.backend, &dst.backend) {
            return src
                .backend
                .move_path(ctx, &src.backend_path, &dst.backend_path)
                .await;
        }
        let data = src
            .backend
            .read(ctx, &src.backend_path, 0, 0)
            .await
            .map_err(|error| {
                Error::Tool(format!(
                    "Cannot read source for cross-backend move: {from}: {error}"
                ))
            })?;
        // `Filesystem::write` carries text only, so a binary payload (which a backend hands back
        // base64-encoded in `content`) would be written as the base64 *text* and the bytes would
        // change (#3042).
        if data.encoding != "utf-8" {
            return Err(Error::Tool(format!(
                "Cannot move {from} across backends: the source is {}-encoded binary content, \
                 which the text-only write path cannot preserve. Copy it inside its own backend \
                 instead.",
                data.encoding
            )));
        }
        dst.backend
            .write(ctx, &dst.backend_path, &data.content)
            .await?;
        if let Err(error) = src.backend.delete(ctx, &src.backend_path).await {
            return Err(Error::Tool(format!(
                "Copied {from} to {to} but could not delete the source: {error}"
            )));
        }
        Ok(())
    }

    async fn exists(&self, ctx: &RuntimeContext, path: &str) -> Result<bool> {
        let route = self.route_for_path(path);
        route.backend.exists(ctx, &route.backend_path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::LocalFilesystem;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("fs_composite_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Workspace (default backend) + `memory/` route (its own directory), both sandboxed.
    fn composite(tag: &str) -> (CompositeFilesystem, std::path::PathBuf, std::path::PathBuf) {
        let root = temp_dir(tag);
        let workspace = root.join("workspace");
        let shared = root.join("shared");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        let fs = CompositeFilesystem::new(
            Arc::new(LocalFilesystem::sandboxed(&workspace)),
            vec![(
                "memory/".to_string(),
                Arc::new(LocalFilesystem::sandboxed(&shared)),
            )],
        );
        (fs, workspace, shared)
    }

    #[tokio::test]
    async fn relative_spellings_route_to_the_same_backend() {
        let (fs, _workspace, shared) = composite("spellings");
        let ctx = RuntimeContext::default();
        fs.write(&ctx, "./memory/notes.md", "note").await.unwrap();
        assert_eq!(
            std::fs::read_to_string(shared.join("notes.md")).unwrap(),
            "note",
            "a ./ spelling must reach the routed backend, not the default one"
        );
        for path in [
            "memory/notes.md",
            "/memory/notes.md",
            "memory//notes.md",
            "/./memory/notes.md",
        ] {
            assert_eq!(
                fs.read(&ctx, path, 0, 0).await.unwrap().content,
                "note",
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn listing_the_root_stays_inside_the_workspace() {
        let root = temp_dir("root_ls");
        let workspace = root.join("workspace");
        let shared = root.join("shared");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(workspace.join("a.txt"), "a").unwrap();
        let fs = CompositeFilesystem::new(
            // An unrestricted default backend used to turn `ls("/")` into a listing of the
            // host filesystem root (#3253/#770).
            Arc::new(LocalFilesystem::new(&workspace)),
            vec![(
                "memory/".to_string(),
                Arc::new(LocalFilesystem::sandboxed(&shared)),
            )],
        );
        let ctx = RuntimeContext::default();
        for path in ["/", ".", "", "./"] {
            let mut paths: Vec<String> = fs
                .ls(&ctx, path)
                .await
                .unwrap()
                .into_iter()
                .map(|entry| entry.path)
                .collect();
            paths.sort();
            assert_eq!(
                paths,
                vec!["/a.txt".to_string(), "memory/".to_string()],
                "{path}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_host_spelled_path_is_not_captured_by_a_route() {
        let (fs, _workspace, _shared) = composite("host_path");
        let ctx = RuntimeContext::default();
        fs.write(&ctx, "memory/notes.md", "note").await.unwrap();
        // A drive/UNC-spelled path stays on the default backend: the routed prefix inside it
        // must not redirect it to the shared store (#2690).
        let error = fs
            .read(&ctx, "C:/memory/notes.md", 0, 0)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("C:/memory/notes.md"), "got: {error}");
    }

    #[tokio::test]
    async fn a_cross_backend_move_keeps_text_and_refuses_binary() {
        let (fs, workspace, shared) = composite("move");
        let ctx = RuntimeContext::default();
        fs.write(&ctx, "note.md", "body").await.unwrap();
        fs.move_path(&ctx, "note.md", "memory/note.md")
            .await
            .unwrap();
        assert_eq!(
            fs.read(&ctx, "memory/note.md", 0, 0).await.unwrap().content,
            "body"
        );
        assert_eq!(
            std::fs::read_to_string(shared.join("note.md")).unwrap(),
            "body"
        );
        assert!(!fs.exists(&ctx, "note.md").await.unwrap());

        // A binary source cannot be carried by the text-only write path: writing the base64 text
        // it reads back would change the bytes (#3042).
        std::fs::write(
            workspace.join("image.png"),
            [0x89, b'P', b'N', b'G', 0x00, 0x01],
        )
        .unwrap();
        let error = fs
            .move_path(&ctx, "image.png", "memory/image.png")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("binary content"), "got: {error}");
        assert!(
            fs.exists(&ctx, "image.png").await.unwrap(),
            "a refused move must leave the source in place"
        );
        assert!(!fs.exists(&ctx, "memory/image.png").await.unwrap());
    }
}
