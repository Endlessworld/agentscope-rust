//! Routes file operations to different `Filesystem` stores by path prefix, mirroring
//! `CompositeFilesystem` (unified non-sandbox view blending local + remote stores).
use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::{Error, Result};

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
impl CompositeFilesystem {
    pub fn new(default_backend: Arc<dyn Filesystem>, routes: Vec<(String, Arc<dyn Filesystem>)>) -> Self {
        let mut routes = routes;
        routes.sort_by_key(|r| std::cmp::Reverse(r.0.len()));
        Self { default_backend, routes }
    }

    fn route_for_path(&self, path: &str) -> Route {
        let match_path = strip_leading_slash(path);
        for (prefix, backend) in &self.routes {
            let canonical = strip_leading_slash(prefix);
            let prefix_no_slash = canonical.strip_suffix('/').unwrap_or(canonical);
            if match_path == prefix_no_slash {
                let backend_path = if canonical.ends_with('/') {
                    "/".to_string()
                } else if path.starts_with('/') {
                    path.to_string()
                } else {
                    format!("/{path}")
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
            if match_path.starts_with(&normalized) {
                let suffix = &match_path[normalized.len()..];
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
            backend_path: path.to_string(),
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
        if path == "/" || path == "." {
            let mut out = self.default_backend.ls(ctx, "/").await.unwrap_or_default();
            for (prefix, _) in &self.routes {
                if prefix.ends_with('/') {
                    out.push(FileInfo::dir(prefix.clone()));
                }
            }
            out.sort_by_key(|f| f.path.clone());
            return Ok(out);
        }
        self.default_backend.ls(ctx, path).await
    }

    async fn read(
        &self,
        ctx: &RuntimeContext,
        path: &str,
        offset: usize,
        limit: usize,
    ) -> Result<FileData> {
        let route = self.route_for_path(path);
        route.backend.read(ctx, &route.backend_path, offset, limit).await
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
        let mut results = Vec::new();
        if let Some(p) = path {
            let route = self.route_for_path(p);
            if let Some(prefix) = &route.prefix {
                let matches = route.backend.grep(ctx, pattern, Some(&route.backend_path), glob).await?;
                return Ok(matches.into_iter().map(|m| GrepMatch {
                    path: self.remap(prefix, &m.path),
                    line: m.line,
                    text: m.text,
                }).collect());
            }
        }
        results.extend(self.default_backend.grep(ctx, pattern, path, glob).await?);
        Ok(results)
    }

    async fn glob(&self, ctx: &RuntimeContext, pattern: &str, path: &str) -> Result<Vec<FileInfo>> {
        let route = self.route_for_path(path);
        if let Some(prefix) = &route.prefix {
            let entries = route.backend.glob(ctx, pattern, &route.backend_path).await?;
            return Ok(entries
                .into_iter()
                .map(|mut fi| {
                    fi.path = self.remap(prefix, &fi.path);
                    fi
                })
                .collect());
        }
        self.default_backend.glob(ctx, pattern, path).await
    }

    async fn delete(&self, ctx: &RuntimeContext, path: &str) -> Result<()> {
        let route = self.route_for_path(path);
        route.backend.delete(ctx, &route.backend_path).await
    }

    async fn move_path(&self, ctx: &RuntimeContext, from: &str, to: &str) -> Result<()> {
        let src = self.route_for_path(from);
        let dst = self.route_for_path(to);
        if Arc::ptr_eq(&src.backend, &dst.backend) {
            return src.backend.move_path(ctx, &src.backend_path, &dst.backend_path).await;
        }
        let data = src.backend.read(ctx, &src.backend_path, 0, 0).await.map_err(|_| {
            Error::Tool(format!("Cannot read source for cross-backend move: {from}"))
        })?;
        dst.backend.write(ctx, &dst.backend_path, &data.content).await?;
        let _ = src.backend.delete(ctx, &src.backend_path).await;
        Ok(())
    }

    async fn exists(&self, ctx: &RuntimeContext, path: &str) -> Result<bool> {
        let route = self.route_for_path(path);
        route.backend.exists(ctx, &route.backend_path).await
    }
}
