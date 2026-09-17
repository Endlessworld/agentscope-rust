//! Filesystem wrapper that substitutes a fixed [`RuntimeContext`] on every
//! delegated call, ignoring whatever context the caller supplies.
//!
//! Mirrors `io.agentscope.harness.agent.filesystem.BakedContextFilesystem`. Used by
//! `HarnessAgent::workspace_for(user_id, session_id)` to construct an out-of-band
//! workspace view bound to an explicit user identity: controllers acting on another
//! user's namespace can pass an empty context downstream, and this wrapper ensures the
//! namespace factory underneath still receives the baked-in identity.

use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;

use super::{FileData, FileInfo, Filesystem, GrepMatch};

/// Filesystem that delegates to another filesystem with a baked-in [`RuntimeContext`].
pub struct BakedContextFilesystem {
    delegate: Arc<dyn Filesystem>,
    baked_rc: RuntimeContext,
}

impl BakedContextFilesystem {
    /// Creates a wrapper around `delegate`, always using `baked_rc`.
    pub fn new(delegate: Arc<dyn Filesystem>, baked_rc: RuntimeContext) -> Self {
        Self { delegate, baked_rc }
    }

    /// Returns the baked-in runtime context.
    pub fn baked_context(&self) -> &RuntimeContext {
        &self.baked_rc
    }

    /// Returns the wrapped filesystem.
    pub fn delegate(&self) -> &Arc<dyn Filesystem> {
        &self.delegate
    }
}

impl std::fmt::Debug for BakedContextFilesystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BakedContextFilesystem")
            .field("baked_rc", &self.baked_rc)
            .finish()
    }
}

#[async_trait]
impl Filesystem for BakedContextFilesystem {
    async fn ls(&self, _ctx: &RuntimeContext, path: &str) -> Result<Vec<FileInfo>> {
        self.delegate.ls(&self.baked_rc, path).await
    }

    async fn read(
        &self,
        _ctx: &RuntimeContext,
        path: &str,
        offset: usize,
        limit: usize,
    ) -> Result<FileData> {
        self.delegate.read(&self.baked_rc, path, offset, limit).await
    }

    async fn write(&self, _ctx: &RuntimeContext, path: &str, content: &str) -> Result<()> {
        self.delegate.write(&self.baked_rc, path, content).await
    }

    async fn edit(
        &self,
        _ctx: &RuntimeContext,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<usize> {
        self.delegate
            .edit(&self.baked_rc, path, old, new, replace_all)
            .await
    }

    async fn grep(
        &self,
        _ctx: &RuntimeContext,
        pattern: &str,
        path: Option<&str>,
        glob: Option<&str>,
    ) -> Result<Vec<GrepMatch>> {
        self.delegate.grep(&self.baked_rc, pattern, path, glob).await
    }

    async fn glob(&self, _ctx: &RuntimeContext, pattern: &str, path: &str) -> Result<Vec<FileInfo>> {
        self.delegate.glob(&self.baked_rc, pattern, path).await
    }

    async fn delete(&self, _ctx: &RuntimeContext, path: &str) -> Result<()> {
        self.delegate.delete(&self.baked_rc, path).await
    }

    async fn move_path(&self, _ctx: &RuntimeContext, from: &str, to: &str) -> Result<()> {
        self.delegate.move_path(&self.baked_rc, from, to).await
    }

    async fn exists(&self, _ctx: &RuntimeContext, path: &str) -> Result<bool> {
        self.delegate.exists(&self.baked_rc, path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::remote::store::{BaseStore, InMemoryStore, NamespaceFactory};
    use crate::filesystem::RemoteFilesystem;

    #[derive(Debug)]
    struct UserNs;
    impl NamespaceFactory for UserNs {
        fn get_namespace(&self, ctx: &RuntimeContext) -> Vec<String> {
            vec![
                "users".to_string(),
                ctx.user_id().unwrap_or("anon").to_string(),
                "files".to_string(),
            ]
        }
    }

    #[tokio::test]
    async fn baked_context_overrides_caller_context() {
        let store: Arc<dyn BaseStore> = Arc::new(InMemoryStore::new());
        let remote = Arc::new(RemoteFilesystem::new(store.clone(), Arc::new(UserNs)));
        let baked = BakedContextFilesystem::new(
            remote.clone(),
            RuntimeContext::new().with_user_id("alice"),
        );

        // Caller passes an empty context; the write must land in alice's namespace.
        let empty = RuntimeContext::new();
        baked.write(&empty, "/a.md", "alice-data").await.unwrap();

        let alice = RuntimeContext::new().with_user_id("alice");
        let bob = RuntimeContext::new().with_user_id("bob");
        assert_eq!(
            remote.read(&alice, "/a.md", 0, 0).await.unwrap().content,
            "alice-data"
        );
        assert!(remote.read(&bob, "/a.md", 0, 0).await.is_err());
    }

    #[tokio::test]
    async fn baked_context_delegates_all_operations() {
        let store: Arc<dyn BaseStore> = Arc::new(InMemoryStore::new());
        let remote = Arc::new(RemoteFilesystem::new(store, Arc::new(UserNs)));
        let baked = BakedContextFilesystem::new(
            remote,
            RuntimeContext::new().with_user_id("alice"),
        );
        let empty = RuntimeContext::new();
        baked.write(&empty, "/notes/a.md", "one").await.unwrap();
        assert!(baked.exists(&empty, "/notes/a.md").await.unwrap());
        assert_eq!(baked.ls(&empty, "/notes").await.unwrap().len(), 1);
        assert_eq!(baked.read(&empty, "/notes/a.md", 0, 0).await.unwrap().content, "one");
        baked.edit(&empty, "/notes/a.md", "one", "two", false).await.unwrap();
        assert_eq!(baked.read(&empty, "/notes/a.md", 0, 0).await.unwrap().content, "two");
        baked.move_path(&empty, "/notes/a.md", "/notes/b.md").await.unwrap();
        assert!(!baked.exists(&empty, "/notes/a.md").await.unwrap());
        assert!(baked.exists(&empty, "/notes/b.md").await.unwrap());
        baked.delete(&empty, "/notes").await.unwrap();
        assert!(!baked.exists(&empty, "/notes/b.md").await.unwrap());
    }
}
