//! Session tree & freshness evaluation, mirroring
//! `io.agentscope.harness.agent.memory.session`.
//!
//! Session entries form an append-only tree via `id`/`parentId` and are serialized
//! as JSONL (one entry per line), tagged by `type`.

use std::collections::HashMap;

use chrono::{DateTime, Duration as ChronoDuration, Local, NaiveTime};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A session tree entry (message / compaction / summary).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    Message {
        id: String,
        #[serde(rename = "parentId", skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        timestamp: String,
        role: String,
        content: String,
        #[serde(rename = "toolCallId", skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
    },
    Compaction {
        id: String,
        #[serde(rename = "parentId", skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        timestamp: String,
        #[serde(rename = "firstKeptEntryId")]
        first_kept_entry_id: String,
        #[serde(rename = "summaryEntryId")]
        summary_entry_id: String,
    },
    Summary {
        id: String,
        #[serde(rename = "parentId", skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        timestamp: String,
        summary: String,
        #[serde(default = "default_format")]
        format: String,
    },
}

fn default_format() -> String {
    "structured".to_string()
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn new_id() -> String {
    Uuid::new_v4().to_string()
}

impl SessionEntry {
    pub fn message(
        parent_id: Option<&str>,
        role: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        SessionEntry::Message {
            id: new_id(),
            parent_id: parent_id.map(str::to_string),
            timestamp: now_iso(),
            role: role.into(),
            content: content.into(),
            tool_call_id: None,
        }
    }

    pub fn compaction(
        parent_id: Option<&str>,
        first_kept_entry_id: impl Into<String>,
        summary_entry_id: impl Into<String>,
    ) -> Self {
        SessionEntry::Compaction {
            id: new_id(),
            parent_id: parent_id.map(str::to_string),
            timestamp: now_iso(),
            first_kept_entry_id: first_kept_entry_id.into(),
            summary_entry_id: summary_entry_id.into(),
        }
    }

    pub fn summary(parent_id: Option<&str>, summary: impl Into<String>) -> Self {
        SessionEntry::Summary {
            id: new_id(),
            parent_id: parent_id.map(str::to_string),
            timestamp: now_iso(),
            summary: summary.into(),
            format: default_format(),
        }
    }

    pub fn id(&self) -> &str {
        match self {
            SessionEntry::Message { id, .. }
            | SessionEntry::Compaction { id, .. }
            | SessionEntry::Summary { id, .. } => id,
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            SessionEntry::Message { parent_id, .. }
            | SessionEntry::Compaction { parent_id, .. }
            | SessionEntry::Summary { parent_id, .. } => parent_id.as_deref(),
        }
    }

    pub fn is_message(&self) -> bool {
        matches!(self, SessionEntry::Message { .. })
    }
}

/// Manages an append-only in-memory session tree.
#[derive(Debug, Default)]
pub struct SessionTree {
    entries_by_id: HashMap<String, SessionEntry>,
    append_order: Vec<SessionEntry>,
    last_compaction_first_kept_id: Option<String>,
    last_summary_entry_id: Option<String>,
}

impl SessionTree {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends an entry and returns its id.
    pub fn append(&mut self, entry: SessionEntry) -> String {
        let id = entry.id().to_string();
        if let SessionEntry::Compaction {
            first_kept_entry_id,
            summary_entry_id,
            ..
        } = &entry
        {
            self.last_compaction_first_kept_id = Some(first_kept_entry_id.clone());
            self.last_summary_entry_id = Some(summary_entry_id.clone());
        }
        self.entries_by_id.insert(id.clone(), entry.clone());
        self.append_order.push(entry);
        id
    }

    pub fn size(&self) -> usize {
        self.append_order.len()
    }

    pub fn get_all_entries(&self) -> &[SessionEntry] {
        &self.append_order
    }

    pub fn get_message_entries(&self) -> Vec<&SessionEntry> {
        self.append_order
            .iter()
            .filter(|e| e.is_message())
            .collect()
    }

    /// Builds the LLM-visible context: summary (if any) + messages from the first kept entry.
    pub fn build_context(&self) -> Vec<&SessionEntry> {
        if self.append_order.is_empty() {
            return Vec::new();
        }
        let Some(first_kept) = &self.last_compaction_first_kept_id else {
            return self.append_order.iter().collect();
        };
        let mut context: Vec<&SessionEntry> = Vec::new();
        if let Some(summary_id) = &self.last_summary_entry_id {
            if let Some(s) = self.entries_by_id.get(summary_id) {
                context.push(s);
            }
        }
        let mut found = false;
        for entry in &self.append_order {
            if entry.id() == first_kept {
                found = true;
            }
            if found && entry.is_message() {
                context.push(entry);
            }
        }
        context
    }

    /// Serializes all entries as JSONL.
    pub fn to_jsonl(&self) -> String {
        self.append_order
            .iter()
            .map(|e| serde_json::to_string(e).expect("SessionEntry is serializable"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Loads entries from a JSONL string; returns the number of new entries added.
    pub fn load_jsonl(&mut self, content: &str) -> usize {
        let mut count = 0;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<SessionEntry>(line) {
                let id = entry.id().to_string();
                if self.entries_by_id.contains_key(&id) {
                    continue;
                }
                self.entries_by_id.insert(id, entry.clone());
                self.append_order.push(entry);
                count += 1;
            }
        }
        count
    }
}

/// Evaluates whether a session is stale and should be reset.
///
/// Supports two policies: daily reset after a configured local hour, and an idle timeout.
#[derive(Debug, Clone)]
pub struct SessionFreshnessEvaluator {
    daily_reset_hour: i32,
    idle_timeout: Option<ChronoDuration>,
}

impl SessionFreshnessEvaluator {
    /// Creates an evaluator with a daily reset hour and optional idle timeout.
    pub fn new(daily_reset_hour: i32, idle_timeout: Option<ChronoDuration>) -> Self {
        Self {
            daily_reset_hour,
            idle_timeout,
        }
    }

    /// Defaults: daily reset at 4 AM, idle timeout of 2 hours.
    pub fn defaults() -> Self {
        Self::new(4, Some(ChronoDuration::hours(2)))
    }

    pub fn daily_reset_hour(&self) -> i32 {
        self.daily_reset_hour
    }

    pub fn idle_timeout(&self) -> Option<ChronoDuration> {
        self.idle_timeout
    }

    /// Returns true when the session should be considered stale and reset.
    pub fn is_stale(&self, last_activity: Option<DateTime<Local>>) -> bool {
        let Some(last) = last_activity else {
            return true;
        };
        let now = Local::now();

        if let Some(timeout) = self.idle_timeout {
            if now - last > timeout {
                return true;
            }
        }

        if self.daily_reset_hour >= 0 {
            let reset_time = NaiveTime::from_hms_opt(self.daily_reset_hour as u32, 0, 0)
                .expect("valid reset hour");
            let today_reset = now.date_naive().and_time(reset_time);
            if now.naive_local() > today_reset && last.naive_local() < today_reset {
                return true;
            }
        }

        false
    }
}
