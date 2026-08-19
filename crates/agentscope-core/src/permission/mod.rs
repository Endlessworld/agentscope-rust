//! Permission evaluation engine and related types, mirroring
//! `io.agentscope.core.permission`.
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::message::JsonMap;

/// Permission mode controlling how the engine evaluates tool requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PermissionMode {
    #[serde(rename = "default")]
    #[default]
    Default,
    #[serde(rename = "accept_edits")]
    AcceptEdits,
    #[serde(rename = "explore")]
    Explore,
    #[serde(rename = "bypass")]
    Bypass,
    #[serde(rename = "dont_ask")]
    DontAsk,
}

/// Outcome of a permission rule or tool self-check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionBehavior {
    #[serde(rename = "allow")]
    Allow,
    #[serde(rename = "deny")]
    Deny,
    #[serde(rename = "ask")]
    Ask,
    #[serde(rename = "passthrough")]
    Passthrough,
}

/// A single permission rule pinned to a tool name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRule {
    #[serde(rename = "tool_name")]
    pub tool_name: String,
    #[serde(rename = "rule_content")]
    pub rule_content: Option<String>,
    pub behavior: PermissionBehavior,
    pub source: String,
}

impl PermissionRule {
    pub fn new(
        tool_name: impl Into<String>,
        rule_content: Option<String>,
        behavior: PermissionBehavior,
        source: impl Into<String>,
    ) -> Self {
        Self { tool_name: tool_name.into(), rule_content, behavior, source: source.into() }
    }
}

/// A directory included in a permission context's working scope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdditionalWorkingDirectory {
    pub path: String,
    pub source: String,
}

impl AdditionalWorkingDirectory {
    pub fn new(path: impl Into<String>, source: impl Into<String>) -> Self {
        Self { path: path.into(), source: source.into() }
    }
}

/// Decision returned by a permission rule match or a tool self-check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionDecision {
    pub behavior: PermissionBehavior,
    pub message: String,
    #[serde(rename = "decision_reason", default)]
    pub decision_reason: Option<String>,
    #[serde(rename = "updated_input", default)]
    pub updated_input: Option<JsonMap>,
    #[serde(rename = "suggested_rules", default)]
    pub suggested_rules: Option<Vec<PermissionRule>>,
}

impl PermissionDecision {
    pub fn allow(message: impl Into<String>) -> Self {
        Self::from_behavior(PermissionBehavior::Allow, message.into())
    }
    pub fn deny(message: impl Into<String>) -> Self {
        Self::from_behavior(PermissionBehavior::Deny, message.into())
    }
    pub fn ask(message: impl Into<String>) -> Self {
        Self::from_behavior(PermissionBehavior::Ask, message.into())
    }
    pub fn passthrough(message: impl Into<String>) -> Self {
        Self::from_behavior(PermissionBehavior::Passthrough, message.into())
    }
    fn from_behavior(behavior: PermissionBehavior, message: String) -> Self {
        Self { behavior, message, decision_reason: None, updated_input: None, suggested_rules: None }
    }

    pub fn with_suggested_rules(mut self, rules: Option<Vec<PermissionRule>>) -> Self {
        self.suggested_rules = rules;
        self
    }
    pub fn with_decision_reason(mut self, reason: impl Into<String>) -> Self {
        self.decision_reason = Some(reason.into());
        self
    }
    pub fn with_updated_input(mut self, input: JsonMap) -> Self {
        self.updated_input = Some(input);
        self
    }
}

/// Permission evaluation context: mode + working directories + three rule tables.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PermissionContextState {
    #[serde(default)]
    pub mode: PermissionMode,
    #[serde(rename = "working_directories", default)]
    pub working_directories: HashMap<String, AdditionalWorkingDirectory>,
    #[serde(rename = "allow_rules", default)]
    pub allow_rules: HashMap<String, Vec<PermissionRule>>,
    #[serde(rename = "deny_rules", default)]
    pub deny_rules: HashMap<String, Vec<PermissionRule>>,
    #[serde(rename = "ask_rules", default)]
    pub ask_rules: HashMap<String, Vec<PermissionRule>>,
}

