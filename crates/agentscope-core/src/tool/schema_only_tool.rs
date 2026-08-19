//! A tool defined only by its schema (executed outside the framework).
use async_trait::async_trait;

use crate::error::{Error, Result};
use crate::message::{JsonMap, ToolResultBlock};
use crate::model::ToolSchema;

use super::tool::Tool;

/// A tool defined only by its schema. Invocation is not supported (external).
#[derive(Debug, Clone)]
pub struct SchemaOnlyTool {
    schema: ToolSchema,
}

impl SchemaOnlyTool {
    /// Creates a new schema-only tool from the given schema.
    pub fn new(schema: ToolSchema) -> Self {
        Self { schema }
    }
}

#[async_trait]
impl Tool for SchemaOnlyTool {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn description(&self) -> &str {
        &self.schema.description
    }
    fn is_external(&self) -> bool {
        true
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(&self.schema.parameters)
            .unwrap_or(serde_json::Value::Object(Default::default()))
    }
    async fn call(&self, _args: JsonMap, _ctx: super::tool::ToolExecutionContext) -> Result<ToolResultBlock> {
        Err(Error::Tool(format!(
            "Tool '{}' is external and cannot be executed by the framework",
            self.name()
        )))
    }
}
