//! LLM-driven subagent spec generation, mirroring
//! `io.agentscope.harness.agent.subagent.SubagentSpecGenerator`.
//!
//! The generator prompts a model to emit a Markdown document whose YAML front matter follows
//! the [`AgentSpecLoader`](super::loader) schema, then round-trips it through
//! [`parse`](super::loader::parse) so a malformed answer fails before anything is written.
use std::sync::Arc;

use agentscope_core::message::Msg;
use agentscope_core::model::Model;

use super::loader::parse;
use super::SubagentDeclaration;
use crate::agent::context::auxiliary_text;

/// Prompt used for every generation call (verbatim from the Java generator).
const PROMPT_TEMPLATE: &str = "\
You are designing a subagent specification for the agentscope framework.

User description of the agent's purpose:
---
{}
---

Existing agent ids (DO NOT reuse, even with different casing): {}

Produce a Markdown document with YAML frontmatter that matches this schema:

---
description: <one sentence describing when the orchestrator should delegate to this agent>
mode: subagent
hidden: false
# Optional model hyperparameters (omit to inherit parent):
# temperature: <0.0..2.0>
# top_p: <0.0..1.0>
# steps: <positive integer, default 10>
# tools: [<inherited tool name>, ...]   # optional allowlist; empty = inherit all
---

<System-prompt body in Markdown: describe the agent's role, capabilities, output
 format, and constraints. Keep the body focused — this becomes the subagent's sysPrompt.>

Output rules:
- Return ONLY the markdown document. No prose, no code fences, no commentary.
- Do NOT include a `name:` field — the caller derives the name from the filename.
- The description must be at most 200 characters.
- The body must not be empty when no workspace.path is specified.";

/// The raw markdown and the declaration it parsed into.
#[derive(Debug, Clone)]
pub struct GeneratedSpec {
    pub markdown: String,
    pub declaration: SubagentDeclaration,
}

/// LLM-driven generator for [`SubagentDeclaration`] markdown specs.
pub struct SubagentSpecGenerator {
    model: Arc<dyn Model>,
}

impl SubagentSpecGenerator {
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self { model }
    }

    pub fn model(&self) -> &Arc<dyn Model> {
        &self.model
    }

    /// Asks the model for a spec markdown document and returns it trimmed.
    ///
    /// `existing_ids` are injected into the prompt so the model avoids names that are
    /// already registered (case-sensitive on the model's side, like Java).
    pub async fn generate_markdown(
        &self,
        description: &str,
        existing_ids: &[String],
    ) -> crate::Result<String> {
        let existing = if existing_ids.is_empty() {
            "(none)".to_string()
        } else {
            existing_ids.join(", ")
        };
        let prompt = PROMPT_TEMPLATE
            .replacen("{}", description, 1)
            .replacen("{}", &existing, 1);
        let markdown =
            auxiliary_text(Arc::clone(&self.model), vec![Msg::user(prompt)], None).await?;
        Ok(markdown.trim().to_string())
    }

    /// Generates a spec and validates it by round-tripping through the loader.
    ///
    /// A malformed answer is an error (Java throws `IllegalStateException`): silently writing
    /// an unparseable file would surface only on the next reasoning step, far from the cause.
    pub async fn generate_and_validate(
        &self,
        description: &str,
        agent_name: &str,
        existing_ids: &[String],
        main_workspace: Option<&str>,
    ) -> crate::Result<GeneratedSpec> {
        let markdown = self.generate_markdown(description, existing_ids).await?;
        match parse(&markdown, agent_name, main_workspace) {
            Some(declaration) => Ok(GeneratedSpec {
                markdown,
                declaration,
            }),
            None => Err(crate::Error::Agent(format!(
                "LLM produced a malformed subagent spec for '{agent_name}'; \
                 could not parse frontmatter"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use agentscope_core::message::{ContentBlock, TextBlock};
    use agentscope_core::model::ChatResponse;
    use agentscope_core::model::{GenerateOptions, ToolSchema};
    use futures::stream::{self, BoxStream};

    use super::*;

    /// Replays fixed text and records the prompt it received.
    struct StubModel {
        text: String,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl StubModel {
        fn new(text: &str) -> Self {
            Self {
                text: text.to_string(),
                seen: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl Model for StubModel {
        fn stream(
            &self,
            messages: Vec<Msg>,
            _tools: Vec<ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, crate::Result<ChatResponse>> {
            self.seen
                .lock()
                .unwrap()
                .extend(messages.iter().map(Msg::text_content));
            let item = ChatResponse::new(
                "r1",
                vec![ContentBlock::Text(TextBlock::new(self.text.clone()))],
                Default::default(),
                None,
                None,
            );
            Box::pin(stream::iter(vec![Ok(item)]))
        }

        fn model_name(&self) -> &str {
            "stub"
        }
    }

    const SPEC: &str = "---\ndescription: Reviews pull requests\nmode: subagent\n---\n\n\
                        You are a reviewer. Read the diff, list defects, and stop.\n";

    #[tokio::test]
    async fn generated_markdown_round_trips_through_the_loader() {
        let model = Arc::new(StubModel::new(SPEC));
        let generator = SubagentSpecGenerator::new(Arc::clone(&model) as Arc<dyn Model>);
        let spec = generator
            .generate_and_validate("review PRs", "reviewer", &[], None)
            .await
            .expect("generated spec parses");

        assert_eq!(spec.declaration.name, "reviewer");
        assert_eq!(spec.declaration.description, "Reviews pull requests");
        assert!(spec.declaration.prompt.contains("You are a reviewer"));
        assert!(spec.markdown.starts_with("---"));

        let sent = model.seen.lock().unwrap().join("\n");
        assert!(sent.contains("review PRs"), "{sent}");
        assert!(sent.contains("(none)"), "{sent}");
    }

    #[tokio::test]
    async fn existing_ids_reach_the_prompt() {
        let model = Arc::new(StubModel::new(SPEC));
        let generator = SubagentSpecGenerator::new(Arc::clone(&model) as Arc<dyn Model>);
        let existing = vec!["general-purpose".to_string(), "reviewer".to_string()];
        generator
            .generate_and_validate("review PRs", "reviewer", &existing, None)
            .await
            .unwrap();
        let sent = model.seen.lock().unwrap().join("\n");
        assert!(sent.contains("general-purpose, reviewer"), "{sent}");
    }

    #[tokio::test]
    async fn a_malformed_answer_is_an_error() {
        let model = Arc::new(StubModel::new("Sure! Here is an agent for you."));
        let generator = SubagentSpecGenerator::new(Arc::clone(&model) as Arc<dyn Model>);
        let error = generator
            .generate_and_validate("review PRs", "reviewer", &[], None)
            .await
            .expect_err("unparseable markdown must fail");
        assert!(
            error.to_string().contains("could not parse frontmatter"),
            "{error}"
        );
    }
}
