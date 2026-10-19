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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defer_loading_follows_the_schema() {
        // Unspecified schema → eagerly loaded.
        let eager = SchemaOnlyTool::new(ToolSchema::new("t", "d"));
        assert!(!Tool::defer_loading(&eager));
        assert_eq!(eager.schema().defer_loading, None);

        // Explicitly deferred schema → reported and preserved by `schema()`.
        let deferred =
            SchemaOnlyTool::new(ToolSchema::new("t", "d").defer_loading(Some(true)));
        assert!(Tool::defer_loading(&deferred));
        assert_eq!(deferred.schema().defer_loading, Some(true));

        // Explicit `false` (Java's `Boolean.FALSE`) also stays false.
        let explicit_false =
            SchemaOnlyTool::new(ToolSchema::new("t", "d").defer_loading(Some(false)));
        assert!(!Tool::defer_loading(&explicit_false));
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
    /// Mirrors Java `SchemaOnlyTool.getDeferLoading()`: the flag travels with the
    /// schema the tool was built from.
    fn defer_loading(&self) -> bool {
        self.schema.defer_loading.unwrap_or(false)
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
