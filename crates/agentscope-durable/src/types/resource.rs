//! Session-scoped, versioned private resources.
//!
//! Requests and their results commit under the same writer lease as the turn, so a
//! resource mutation is never visible without the state change that produced it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::json;
use crate::{DurableError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ResourceOperation {
    Get,
    Search,
    Put,
    Cas,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceRequest {
    pub request_id: String,
    pub operation: ResourceOperation,
    #[serde(default)]
    pub namespace: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default)]
    pub value: Value,
    #[serde(default)]
    pub expected_version: u64,
    #[serde(default)]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
}

impl ResourceRequest {
    pub fn validate(&self) -> Result<()> {
        json::text(&self.request_id, "requestId")?;
        if self.namespace.len() > 32 || self.limit > 1000 || self.offset > 1_000_000 {
            return Err(DurableError::invalid("Invalid resource request"));
        }
        for part in &self.namespace {
            json::text(part, "namespace")?;
        }
        if self.operation != ResourceOperation::Search {
            json::text(self.key.as_deref().unwrap_or_default(), "resource key")?;
        }
        if matches!(
            self.operation,
            ResourceOperation::Put | ResourceOperation::Cas
        ) && !self.value.is_object()
        {
            return Err(DurableError::invalid("Resource value must be an object"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceValue {
    #[serde(default)]
    pub namespace: Vec<String>,
    pub key: String,
    pub value: Value,
    pub version: u64,
}

impl ResourceValue {
    pub fn validate(&self) -> Result<()> {
        if self.namespace.len() > 32 || !self.value.is_object() || self.version < 1 {
            return Err(DurableError::invalid("Invalid resource value"));
        }
        for part in &self.namespace {
            json::text(part, "namespace")?;
        }
        json::text(&self.key, "resource key")?;
        Ok(())
    }
}
