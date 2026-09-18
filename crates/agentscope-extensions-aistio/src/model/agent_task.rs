//! Immutable ASDP execution envelope for one durable AgentTask, mirroring
//! `io.agentscope.extensions.aistio.model.AgentTaskAssignment`.

use serde::{Deserialize, Serialize};

/// Execution envelope delivered by the control plane for one AgentTask attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTaskAssignment {
    /// Attempt id.
    pub attempt_id: String,
    /// Durable agent-task id.
    pub agent_task_id: String,
    /// Run id.
    pub run_id: String,
    /// Node id.
    pub node_id: String,
    /// Registered generation.
    pub generation: i64,
    /// Command to execute.
    pub command: String,
    /// URL to fetch the task context from.
    pub context_url: String,
    /// Task-scoped token.
    pub task_token: String,
    /// Attempt-scoped token.
    pub attempt_token: String,
    /// Session id the attempt belongs to.
    pub session_id: String,
    /// Opaque payload (base64 in JSON transport).
    #[serde(default, with = "base64_opt")]
    pub payload: Vec<u8>,
    /// Envelope timestamp, epoch millis.
    pub timestamp: i64,
}

/// Serde adapter encoding `Vec<u8>` as base64 for JSON transport.
mod base64_opt {
    use serde::{Deserialize, Deserializer, Serializer};

    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[(n & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    fn decode(s: &str) -> Result<Vec<u8>, String> {
        let mut rev = [255u8; 256];
        for (i, &c) in ALPHABET.iter().enumerate() {
            rev[c as usize] = i as u8;
        }
        let mut out = Vec::with_capacity(s.len() / 4 * 3);
        let mut buf = 0u32;
        let mut bits = 0u32;
        for c in s.bytes() {
            if c == b'=' {
                continue;
            }
            let v = rev[c as usize];
            if v == 255 {
                return Err(format!("invalid base64 byte: {c}"));
            }
            buf = (buf << 6) | v as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push(((buf >> bits) & 0xff) as u8);
            }
        }
        Ok(out)
    }

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        decode(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_roundtrips_as_base64() {
        let a = AgentTaskAssignment {
            attempt_id: "att1".into(),
            agent_task_id: "task1".into(),
            run_id: "run1".into(),
            node_id: "node1".into(),
            generation: 2,
            command: "run".into(),
            context_url: "http://cp/ctx".into(),
            task_token: "tt".into(),
            attempt_token: "at".into(),
            session_id: "s1".into(),
            payload: vec![1, 2, 3],
            timestamp: 12345,
        };
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains("\"payload\":\"AQID\""));
        let back: AgentTaskAssignment = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
    }
}
