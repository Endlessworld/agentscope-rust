//! A page of Level-3 full history, mirroring `MessagePage`.
//!
//! Unlike the Level-4 context, this includes messages already compacted away, so
//! operators can audit what the agent originally saw.

use serde::{Deserialize, Serialize};

/// One full-content history entry.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageItem {
    /// Sequence within the session.
    #[serde(default)]
    pub seq: i64,
    /// Message role.
    #[serde(default)]
    pub role: String,
    /// Message content.
    #[serde(default)]
    pub content: String,
    /// Tool name.
    #[serde(default)]
    pub tool_name: String,
    /// Raw tool input.
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    /// Tool output.
    #[serde(default)]
    pub tool_output: String,
    /// Occurred-at epoch millis.
    #[serde(default)]
    pub occurred_at: i64,
}

impl MessageItem {
    /// A plain role/content entry.
    pub fn of(seq: i64, role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            seq,
            role: role.into(),
            content: content.into(),
            ..Default::default()
        }
    }

    /// Compact JSON view, omitting empty fields.
    pub fn to_json_value(&self) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        out.insert("seq".into(), self.seq.into());
        out.insert("role".into(), self.role.clone().into());
        out.insert("content".into(), self.content.clone().into());
        if !self.tool_name.is_empty() {
            out.insert("toolName".into(), self.tool_name.clone().into());
        }
        if let Some(input) = &self.tool_input {
            if !input.is_null() {
                out.insert("toolInput".into(), input.clone());
            }
        }
        if !self.tool_output.is_empty() {
            out.insert("toolOutput".into(), self.tool_output.clone().into());
        }
        if self.occurred_at > 0 {
            out.insert("occurredAt".into(), self.occurred_at.into());
        }
        serde_json::Value::Object(out)
    }
}

/// A page of Level-3 full history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePage {
    /// Owning session id.
    pub session_id: String,
    /// The page of messages.
    pub messages: Vec<MessageItem>,
    /// Requested offset.
    pub offset: i64,
    /// Requested limit (`<= 0` means "all remaining").
    pub limit: i64,
    /// Untruncated total count.
    pub total: i64,
}

impl MessagePage {
    /// Slices `all` into a page and reports the untruncated total.
    pub fn of(
        session_id: impl Into<String>,
        all: Vec<MessageItem>,
        offset: i64,
        limit: i64,
    ) -> Self {
        let total = all.len();
        let from = offset.max(0) as usize;
        let to = if limit <= 0 {
            total
        } else {
            (from + limit as usize).min(total)
        };
        let messages = if from >= total {
            Vec::new()
        } else {
            all[from..to].to_vec()
        };
        Self {
            session_id: session_id.into(),
            messages,
            offset,
            limit,
            total: total as i64,
        }
    }

    /// JSON view served by `GET /agentscope/sessions/{id}/messages`.
    pub fn to_json_value(&self) -> serde_json::Value {
        let items: Vec<serde_json::Value> =
            self.messages.iter().map(|m| m.to_json_value()).collect();
        serde_json::json!({
            "sessionId": self.session_id,
            "offset": self.offset,
            "limit": self.limit,
            "total": self.total,
            "messages": items,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_and_reports_total() {
        let all: Vec<MessageItem> = (0..10)
            .map(|i| MessageItem::of(i, "user", format!("m{i}")))
            .collect();
        let page = MessagePage::of("s1", all, 2, 3);
        assert_eq!(page.total, 10);
        assert_eq!(page.messages.len(), 3);
        assert_eq!(page.messages[0].seq, 2);
    }

    #[test]
    fn offset_past_end_yields_empty() {
        let all = vec![MessageItem::of(0, "user", "a")];
        let page = MessagePage::of("s1", all, 5, 10);
        assert!(page.messages.is_empty());
        assert_eq!(page.total, 1);
    }

    #[test]
    fn limit_zero_returns_all_remaining() {
        let all: Vec<MessageItem> = (0..4)
            .map(|i| MessageItem::of(i, "user", format!("m{i}")))
            .collect();
        let page = MessagePage::of("s1", all, 1, 0);
        assert_eq!(page.messages.len(), 3);
    }
}