impl PermissionContextState {
    /// Whether this context is "trivial" — built with no customisation.
    pub fn is_trivial(&self) -> bool {
        self.mode == PermissionMode::Default
            && self.working_directories.is_empty()
            && self.allow_rules.is_empty()
            && self.deny_rules.is_empty()
            && self.ask_rules.is_empty()
    }

    pub fn builder() -> PermissionContextBuilder {
        PermissionContextBuilder::default()
    }

    /// Returns a copy of this context with the mode replaced.
    pub fn with_mode(&self, mode: PermissionMode) -> Self {
        let mut c = self.clone();
        c.mode = mode;
        c
    }
}

/// Builder for [`PermissionContextState`].
#[derive(Debug, Clone, Default)]
pub struct PermissionContextBuilder {
    mode: PermissionMode,
    working_directories: HashMap<String, AdditionalWorkingDirectory>,
    allow_rules: HashMap<String, Vec<PermissionRule>>,
    deny_rules: HashMap<String, Vec<PermissionRule>>,
    ask_rules: HashMap<String, Vec<PermissionRule>>,
}

impl PermissionContextBuilder {
    pub fn mode(mut self, mode: PermissionMode) -> Self {
        self.mode = mode;
        self
    }
    pub fn add_working_directory(mut self, key: impl Into<String>, dir: AdditionalWorkingDirectory) -> Self {
        self.working_directories.insert(key.into(), dir);
        self
    }
    pub fn add_allow_rule(mut self, tool: impl Into<String>, rule: PermissionRule) -> Self {
        self.allow_rules.entry(tool.into()).or_default().push(rule);
        self
    }
    pub fn add_deny_rule(mut self, tool: impl Into<String>, rule: PermissionRule) -> Self {
        self.deny_rules.entry(tool.into()).or_default().push(rule);
        self
    }
    pub fn add_ask_rule(mut self, tool: impl Into<String>, rule: PermissionRule) -> Self {
        self.ask_rules.entry(tool.into()).or_default().push(rule);
        self
    }
    pub fn build(self) -> PermissionContextState {
        PermissionContextState {
            mode: self.mode,
            working_directories: self.working_directories,
            allow_rules: self.allow_rules,
            deny_rules: self.deny_rules,
            ask_rules: self.ask_rules,
        }
    }
}

/// Evaluates tool execution requests against configured permission rules.
#[derive(Debug, Clone)]
pub struct PermissionEngine {
    context: PermissionContextState,
    allow_rules: HashMap<String, Vec<PermissionRule>>,
    deny_rules: HashMap<String, Vec<PermissionRule>>,
    ask_rules: HashMap<String, Vec<PermissionRule>>,
}

impl PermissionEngine {
    /// Creates an engine seeded from the given context's rules and mode.
    pub fn new(context: PermissionContextState) -> Self {
        Self {
            allow_rules: context.allow_rules.clone(),
            deny_rules: context.deny_rules.clone(),
            ask_rules: context.ask_rules.clone(),
            context,
        }
    }

    /// Returns the underlying context.
    pub fn context(&self) -> &PermissionContextState {
        &self.context
    }

    /// Adds a rule to the engine's internal rule set.
    pub fn add_rule(&mut self, rule: PermissionRule) {
        match rule.behavior {
            PermissionBehavior::Allow => self.allow_rules.entry(rule.tool_name.clone()).or_default().push(rule),
            PermissionBehavior::Deny => self.deny_rules.entry(rule.tool_name.clone()).or_default().push(rule),
            PermissionBehavior::Ask => self.ask_rules.entry(rule.tool_name.clone()).or_default().push(rule),
            PermissionBehavior::Passthrough => {}
        }
    }

