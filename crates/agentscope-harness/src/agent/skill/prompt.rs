//! Renders the harness `<available_skills>` system-prompt block from a
//! `SkillCatalog`, mirroring `SkillPromptBuilder`.
//!
//! Each `<skill>` optionally carries a `<files-root>` child giving the absolute
//! path to that skill's files. The `<code_execution>` section is emitted only
//! when at least one visible skill has a non-null files root (i.e. shell is
//! available and at least one skill's files are reachable).
use serde_json::Value;

use super::model::{HarnessSkillEntry, Metadata, SkillCatalog, SkillFilter};

const INDENT: &str = "  ";

/// Default header rendered once before the first skill.
pub const DEFAULT_HEADER: &str = r#"## Available Skills

<usage>
Skills provide specialized capabilities and domain knowledge. Use them when they match your current task.

How to use skills:
- Load skill: load_skill_through_path(skillId="<skill-id>", path="SKILL.md")
- The skill will be activated and its documentation loaded with detailed instructions
- Additional resources (scripts, assets, references) can be loaded with the same tool and other paths

Metadata is rendered as XML under each <skill> element:
- scalar metadata becomes a simple child element
- nested maps become nested XML elements
- lists become repeated <item> elements
- <skill-id> is always appended for tool loading
- <files-root>, when present, gives the absolute path for shell-executing this skill's scripts
</usage>

<available_skills>
"#;

/// Default code-execution instruction appended only when shell is reachable.
pub const DEFAULT_CODE_EXECUTION_INSTRUCTION: &str = r#"

## Code Execution

<code_execution>
You have access to the execute_shell_command tool. Each skill in <available_skills>
includes a <files-root> element giving the absolute path to that skill's files.

Workflow:
1. After loading a skill, look at its <files-root> in <available_skills>
2. List its files:    ls <files-root>/
3. Run scripts:       python3 <files-root>/scripts/<script-name>
4. Always use absolute paths derived from <files-root>; never invent paths
5. If a script exists for the task, run it directly — do not rewrite its logic inline
</code_execution>
"#;

/// Renders the available-skills prompt block.
#[derive(Debug, Clone)]
pub struct SkillPromptBuilder {
    header: String,
    code_execution_instruction: String,
    expose_all_metadata: bool,
}

impl Default for SkillPromptBuilder {
    fn default() -> Self {
        Self {
            header: DEFAULT_HEADER.to_string(),
            code_execution_instruction: DEFAULT_CODE_EXECUTION_INSTRUCTION.to_string(),
            expose_all_metadata: true,
        }
    }
}

impl SkillPromptBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_parts(header: Option<&str>, code_instruction: Option<&str>, expose_all: bool) -> Self {
        Self {
            header: header
                .filter(|h| !h.trim().is_empty())
                .unwrap_or(DEFAULT_HEADER)
                .to_string(),
            code_execution_instruction: code_instruction
                .filter(|h| !h.trim().is_empty())
                .unwrap_or(DEFAULT_CODE_EXECUTION_INSTRUCTION)
                .to_string(),
            expose_all_metadata: expose_all,
        }
    }

    /// Render the prompt block. Returns an empty string when no skills pass the
    /// filter, so the caller can no-op concatenation.
    pub fn render(&self, catalog: &SkillCatalog, filter: &SkillFilter) -> String {
        if catalog.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        let mut any = false;
        let mut any_with_files_root = false;

        for entry in catalog.all() {
            let skill_name = entry.skill().name();
            if !filter.is_allowed(skill_name) {
                continue;
            }
            if !any {
                out.push_str(&self.header);
                any = true;
            }
            self.append_skill(&mut out, entry);
            if let Some(root) = entry.files_root() {
                if !root.trim().is_empty() {
                    any_with_files_root = true;
                }
            }
        }

        if !any {
            return String::new();
        }
        out.push_str("</available_skills>");
        if any_with_files_root {
            out.push_str(&self.code_execution_instruction);
        }
        out
    }

    pub fn render_all(&self, catalog: &SkillCatalog) -> String {
        self.render(catalog, &SkillFilter::all())
    }

    fn append_skill(&self, out: &mut String, entry: &HarnessSkillEntry) {
        let skill = entry.skill();
        out.push_str("<skill>\n");
        for (key, value) in self.metadata_view(skill.metadata()) {
            self.append_xml_node(out, &key, value, 1);
        }
        self.append_xml_node(out, "skill-id", &Value::String(skill.skill_id()), 1);
        if let Some(root) = entry.files_root() {
            if !root.trim().is_empty() {
                self.append_xml_node(out, "files-root", &Value::String(root.to_string()), 1);
            }
        }
        out.push_str("</skill>\n\n");
    }

    fn metadata_view<'a>(&self, metadata: &'a Metadata) -> Vec<(String, &'a Value)> {
        let mut view: Vec<(String, &Value)> = Vec::new();
        if self.expose_all_metadata {
            for (k, v) in metadata {
                view.push((k.clone(), v));
            }
        } else {
            for key in ["name", "description"] {
                if let Some(v) = metadata.get(key) {
                    view.push((key.to_string(), v));
                }
            }
        }
        view
    }

    fn append_xml_node(&self, out: &mut String, key: &str, value: &Value, indent_level: usize) {
        if value.is_null() {
            return;
        }
        let indent = INDENT.repeat(indent_level);
        let valid_tag = is_valid_xml_tag_name(key);
        let (open_tag, close_tag) = if valid_tag {
            (format!("<{key}>"), format!("</{key}>"))
        } else {
            (format!("<entry key=\"{}\">", escape_xml(key)), "</entry>".to_string())
        };

        if is_scalar_value(value) {
            out.push_str(&indent);
            out.push_str(&open_tag);
            out.push_str(&escape_xml(&scalar_text(value)));
            out.push_str(&close_tag);
            out.push('\n');
            return;
        }

        out.push_str(&indent);
        out.push_str(&open_tag);
        out.push('\n');
        match value {
            Value::Object(map) => {
                for (k, v) in map {
                    self.append_xml_node(out, k, v, indent_level + 1);
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.append_xml_node(out, "item", item, indent_level + 1);
                }
            }
            other => {
                out.push_str(&INDENT.repeat(indent_level + 1));
                out.push_str(&escape_xml(&scalar_text(other)));
                out.push('\n');
            }
        }
        out.push_str(&indent);
        out.push_str(&close_tag);
        out.push('\n');
    }
}

