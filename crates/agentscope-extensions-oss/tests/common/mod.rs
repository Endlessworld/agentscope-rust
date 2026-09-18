//! Shared test helpers.
#![allow(dead_code)]

use std::sync::Arc;

use agentscope_extensions_oss::{InMemoryOssClient, OssClient};

/// Creates a shared in-memory OSS client.
pub fn mem_client() -> Arc<InMemoryOssClient> {
    Arc::new(InMemoryOssClient::new())
}

/// Coerces an in-memory client into the `Arc<dyn OssClient>` trait object used by
/// the stores, keeping a concrete clone for assertions.
pub fn as_dyn(client: &Arc<InMemoryOssClient>) -> Arc<dyn OssClient> {
    client.clone()
}
