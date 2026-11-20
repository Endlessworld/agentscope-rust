//! The native driver's store: an append-only transcript plus the turns that produced it.
//!
//! This is the half of the seam that does not depend on `agentscope-durable`. It keeps the one
//! property the design asks of it - a write cut short by a crash leaves a last line that a
//! reader drops - without the journal, the leases or the compaction the durable store adds.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use agentscope_core::message::Msg;
use serde::{Deserialize, Serialize};

/// One turn, as the native driver remembers it across restarts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TurnRecord {
    pub(crate) turn_id: String,
    /// A stable name (`running`, `waiting`, `completed`, ...), not the enum: a rename in code
    /// must not silently change what an old file means.
    pub(crate) status: String,
    pub(crate) started_at_ms: i64,
    pub(crate) updated_at_ms: i64,
    pub(crate) stop_reason: Option<String>,
}

impl TurnRecord {
    /// Terminal turns never advance again; a waiting or interrupted one may be resumed.
    pub(crate) fn terminal(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "failed" | "cancelled")
    }
}

/// A session's native history under `<root>/acp-sessions/<session_id>/`.
pub(crate) struct SessionHistory {
    dir: PathBuf,
}

impl SessionHistory {
    /// Opens (creating if needed) the directory a session's history lives in.
    pub(crate) fn open(root: &Path, session_id: &str) -> std::result::Result<Self, String> {
        let dir = root.join("acp-sessions").join(session_id);
        fs::create_dir_all(&dir)
            .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
        Ok(Self { dir })
    }

    fn messages_path(&self) -> PathBuf {
        self.dir.join("messages.jsonl")
    }

    fn turns_path(&self) -> PathBuf {
        self.dir.join("turns.json")
    }

    /// Appends one message as a single line.
    ///
    /// One `write_all` for the line and its newline: a partial line is what a crash leaves, and
    /// [`Self::messages`] is written to drop exactly that.
    pub(crate) fn append_message(&self, message: &Msg) -> std::result::Result<(), String> {
        let line = serde_json::to_string(message).map_err(|error| error.to_string())?;
        let mut buffer = Vec::with_capacity(line.len() + 1);
        buffer.extend_from_slice(line.as_bytes());
        buffer.push(b'\n');
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.messages_path())
            .map_err(|error| error.to_string())?;
        file.write_all(&buffer).map_err(|error| error.to_string())
    }

    /// The transcript.
    ///
    /// A file that does not end in a newline has a last write that was cut short, so that line
    /// is dropped rather than returned as a message that never finished.
    pub(crate) fn messages(&self) -> std::result::Result<Vec<Msg>, String> {
        let text = match fs::read_to_string(self.messages_path()) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.to_string()),
        };
        let body = if text.is_empty() || text.ends_with('\n') {
            text.as_str()
        } else {
            match text.rfind('\n') {
                Some(index) => &text[..=index],
                None => "",
            }
        };
        let mut messages = Vec::new();
        for line in body.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let message = serde_json::from_str::<Msg>(line)
                .map_err(|error| format!("a transcript line does not parse: {error}"))?;
            messages.push(message);
        }
        Ok(messages)
    }

    /// The turns, oldest first.
    pub(crate) fn turns(&self) -> std::result::Result<Vec<TurnRecord>, String> {
        let text = match fs::read_to_string(self.turns_path()) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.to_string()),
        };
        serde_json::from_str(&text).map_err(|error| error.to_string())
    }

    /// Inserts or replaces the record of one turn.
    ///
    /// The file is replaced whole (write a sibling, then rename) so a crash leaves either the
    /// previous version or the new one, never a half-written array.
    pub(crate) fn record_turn(&self, record: TurnRecord) -> std::result::Result<(), String> {
        let mut turns = self.turns()?;
        match turns
            .iter_mut()
            .find(|existing| existing.turn_id == record.turn_id)
        {
            Some(existing) => *existing = record,
            None => turns.push(record),
        }
        let text = serde_json::to_string_pretty(&turns).map_err(|error| error.to_string())?;
        let staging = self.turns_path().with_extension("json.staging");
        fs::write(&staging, text).map_err(|error| error.to_string())?;
        fs::rename(&staging, self.turns_path()).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::MsgRole;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "acp-session-history-{}-{}-{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    fn record(turn_id: &str, status: &str, at: i64) -> TurnRecord {
        TurnRecord {
            turn_id: turn_id.to_string(),
            status: status.to_string(),
            started_at_ms: at,
            updated_at_ms: at,
            stop_reason: None,
        }
    }

    #[test]
    fn an_appended_message_reads_back_in_order() {
        let root = scratch("roundtrip");
        let history = SessionHistory::open(&root, "session-1").expect("open");
        history.append_message(&Msg::user("first")).expect("append");
        history
            .append_message(&Msg::assistant("second"))
            .expect("append");
        let messages = history.messages().expect("read");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role(), MsgRole::User);
        assert_eq!(messages[1].role(), MsgRole::Assistant);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_last_line_without_its_newline_is_dropped() {
        let root = scratch("halfline");
        let history = SessionHistory::open(&root, "session-1").expect("open");
        history.append_message(&Msg::user("kept")).expect("append");
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(history.messages_path())
            .expect("open log");
        file.write_all(b"{\"role\":\"user\",\"content\":[")
            .expect("a cut write");
        drop(file);
        let messages = history.messages().expect("read");
        assert_eq!(messages.len(), 1, "the cut line is not a message");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_turn_is_updated_where_it_already_appears() {
        let root = scratch("turns");
        let history = SessionHistory::open(&root, "session-1").expect("open");
        history
            .record_turn(record("t1", "running", 1))
            .expect("record");
        let mut completed = record("t1", "completed", 1);
        completed.updated_at_ms = 2;
        completed.stop_reason = Some("end_turn".to_string());
        history.record_turn(completed).expect("record");
        history
            .record_turn(record("t2", "waiting", 3))
            .expect("record");
        let turns = history.turns().expect("read");
        assert_eq!(turns.len(), 2, "the second write replaced the first");
        assert_eq!(turns[0].status, "completed");
        assert_eq!(turns[0].stop_reason.as_deref(), Some("end_turn"));
        assert!(turns[0].terminal());
        assert!(!turns[1].terminal(), "a waiting turn may be resumed");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_session_with_no_writes_reads_as_empty() {
        let root = scratch("empty");
        let history = SessionHistory::open(&root, "session-1").expect("open");
        assert!(history.messages().expect("read").is_empty());
        assert!(history.turns().expect("read").is_empty());
        let _ = fs::remove_dir_all(root);
    }
}
