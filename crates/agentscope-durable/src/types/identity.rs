//! Identities, session/thread specifications and their read views.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::json;
use crate::Result;

/// `SessionKey` is the outer identity; the pairing matches the core `ThreadKey` layout.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionKey {
    pub namespace: String,
    pub session_id: String,
}

impl SessionKey {
    pub fn new(namespace: impl Into<String>, session_id: impl Into<String>) -> Result<Self> {
        let key = Self {
            namespace: namespace.into(),
            session_id: session_id.into(),
        };
        json::text(&key.namespace, "namespace")?;
        json::text(&key.session_id, "sessionId")?;
        Ok(key)
    }

    /// Opaque, filesystem-safe key derived from the identity components.
    pub fn storage_key(&self) -> String {
        format!(
            "session:{}:{}",
            json::hash(&Value::String(self.namespace.clone())),
            json::hash(&Value::String(self.session_id.clone()))
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadKey {
    pub session: SessionKey,
    pub thread_id: String,
}

impl ThreadKey {
    pub fn new(session: SessionKey, thread_id: impl Into<String>) -> Result<Self> {
        let thread_id = thread_id.into();
        json::text(&thread_id, "threadId")?;
        Ok(Self { session, thread_id })
    }

    pub fn storage_key(&self) -> String {
        format!(
            "{}:thread:{}",
            self.session.storage_key(),
            json::hash(&Value::String(self.thread_id.clone()))
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Lifecycle {
    Open,
    Closed,
}

/// Frozen harness identity: a checkpoint is only restorable under the same binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadBinding {
    pub harness: String,
    pub definition_version: String,
    pub state_version: String,
}

impl ThreadBinding {
    pub fn new(
        harness: impl Into<String>,
        definition_version: impl Into<String>,
        state_version: impl Into<String>,
    ) -> Result<Self> {
        let binding = Self {
            harness: harness.into(),
            definition_version: definition_version.into(),
            state_version: state_version.into(),
        };
        json::text(&binding.harness, "harness")?;
        json::text(&binding.definition_version, "definitionVersion")?;
        json::text(&binding.state_version, "stateVersion")?;
        Ok(binding)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSpec {
    pub key: SessionKey,
    pub primary_binding: ThreadBinding,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub key: SessionKey,
    pub primary_thread_id: String,
    pub status: Lifecycle,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSpec {
    pub key: ThreadKey,
    pub binding: ThreadBinding,
}

impl ThreadSpec {
    pub fn harness(&self) -> &str {
        &self.binding.harness
    }

    pub fn definition_version(&self) -> &str {
        &self.binding.definition_version
    }

    pub fn state_version(&self) -> &str {
        &self.binding.state_version
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadView {
    pub definition: ThreadSpec,
    pub status: Lifecycle,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPage {
    pub items: Vec<SessionView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadPage {
    pub items: Vec<ThreadView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// Ready-work selection: an exact definition/state pair, or every definition of a harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessSelector {
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_version: Option<String>,
}

impl HarnessSelector {
    pub fn all(harness: impl Into<String>) -> Result<Self> {
        let selector = Self {
            harness: harness.into(),
            definition_version: None,
            state_version: None,
        };
        json::text(&selector.harness, "harness")?;
        Ok(selector)
    }

    pub fn exact(definition: &ThreadSpec) -> Result<Self> {
        let selector = Self {
            harness: definition.harness().to_string(),
            definition_version: Some(definition.definition_version().to_string()),
            state_version: Some(definition.state_version().to_string()),
        };
        json::text(&selector.harness, "harness")?;
        Ok(selector)
    }

    pub fn validate(&self) -> Result<()> {
        json::text(&self.harness, "harness")?;
        if self.definition_version.is_some() != self.state_version.is_some() {
            return Err(crate::DurableError::invalid(
                "Select both definition and state versions, or neither",
            ));
        }
        if let Some(version) = &self.definition_version {
            json::text(version, "definitionVersion")?;
        }
        if let Some(version) = &self.state_version {
            json::text(version, "stateVersion")?;
        }
        Ok(())
    }
}
