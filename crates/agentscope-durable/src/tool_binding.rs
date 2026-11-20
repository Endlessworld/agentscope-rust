//! Which executor owns a tool, and the digest of the catalog entry that said so.
//!
//! A host publishes its tools through a [`ToolCatalog`]; the adapter turns a native tool call
//! into a durable [`ToolCall`](crate::ToolCall) using the binding the catalog reports. A call
//! whose tool is not in the catalog keeps the historical `"local"` executor, so existing hosts
//! behave exactly as before.

use std::collections::BTreeMap;

/// The durable identity a catalog assigns to one tool.
///
/// `digest` is the 64-hex fingerprint of the entry's definition; the backend rejects a call
/// whose binding does not match, so a tool that changed between dispatch and query is caught.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolBinding {
    pub executor: String,
    pub digest: String,
}

impl ToolBinding {
    pub fn new(executor: impl Into<String>, digest: impl Into<String>) -> Self {
        Self {
            executor: executor.into(),
            digest: digest.into(),
        }
    }
}

/// The tools a host exposes, by name. Implementations are read-mostly and shared across ticks.
pub trait ToolCatalog: Send + Sync {
    fn binding(&self, tool: &str) -> Option<ToolBinding>;
}

/// A catalog built from a fixed map; the shape tests and small hosts use.
#[derive(Debug, Clone, Default)]
pub struct MapCatalog {
    entries: BTreeMap<String, ToolBinding>,
}

impl MapCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, tool: impl Into<String>, binding: ToolBinding) -> Self {
        self.entries.insert(tool.into(), binding);
        self
    }
}

impl ToolCatalog for MapCatalog {
    fn binding(&self, tool: &str) -> Option<ToolBinding> {
        self.entries.get(tool).cloned()
    }
}