    /// Resolves a permission decision for the given tool invocation.
    ///
    /// `matcher` decides whether a rule's `rule_content` pattern matches the input.
    pub fn check_permission<F>(
        &self,
        tool_name: &str,
        is_read_only: bool,
        input: &JsonMap,
        matcher: F,
    ) -> PermissionDecision
    where
        F: Fn(&str, &JsonMap) -> bool,
    {
        if let Some(d) = self.check_deny_rules(tool_name, input, &matcher) {
            return d;
        }
        if let Some(d) = self.check_ask_rules(tool_name, input, &matcher) {
            return d;
        }
        if let Some(d) = self.tool_check(tool_name, is_read_only) {
            return d;
        }
        if let Some(d) = self.check_allow_rules(tool_name, input, &matcher) {
            return d;
        }
        if self.context.mode == PermissionMode::Bypass {
            return PermissionDecision::allow(format!("Permission granted for {} (bypass mode)", tool_name))
                .with_decision_reason("Bypass mode allows all operations");
        }
        if self.context.mode == PermissionMode::DontAsk {
            return PermissionDecision::deny(format!("Permission denied for {} (dont_ask mode)", tool_name))
                .with_decision_reason("User is not available to answer permission prompts");
        }
        PermissionDecision::ask(format!("Permission required for {}", tool_name))
            .with_decision_reason(format!("Mode: {:?}", self.context.mode))
    }

    fn tool_check(&self, tool_name: &str, is_read_only: bool) -> Option<PermissionDecision> {
        match self.context.mode {
            PermissionMode::Explore => {
                if is_read_only {
                    Some(
                        PermissionDecision::allow(format!("Permission granted for {} (explore mode - read-only tool)", tool_name))
                            .with_decision_reason("Explore mode allows read-only operations"),
                    )
                } else {
                    Some(
                        PermissionDecision::deny(format!("Permission denied for {} (explore mode is read-only)", tool_name))
                            .with_decision_reason("Explore mode does not allow modifications"),
                    )
                }
            }
            PermissionMode::AcceptEdits => {
                if is_read_only {
                    Some(
                        PermissionDecision::allow(format!("Permission granted for {} (accept edits mode - read-only tool)", tool_name))
                            .with_decision_reason("Accept edits mode allows read-only operations"),
                    )
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn check_deny_rules<F>(&self, tool: &str, input: &JsonMap, matcher: &F) -> Option<PermissionDecision>
    where F: Fn(&str, &JsonMap) -> bool {
        for rule in rules_for(&self.deny_rules, tool) {
            if rule_matches(rule, input, matcher) {
                return Some(
                    PermissionDecision::deny(format!("Permission to use {} has been denied", tool))
                        .with_decision_reason(format!("Rule: {}", rule.rule_content.as_deref().unwrap_or(""))),
                );
            }
        }
        None
    }

    fn check_ask_rules<F>(&self, tool: &str, input: &JsonMap, matcher: &F) -> Option<PermissionDecision>
    where F: Fn(&str, &JsonMap) -> bool {
        for rule in rules_for(&self.ask_rules, tool) {
            if rule_matches(rule, input, matcher) {
                return Some(
                    PermissionDecision::ask(format!("Permission required for {}", tool))
                        .with_decision_reason(format!("Rule: {}", rule.rule_content.as_deref().unwrap_or(""))),
                );
            }
        }
        None
    }

    fn check_allow_rules<F>(&self, tool: &str, input: &JsonMap, matcher: &F) -> Option<PermissionDecision>
    where F: Fn(&str, &JsonMap) -> bool {
        for rule in rules_for(&self.allow_rules, tool) {
            if rule_matches(rule, input, matcher) {
                return Some(
                    PermissionDecision::allow(format!("Permission granted for {}", tool))
                        .with_updated_input(input.clone()),
                );
            }
        }
        None
    }
}

fn rules_for<'a>(table: &'a HashMap<String, Vec<PermissionRule>>, tool: &str) -> &'a [PermissionRule] {
    table.get(tool).map(|v| v.as_slice()).unwrap_or(&[])
}

fn rule_matches<F>(rule: &PermissionRule, input: &JsonMap, matcher: &F) -> bool
where F: Fn(&str, &JsonMap) -> bool {
    match &rule.rule_content {
        Some(c) if !c.is_empty() => matcher(c, input),
        _ => true,
    }
}