fn is_scalar_value(value: &Value) -> bool {
    !matches!(value, Value::Object(_) | Value::Array(_))
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn is_valid_xml_tag_name(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::skill::model::{AgentSkill, HarnessSkillEntry};
    use serde_json::json;

    fn skill(name: &str) -> AgentSkill {
        AgentSkill::builder()
            .name(name)
            .source("workspace")
            .description(format!("desc {name}"))
            .build()
    }

    fn catalog(entries: Vec<HarnessSkillEntry>) -> SkillCatalog {
        SkillCatalog::of(entries)
    }

    #[test]
    fn empty_catalog_renders_empty() {
        let b = SkillPromptBuilder::new();
        assert_eq!(b.render_all(&SkillCatalog::empty()), "");
    }

    #[test]
    fn renders_skill_id_and_description() {
        let b = SkillPromptBuilder::new();
        let cat = catalog(vec![HarnessSkillEntry::new(skill("data"))]);
        let out = b.render_all(&cat);
        assert!(out.contains("<available_skills>"));
        assert!(out.contains("<skill-id>data_workspace</skill-id>"));
        assert!(out.contains("</available_skills>"));
        // The default header mentions <files-root> in prose, so check no rendered
        // element (a path value starting with '/') and no code-execution section.
        assert!(!out.contains("<files-root>/"));
        assert!(!out.contains("code_execution"));
    }

    #[test]
    fn files_root_enables_code_execution_section() {
        let b = SkillPromptBuilder::new();
        let entry = HarnessSkillEntry::new(skill("data")).with_files_root("/workspace/skills/data");
        let out = b.render_all(&catalog(vec![entry]));
        assert!(out.contains("<files-root>/workspace/skills/data</files-root>"));
        assert!(out.contains("<code_execution>"));
    }

    #[test]
    fn filter_excludes_hidden_skill() {
        let b = SkillPromptBuilder::new();
        let cat = catalog(vec![
            HarnessSkillEntry::new(skill("a")),
            HarnessSkillEntry::new(skill("secret")),
        ]);
        let out = b.render(&cat, &SkillFilter::only(&["a"]));
        assert!(out.contains("a_workspace"));
        assert!(!out.contains("secret_workspace"));
    }

    #[test]
    fn nested_metadata_renders_as_xml() {
        let b = SkillPromptBuilder::new();
        let mut metadata = serde_json::Map::new();
        metadata.insert("author".into(), json!("alice"));
        metadata.insert("tags".into(), json!(["a", "b"]));
        metadata.insert("nested".into(), json!({"x": 1}));
        let s = AgentSkill::builder()
            .name("m")
            .source("ws")
            .description("d")
            .metadata(metadata)
            .build();
        let out = b.render_all(&catalog(vec![HarnessSkillEntry::new(s)]));
        assert!(out.contains("<author>alice</author>"));
        assert!(out.contains("<item>a</item>"));
        assert!(out.contains("<nested>"));
    }

    #[test]
    fn invalid_tag_keys_use_entry_wrapper() {
        let b = SkillPromptBuilder::new();
        let mut metadata = serde_json::Map::new();
        metadata.insert("not valid key!".into(), json!(1));
        let s = AgentSkill::builder()
            .name("x")
            .source("ws")
            .description("d")
            .metadata(metadata)
            .build();
        let out = b.render_all(&catalog(vec![HarnessSkillEntry::new(s)]));
        assert!(out.contains("<entry key=\"not valid key!\">"));
    }
}
