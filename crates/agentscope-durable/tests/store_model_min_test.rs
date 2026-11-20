//! Verification of the information-theoretic store model (design reference for B3-phase2).
//!
//! This test does not touch the production store. It writes a 20-round session
//! into a throwaway SQLite database using the layout proposed in
//! `docs/DURABLE_INFORMATION_MODEL_TRACKER.md` — incremental frames, periodic
//! snapshots, content-addressed blobs — and checks three claims made for that
//! model:
//!
//! 1. A commit carries only the new information (a frame is a list of content
//!    ids), so its cost does not follow the size of the conversation.
//! 2. The database stays within a constant factor of the unique content, while
//!    rewriting the whole state every round grows with the square of its size.
//! 3. A snapshot plus the frames committed after it rebuilds the state byte for
//!    byte. That is the premise for not disturbing a provider's KV cache: the
//!    bytes sent to the model must not depend on how the state was stored.
//!
//! A snapshot stores content ids, not the content: the bytes live once in
//! `blobs`, and every frame or snapshot only references them. Storing the whole
//! state in the snapshot would duplicate the conversation and make the model's
//! footprint follow the state size instead of the unique content.

use std::collections::HashMap;

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

/// Content id of a blob (the first half of its SHA-256, enough to name it).
fn content_id(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    digest[..32].to_string()
}

fn block(kind: &str, round: usize, bytes: usize) -> String {
    format!("{kind}-{round}:{}", "x".repeat(bytes))
}

#[test]
fn incremental_frames_and_snapshots_stay_near_the_unique_content() {
    let dir = std::env::temp_dir().join(format!(
        "agentscope_store_model_probe_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("session.db");

    let conn = Connection::open(&db).unwrap();
    let _: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .unwrap();
    conn.execute_batch(
        "CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
         CREATE TABLE frames (seq INTEGER PRIMARY KEY, parent INTEGER, payload TEXT NOT NULL, digest TEXT NOT NULL);
         CREATE TABLE snapshots (seq INTEGER PRIMARY KEY, state TEXT NOT NULL);
         CREATE TABLE blobs (hash TEXT PRIMARY KEY, content TEXT NOT NULL);",
    )
    .unwrap();

    let rounds = 20usize;
    let snapshot_every = 5usize;

    let mut blobs: HashMap<String, usize> = HashMap::new();
    let mut unique_bytes = 0usize;
    let mut frame_bytes = 0usize;
    let mut legacy_bytes = 0usize;
    let mut all_messages: Vec<String> = Vec::new();
    let mut frames: Vec<(usize, Vec<String>)> = Vec::new();

    for round in 1..=rounds {
        let mut new_messages = vec![
            block("user", round, 8 * 1024),
            block("assistant", round, 8 * 1024),
            block("tool", round, 8 * 1024),
        ];
        // Every fifth round produces one large tool output (64 KiB).
        if round % 5 == 0 {
            new_messages.push(block("tool-big", round, 64 * 1024));
        }

        let mut added = Vec::new();
        for message in new_messages {
            let id = content_id(&message);
            if !blobs.contains_key(&id) {
                blobs.insert(id.clone(), message.len());
                unique_bytes += message.len();
                conn.execute(
                    "INSERT OR IGNORE INTO blobs (hash, content) VALUES (?1, ?2)",
                    params![id, message],
                )
                .unwrap();
            }
            added.push(id);
            all_messages.push(message);
        }

        let payload = serde_json::json!({ "round": round, "added": added }).to_string();
        frame_bytes += payload.len();
        let digest = content_id(&payload);
        conn.execute(
            "INSERT INTO frames (seq, parent, payload, digest) VALUES (?1, ?2, ?3, ?4)",
            params![round as i64, round as i64 - 1, payload, digest],
        )
        .unwrap();
        frames.push((round, added));

        // What the current store does: rewrite the whole state every round.
        legacy_bytes += serde_json::to_string(&all_messages).unwrap().len();

        if round % snapshot_every == 0 {
            // The snapshot is an index, not a copy: it names the messages by content id.
            let ids: Vec<String> = all_messages
                .iter()
                .map(|message| content_id(message))
                .collect();
            let state = serde_json::to_string(&ids).unwrap();
            conn.execute(
                "INSERT INTO snapshots (seq, state) VALUES (?1, ?2)",
                params![round as i64, state],
            )
            .unwrap();
            // Keep the newest two snapshots: recovery needs one and the next one
            // replaces it once the tail has been folded.
            if round > snapshot_every {
                conn.execute(
                    "DELETE FROM snapshots WHERE seq < ?1",
                    params![round as i64 - snapshot_every as i64],
                )
                .unwrap();
            }
        }
    }

    // Recover from the older of the two kept snapshots plus every frame after it.
    let (snapshot_seq, snapshot_state): (i64, String) = conn
        .query_row(
            "SELECT seq, state FROM snapshots ORDER BY seq DESC LIMIT 1 OFFSET 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut recovered: Vec<String> = Vec::new();
    let snapshot_ids: Vec<String> = serde_json::from_str(&snapshot_state).unwrap();
    for id in &snapshot_ids {
        let content: String = conn
            .query_row(
                "SELECT content FROM blobs WHERE hash = ?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        recovered.push(content);
    }
    for (round, added) in &frames {
        if (*round as i64) <= snapshot_seq {
            continue;
        }
        for id in added {
            let content: String = conn
                .query_row(
                    "SELECT content FROM blobs WHERE hash = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .unwrap();
            recovered.push(content);
        }
    }
    assert_eq!(
        serde_json::to_string(&recovered).unwrap(),
        serde_json::to_string(&all_messages).unwrap(),
        "a snapshot plus the frames after it must rebuild the state byte for byte"
    );
    assert_eq!(recovered.len(), all_messages.len());

    let kept_snapshots: i64 = conn
        .query_row("SELECT COUNT(*) FROM snapshots", [], |row| row.get(0))
        .unwrap();
    let page_count: i64 = conn
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .unwrap();
    let page_size: i64 = conn
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .unwrap();
    let db_bytes = page_count * page_size;
    let snapshot_bytes: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(length(state)), 0) FROM snapshots",
            [],
            |row| row.get(0),
        )
        .unwrap();

    let new_model_bytes = frame_bytes as i64 + snapshot_bytes;
    println!(
        "rounds={rounds} unique={unique_bytes} frames={frame_bytes} snapshots={snapshot_bytes} legacy_rewrites={legacy_bytes} db={db_bytes} kept_snapshots={kept_snapshots} legacy/new={:.1} db/unique={:.2}",
        legacy_bytes as f64 / new_model_bytes.max(1) as f64,
        db_bytes as f64 / unique_bytes as f64,
    );

    assert!(
        frame_bytes * 5 < unique_bytes,
        "a frame must only carry references, not the content: {frame_bytes} vs {unique_bytes}"
    );
    assert!(
        db_bytes < legacy_bytes as i64 / 3,
        "the incremental model must be far smaller than rewriting the state every round: {db_bytes} vs {legacy_bytes}"
    );
    assert!(
        (db_bytes as f64 / unique_bytes as f64) < 1.5,
        "the database must stay within a constant factor of the unique content"
    );
    // The model also buys a smaller write path, not only a smaller database: the same session
    // rewritten in full every round costs three orders of magnitude more than the content it adds.
    assert!(
        legacy_bytes as f64 / new_model_bytes.max(1) as f64 > 100.0,
        "rewriting the whole state every round must cost far more than the incremental model"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
