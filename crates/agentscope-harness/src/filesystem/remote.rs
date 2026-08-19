//! Remote (key-value store backed) filesystem, mirroring `RemoteFilesystem`.
//!
//! The store integration (Redis/OSS/MySQL) is pending; this stub keeps the routing
//! surface (`RemoteFilesystemSpec`/`IsolationScope`) available without a live backend.
use async_trait::async_trait;
use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::{Error, Result};

use super::{FileData, FileInfo, Filesystem, GrepMatch};

/// Placeholder for a store-backed filesystem. All operations return a "not implemented" error
/// until the remote store backend is wired in (see Phase 5 / MCP / extensions).
#[derive(Debug, Clone, Default)]
pub struct RemoteFilesystem;

#[async_trait]
impl Filesystem for RemoteFilesystem {
    async fn ls(&self, _ctx: &RuntimeContext, _path: &str) -> Result<Vec<FileInfo>> {
        Err(Error::Tool("RemoteFilesystem not implemented".into()))
    }
    async fn read(&self, _ctx: &RuntimeContext, _p: &str, _o: usize, _l: usize) -> Result<FileData> {
        Err(Error::Tool("RemoteFilesystem not implemented".into()))
    }
    async fn write(&self, _ctx: &RuntimeContext, _p: &str, _c: &str) -> Result<()> {
        Err(Error::Tool("RemoteFilesystem not implemented".into()))
    }
    async fn edit(&self, _ctx: &RuntimeContext, _p: &str, _o: &str, _n: &str, _r: bool) -> Result<usize> {
        Err(Error::Tool("RemoteFilesystem not implemented".into()))
    }
    async fn grep(&self, _ctx: &RuntimeContext, _p: &str, _pa: Option<&str>, _g: Option<&str>) -> Result<Vec<GrepMatch>> {
        Err(Error::Tool("RemoteFilesystem not implemented".into()))
    }
    async fn glob(&self, _ctx: &RuntimeContext, _p: &str, _pa: &str) -> Result<Vec<FileInfo>> {
        Err(Error::Tool("RemoteFilesystem not implemented".into()))
    }
    async fn delete(&self, _ctx: &RuntimeContext, _p: &str) -> Result<()> {
        Err(Error::Tool("RemoteFilesystem not implemented".into()))
    }
    async fn move_path(&self, _ctx: &RuntimeContext, _f: &str, _t: &str) -> Result<()> {
        Err(Error::Tool("RemoteFilesystem not implemented".into()))
    }
    async fn exists(&self, _ctx: &RuntimeContext, _p: &str) -> Result<bool> {
        Err(Error::Tool("RemoteFilesystem not implemented".into()))
    }
}
