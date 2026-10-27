//! Layered filesystem overlaying a writable "upper" store on a shared "lower" store,
//! mirroring `OverlayFilesystem` (copy-on-write semantics).
use std::collections::BTreeMap;
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::{Error, Result};
use async_trait::async_trait;

use super::{FileData, FileInfo, Filesystem, GrepMatch};

/// A copy-on-write overlay of two filesystems: reads check the upper layer first and fall back to
/// the lower; writes always target the upper layer.
pub struct OverlayFilesystem {
    upper: Arc<dyn Filesystem>,
    lower: Arc<dyn Filesystem>,
}
impl OverlayFilesystem {
    pub fn new(upper: Arc<dyn Filesystem>, lower: Arc<dyn Filesystem>) -> Self {
        Self { upper, lower }
    }

    pub fn upper(&self) -> &dyn Filesystem {
        self.upper.as_ref()
    }

    pub fn lower(&self) -> &dyn Filesystem {
        self.lower.as_ref()
    }
}

#[async_trait]
impl Filesystem for OverlayFilesystem {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn kind(&self) -> super::FilesystemKind {
        super::FilesystemKind::Overlay
    }

    async fn ls(&self, ctx: &RuntimeContext, path: &str) -> Result<Vec<FileInfo>> {
        let upper = self.upper.ls(ctx, path).await.unwrap_or_default();
        let lower = self.lower.ls(ctx, path).await.unwrap_or_default();
        let mut merged: BTreeMap<String, FileInfo> = BTreeMap::new();
        for fi in lower {
            merged.insert(fi.path.clone(), fi);
        }
        for fi in upper {
            merged.insert(fi.path.clone(), fi);
        }
        Ok(merged.into_values().collect())
    }

    async fn read(
        &self,
        ctx: &RuntimeContext,
        path: &str,
        offset: usize,
        limit: usize,
    ) -> Result<FileData> {
        if self.upper.exists(ctx, path).await.unwrap_or(false) {
            self.upper.read(ctx, path, offset, limit).await
        } else {
            self.lower.read(ctx, path, offset, limit).await
        }
    }

    async fn write(&self, ctx: &RuntimeContext, path: &str, content: &str) -> Result<()> {
        self.upper.write(ctx, path, content).await
    }

    async fn edit(
        &self,
        ctx: &RuntimeContext,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<usize> {
        if self.upper.exists(ctx, path).await.unwrap_or(false) {
            return self.upper.edit(ctx, path, old, new, replace_all).await;
        }
        if self.lower.exists(ctx, path).await.unwrap_or(false) {
            // copy-on-write: copy lower content into upper, then edit there
            let src = self.lower.read(ctx, path, 0, 0).await?;
            self.upper.write(ctx, path, &src.content).await?;
            return self.upper.edit(ctx, path, old, new, replace_all).await;
        }
        Err(Error::Tool(format!("File not found: {path}")))
    }

    async fn grep(
        &self,
        ctx: &RuntimeContext,
        pattern: &str,
        path: Option<&str>,
        glob: Option<&str>,
    ) -> Result<Vec<GrepMatch>> {
        let upper = self
            .upper
            .grep(ctx, pattern, path, glob)
            .await
            .unwrap_or_default();
        let lower = self
            .lower
            .grep(ctx, pattern, path, glob)
            .await
            .unwrap_or_default();
        let mut merged: BTreeMap<String, GrepMatch> = BTreeMap::new();
        for m in lower {
            merged.insert(format!("{}:{}", m.path, m.line), m);
        }
        for m in upper {
            merged.insert(format!("{}:{}", m.path, m.line), m);
        }
        Ok(merged.into_values().collect())
    }

    async fn glob(&self, ctx: &RuntimeContext, pattern: &str, path: &str) -> Result<Vec<FileInfo>> {
        let upper = self
            .upper
            .glob(ctx, pattern, path)
            .await
            .unwrap_or_default();
        let lower = self
            .lower
            .glob(ctx, pattern, path)
            .await
            .unwrap_or_default();
        let mut merged: BTreeMap<String, FileInfo> = BTreeMap::new();
        for fi in lower {
            merged.insert(fi.path.clone(), fi);
        }
        for fi in upper {
            merged.insert(fi.path.clone(), fi);
        }
        Ok(merged.into_values().collect())
    }

    async fn delete(&self, ctx: &RuntimeContext, path: &str) -> Result<()> {
        if self.upper.exists(ctx, path).await.unwrap_or(false) {
            self.upper.delete(ctx, path).await
        } else if self.lower.exists(ctx, path).await.unwrap_or(false) {
            Err(Error::Tool(format!("Cannot delete shared file: {path}")))
        } else {
            Ok(())
        }
    }

    async fn move_path(&self, ctx: &RuntimeContext, from: &str, to: &str) -> Result<()> {
        if self.upper.exists(ctx, from).await.unwrap_or(false) {
            return self.upper.move_path(ctx, from, to).await;
        }
        if self.lower.exists(ctx, from).await.unwrap_or(false) {
            let src = self.lower.read(ctx, from, 0, 0).await?;
            return self.upper.write(ctx, to, &src.content).await;
        }
        Err(Error::Tool(format!("Source not found: {from}")))
    }

    async fn exists(&self, ctx: &RuntimeContext, path: &str) -> Result<bool> {
        Ok(self.upper.exists(ctx, path).await.unwrap_or(false)
            || self.lower.exists(ctx, path).await.unwrap_or(false))
    }
}
