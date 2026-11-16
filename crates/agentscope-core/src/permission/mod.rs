//! Permission evaluation engine and related types, mirroring
//! `io.agentscope.core.permission`.
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

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
        Self {
            tool_name: tool_name.into(),
            rule_content,
            behavior,
            source: source.into(),
        }
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
        Self {
            path: path.into(),
            source: source.into(),
        }
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
        Self {
            behavior,
            message,
            decision_reason: None,
            updated_input: None,
            suggested_rules: None,
        }
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

    /// Returns a copy of this context with `rules` appended to their behavior tables.
    ///
    /// Used to persist rules accepted at runtime (Java `ConfirmResult.rules`): with a state
    /// store configured, every call rebuilds the engine from the persisted context, so an
    /// engine-only addition is lost at the next call boundary ("always allow for this session"
    /// would not outlive the call that granted it).
    ///
    /// Passthrough rules are not persisted, and a rule already present in its table is not
    /// added again — re-accepting an equivalent answer must not grow the persisted tables.
    /// `None` and `Some("")` rule content are equivalent at evaluation time, so they
    /// de-duplicate too. This instance is never mutated.
    pub fn with_added_rules(&self, rules: &[PermissionRule]) -> Self {
        if rules.is_empty() {
            return self.clone();
        }
        let mut context = self.clone();
        for rule in rules {
            let table = match rule.behavior {
                PermissionBehavior::Allow => &mut context.allow_rules,
                PermissionBehavior::Deny => &mut context.deny_rules,
                PermissionBehavior::Ask => &mut context.ask_rules,
                PermissionBehavior::Passthrough => continue,
            };
            add_if_absent(table, rule);
        }
        context
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
    pub fn add_working_directory(
        mut self,
        key: impl Into<String>,
        dir: AdditionalWorkingDirectory,
    ) -> Self {
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

/// Adds `rule` to the table its behavior selects (a no-op for `Passthrough`).
fn push_rule(
    allow: &mut HashMap<String, Vec<PermissionRule>>,
    deny: &mut HashMap<String, Vec<PermissionRule>>,
    ask: &mut HashMap<String, Vec<PermissionRule>>,
    rule: PermissionRule,
) {
    let table = match rule.behavior {
        PermissionBehavior::Allow => allow,
        PermissionBehavior::Deny => deny,
        PermissionBehavior::Ask => ask,
        PermissionBehavior::Passthrough => return,
    };
    add_if_absent(table, &rule);
}

/// Appends `rule` to `table` unless an equivalent rule is already there.
fn add_if_absent(table: &mut HashMap<String, Vec<PermissionRule>>, rule: &PermissionRule) {
    let bucket = table.entry(rule.tool_name.clone()).or_default();
    if bucket.iter().any(|existing| same_rule(existing, rule)) {
        return;
    }
    bucket.push(rule.clone());
}

/// Rule equivalence for de-duplication: `None` and empty rule content both mean
/// "unconditional" to the matcher, so they must compare equal here too.
fn same_rule(a: &PermissionRule, b: &PermissionRule) -> bool {
    a.tool_name == b.tool_name
        && a.behavior == b.behavior
        && a.source == b.source
        && a.rule_content.as_deref().unwrap_or("") == b.rule_content.as_deref().unwrap_or("")
}

/// Whether an input key names a filesystem path.
///
/// Only path-shaped arguments take part in the `AcceptEdits` working-directory check; every
/// other argument is ignored, so a tool that also carries a query string is not judged by it.
fn is_path_key(key: &str) -> bool {
    matches!(
        key,
        "path" | "file_path" | "filePath" | "dir" | "directory" | "cwd" | "target" | "source"
    ) || key.ends_with("_path")
        || key.ends_with("Path")
        || key.ends_with("_dir")
        || key.ends_with("Dir")
}

/// Resolves `raw` against the process working directory and normalizes `.`/`..` lexically.
///
/// Deliberately lexical, not `canonicalize`: the target of an edit usually does not exist yet,
/// and following symlinks would let a link inside the workspace point the check outside it.
fn resolve_path(raw: &str) -> PathBuf {
    let path = Path::new(raw);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    normalize_lexically(&absolute)
}

/// Drops `.` components and folds `..` into the parent, without touching the filesystem.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The path-like arguments of a tool input (top level only).
fn path_arguments(input: &JsonMap) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for (key, value) in input {
        if !is_path_key(key) {
            continue;
        }
        let Some(raw) = value.as_str().map(str::trim).filter(|raw| !raw.is_empty()) else {
            continue;
        };
        out.push(resolve_path(raw));
    }
    out
}

/// The rule an `Ask` decision suggests for a "always allow" answer: the same tool narrowed to
/// this invocation's command prefix or path.
///
/// Mirrors Java `PermissionDecision.suggestedRules`: the host can persist the suggestion
/// verbatim instead of inventing an unconditional allow for the whole tool.
/// Attaches the "always allow" suggestion to an `Ask` decision (#3243).
///
/// The suggestion travels with the decision instead of being dropped: a host that offers
/// "always allow" can persist exactly the rule it shows the user. Every `Ask` path (an explicit
/// ask rule, the mode's tool check, or the default branch) goes through here so none of them
/// loses the suggestion.
fn ensure_ask_suggestion(
    decision: PermissionDecision,
    tool_name: &str,
    input: &JsonMap,
) -> PermissionDecision {
    if decision.behavior == PermissionBehavior::Ask && decision.suggested_rules.is_none() {
        decision.with_suggested_rules(Some(vec![suggested_rule(tool_name, input)]))
    } else {
        decision
    }
}

fn suggested_rule(tool: &str, input: &JsonMap) -> PermissionRule {
    let command = ["command", "cmd"]
        .iter()
        .find_map(|key| input.get(*key).and_then(|value| value.as_str()))
        .map(|command| {
            command
                .split_whitespace()
                .next()
                .unwrap_or(command)
                .to_string()
        });
    let content = command.or_else(|| {
        input
            .iter()
            .find(|(key, _)| is_path_key(key))
            .and_then(|(_, value)| value.as_str())
            .map(str::to_string)
    });
    PermissionRule::new(tool, content, PermissionBehavior::Allow, "suggested")
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
    ///
    /// The rule lands in the context snapshot too: [`Self::context`] is the state a host reads
    /// back (and what a newly created engine is seeded with), so leaving it stale would hide a
    /// just-granted `always allow` from both the host and the next `set_permission_context`.
    pub fn add_rule(&mut self, rule: PermissionRule) {
        push_rule(
            &mut self.allow_rules,
            &mut self.deny_rules,
            &mut self.ask_rules,
            rule.clone(),
        );
        push_rule(
            &mut self.context.allow_rules,
            &mut self.context.deny_rules,
            &mut self.context.ask_rules,
            rule,
        );
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
            return ensure_ask_suggestion(d, tool_name, input);
        }
        if let Some(d) = self.tool_check(tool_name, is_read_only, input) {
            return d;
        }
        if let Some(d) = self.check_allow_rules(tool_name, input, &matcher) {
            return d;
        }
        if self.context.mode == PermissionMode::Bypass {
            return PermissionDecision::allow(format!(
                "Permission granted for {} (bypass mode)",
                tool_name
            ))
            .with_decision_reason("Bypass mode allows all operations");
        }
        if self.context.mode == PermissionMode::DontAsk {
            return PermissionDecision::deny(format!(
                "Permission denied for {} (dont_ask mode)",
                tool_name
            ))
            .with_decision_reason("User is not available to answer permission prompts");
        }
        ensure_ask_suggestion(
            PermissionDecision::ask(format!("Permission required for {}", tool_name))
                .with_decision_reason(format!("Mode: {:?}", self.context.mode)),
            tool_name,
            input,
        )
    }

    /// Whether every path-like argument of `input` lies inside the working scope: the process
    /// workspace root (the directory relative paths resolve against) plus every configured
    /// [`PermissionContextState::working_directories`] entry.
    ///
    /// Fail-closed: an input with no recognisable path argument is **not** in scope, so a tool
    /// whose target cannot be proven keeps asking. Used by `AcceptEdits`, which auto-approves
    /// edits inside the working directories instead of ignoring them (#2870).
    fn within_working_directories(&self, input: &JsonMap) -> bool {
        let paths = path_arguments(input);
        if paths.is_empty() {
            return false;
        }
        let mut roots = vec![std::env::current_dir()
            .map(|dir| normalize_lexically(&dir))
            .unwrap_or_default()];
        roots.extend(
            self.context
                .working_directories
                .values()
                .map(|dir| resolve_path(&dir.path)),
        );
        paths.iter().all(|path| {
            roots
                .iter()
                .any(|root| !root.as_os_str().is_empty() && path.starts_with(root))
        })
    }

    fn tool_check(
        &self,
        tool_name: &str,
        is_read_only: bool,
        input: &JsonMap,
    ) -> Option<PermissionDecision> {
        match self.context.mode {
            PermissionMode::Explore => {
                if is_read_only {
                    Some(
                        PermissionDecision::allow(format!(
                            "Permission granted for {} (explore mode - read-only tool)",
                            tool_name
                        ))
                        .with_decision_reason("Explore mode allows read-only operations"),
                    )
                } else {
                    Some(
                        PermissionDecision::deny(format!(
                            "Permission denied for {} (explore mode is read-only)",
                            tool_name
                        ))
                        .with_decision_reason("Explore mode does not allow modifications"),
                    )
                }
            }
            PermissionMode::AcceptEdits => {
                if is_read_only {
                    Some(
                        PermissionDecision::allow(format!(
                            "Permission granted for {} (accept edits mode - read-only tool)",
                            tool_name
                        ))
                        .with_decision_reason("Accept edits mode allows read-only operations"),
                    )
                } else if self.within_working_directories(input) {
                    Some(
                        PermissionDecision::allow(format!(
                            "Permission granted for {} (accept edits mode - working directory)",
                            tool_name
                        ))
                        .with_decision_reason(
                            "Accept edits mode allows edits inside the working directories",
                        ),
                    )
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn check_deny_rules<F>(
        &self,
        tool: &str,
        input: &JsonMap,
        matcher: &F,
    ) -> Option<PermissionDecision>
    where
        F: Fn(&str, &JsonMap) -> bool,
    {
        for rule in rules_for(&self.deny_rules, tool) {
            if rule_matches(rule, input, matcher) {
                return Some(
                    PermissionDecision::deny(format!("Permission to use {} has been denied", tool))
                        .with_decision_reason(format!(
                            "Rule: {}",
                            rule.rule_content.as_deref().unwrap_or("")
                        )),
                );
            }
        }
        None
    }

    fn check_ask_rules<F>(
        &self,
        tool: &str,
        input: &JsonMap,
        matcher: &F,
    ) -> Option<PermissionDecision>
    where
        F: Fn(&str, &JsonMap) -> bool,
    {
        for rule in rules_for(&self.ask_rules, tool) {
            if rule_matches(rule, input, matcher) {
                return Some(
                    PermissionDecision::ask(format!("Permission required for {}", tool))
                        .with_decision_reason(format!(
                            "Rule: {}",
                            rule.rule_content.as_deref().unwrap_or("")
                        )),
                );
            }
        }
        None
    }

    fn check_allow_rules<F>(
        &self,
        tool: &str,
        input: &JsonMap,
        matcher: &F,
    ) -> Option<PermissionDecision>
    where
        F: Fn(&str, &JsonMap) -> bool,
    {
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

fn rules_for<'a>(
    table: &'a HashMap<String, Vec<PermissionRule>>,
    tool: &str,
) -> &'a [PermissionRule] {
    table.get(tool).map(|v| v.as_slice()).unwrap_or(&[])
}

fn rule_matches<F>(rule: &PermissionRule, input: &JsonMap, matcher: &F) -> bool
where
    F: Fn(&str, &JsonMap) -> bool,
{
    match &rule.rule_content {
        Some(c) if !c.is_empty() => matcher(c, input),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with_mode() -> PermissionContextState {
        PermissionContextState::builder()
            .mode(PermissionMode::Explore)
            .add_allow_rule(
                "read",
                PermissionRule::new("read", None, PermissionBehavior::Allow, "policy"),
            )
            .build()
    }

    #[test]
    fn added_rules_are_routed_by_behavior_and_keep_the_rest_of_the_context() {
        let context = ctx_with_mode();
        let merged = context.with_added_rules(&[
            PermissionRule::new("add", None, PermissionBehavior::Allow, "user"),
            PermissionRule::new("shell", None, PermissionBehavior::Deny, "user"),
            PermissionRule::new("add", None, PermissionBehavior::Ask, "user"),
            // Passthrough rules are not persisted.
            PermissionRule::new("add", None, PermissionBehavior::Passthrough, "user"),
        ]);
        assert_eq!(merged.mode, PermissionMode::Explore);
        assert!(
            merged.allow_rules.contains_key("read"),
            "existing rules stay"
        );
        assert_eq!(merged.allow_rules["add"].len(), 1);
        assert_eq!(merged.ask_rules["add"].len(), 1);
        assert_eq!(merged.deny_rules["shell"].len(), 1);
        // The original instance is never mutated.
        assert!(!context.allow_rules.contains_key("add"));

        // Re-adding an equivalent rule (None vs empty content) does not grow the table.
        let again = merged.with_added_rules(&[PermissionRule::new(
            "add",
            Some(String::new()),
            PermissionBehavior::Allow,
            "user",
        )]);
        assert_eq!(again.allow_rules["add"].len(), 1);
    }

    #[test]
    fn the_engine_does_not_duplicate_a_rule_already_in_the_context() {
        let rule = PermissionRule::new("add", None, PermissionBehavior::Allow, "user");
        let mut engine = PermissionEngine::new(ctx_with_mode());
        engine.add_rule(rule.clone());
        engine.add_rule(rule);
        assert_eq!(engine.context().allow_rules["add"].len(), 1);
    }

    fn accept_edits_engine() -> PermissionEngine {
        PermissionEngine::new(
            PermissionContextState::builder()
                .mode(PermissionMode::AcceptEdits)
                .add_working_directory(
                    "extra",
                    AdditionalWorkingDirectory::new("/tmp/workspace-extra", "test"),
                )
                .build(),
        )
    }

    fn path_input(path: &str) -> JsonMap {
        let mut input = JsonMap::new();
        input.insert("path".to_string(), serde_json::json!(path));
        input
    }

    #[test]
    fn accept_edits_allows_an_edit_inside_the_working_directories() {
        let engine = accept_edits_engine();

        // The workspace root (where relative paths resolve) is in scope.
        let workspace_file = std::env::current_dir().unwrap().join("notes.md");
        let inside = path_input(&workspace_file.to_string_lossy());
        assert_eq!(
            engine
                .check_permission("write_file", false, &inside, |_, _| false)
                .behavior,
            PermissionBehavior::Allow
        );

        // So is a configured additional directory, and `..` cannot climb out of it.
        assert_eq!(
            engine
                .check_permission(
                    "write_file",
                    false,
                    &path_input("/tmp/workspace-extra/src/main.rs"),
                    |_, _| false
                )
                .behavior,
            PermissionBehavior::Allow
        );
        assert_eq!(
            engine
                .check_permission(
                    "write_file",
                    false,
                    &path_input("/tmp/workspace-extra/../elsewhere/passwd"),
                    |_, _| false
                )
                .behavior,
            PermissionBehavior::Ask
        );
        // No path argument means nothing can be proven: keep asking.
        assert_eq!(
            engine
                .check_permission("shell", false, &JsonMap::new(), |_, _| false)
                .behavior,
            PermissionBehavior::Ask
        );
    }

    #[test]
    fn an_ask_decision_suggests_a_rule_for_this_invocation() {
        let engine = PermissionEngine::new(PermissionContextState::default());
        let decision = engine.check_permission(
            "write_file",
            false,
            &path_input("/outside/a.txt"),
            |_, _| false,
        );
        assert_eq!(decision.behavior, PermissionBehavior::Ask);
        let rules = decision
            .suggested_rules
            .expect("an ask decision carries its suggestion");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].tool_name, "write_file");
        assert_eq!(rules[0].rule_content.as_deref(), Some("/outside/a.txt"));
        assert_eq!(rules[0].behavior, PermissionBehavior::Allow);

        // A command tool is narrowed to its program, not the whole command line.
        let mut command = JsonMap::new();
        command.insert(
            "command".to_string(),
            serde_json::json!("git push origin main"),
        );
        let decision = engine.check_permission("shell", false, &command, |_, _| false);
        let rules = decision.suggested_rules.expect("a suggestion");
        assert_eq!(rules[0].rule_content.as_deref(), Some("git"));
    }
}
