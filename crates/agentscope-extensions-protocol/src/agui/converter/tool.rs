//! Tool schema conversion, mirroring `AguiToolConverter`.

use agentscope_core::ToolSchema;

use crate::agui::model::AguiTool;

/// Bidirectional converter between [`AguiTool`] and core [`ToolSchema`].
#[derive(Debug, Clone, Copy, Default)]
pub struct AguiToolConverter;

impl AguiToolConverter {
    /// Creates a converter.
    pub fn new() -> Self {
        Self
    }

    /// Converts an AG-UI tool into a core [`ToolSchema`].
    pub fn to_tool_schema(&self, agui_tool: &AguiTool) -> ToolSchema {
        ToolSchema::new(agui_tool.name.clone(), agui_tool.description.clone())
            .parameters(agui_tool.parameters.clone())
    }

    /// Converts a core [`ToolSchema`] into an AG-UI tool.
    pub fn to_agui_tool(&self, schema: &ToolSchema) -> AguiTool {
        AguiTool::new(
            schema.name.clone(),
            schema.description.clone(),
            schema.parameters.clone(),
        )
    }

    /// Converts a list of AG-UI tools into core tool schemas.
    pub fn to_tool_schema_list(&self, agui_tools: &[AguiTool]) -> Vec<ToolSchema> {
        agui_tools.iter().map(|t| self.to_tool_schema(t)).collect()
    }

    /// Converts a list of core tool schemas into AG-UI tools.
    pub fn to_agui_tool_list(&self, schemas: &[ToolSchema]) -> Vec<AguiTool> {
        schemas.iter().map(|s| self.to_agui_tool(s)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parameters() -> serde_json::Map<String, serde_json::Value> {
        json!({
            "type": "object",
            "properties": {"q": {"type": "string"}},
            "required": ["q"]
        })
        .as_object()
        .cloned()
        .unwrap()
    }

    #[test]
    fn agui_tool_round_trips_through_tool_schema() {
        let converter = AguiToolConverter::new();
        let tool = AguiTool::new("search", "Search the web", parameters());
        let schema = converter.to_tool_schema(&tool);
        assert_eq!(schema.name, "search");
        assert_eq!(schema.description, "Search the web");
        assert_eq!(schema.parameters, parameters());
        assert_eq!(converter.to_agui_tool(&schema), tool);
    }

    #[test]
    fn list_conversions_preserve_order() {
        let converter = AguiToolConverter::new();
        let tools = vec![
            AguiTool::new("a", "first", serde_json::Map::new()),
            AguiTool::new("b", "second", parameters()),
        ];
        let schemas = converter.to_tool_schema_list(&tools);
        assert_eq!(schemas.len(), 2);
        assert_eq!(schemas[0].name, "a");
        assert_eq!(schemas[1].name, "b");
        assert!(converter.to_tool_schema_list(&[]).is_empty());
        assert_eq!(converter.to_agui_tool_list(&schemas), tools);
    }
}
