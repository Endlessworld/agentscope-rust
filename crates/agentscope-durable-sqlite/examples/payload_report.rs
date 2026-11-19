//! Temporary analysis tool: where a session database puts its bytes.
//!
//! Prints, for every journal record, the event type and payload size, then groups the totals by
//! type. That separates the bytes a turn really adds from the bytes a snapshot repeats.

use std::collections::BTreeMap;

use rusqlite::Connection;

fn prefix_of(key: &str) -> String {
    let tail = key.rsplit(':').next().unwrap_or(key);
    if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
        key[..key.len() - tail.len()].to_string()
    } else {
        key.to_string()
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: payload_report <db> [--keys]");
    let dump = std::env::args().any(|arg| arg == "--keys");
    let conn = Connection::open(&path).expect("open");

    let mut rows = Vec::new();
    {
        let mut stmt = conn
            .prepare("SELECT k, v FROM agentscope_part ORDER BY k")
            .unwrap();
        let mut query = stmt.query([]).unwrap();
        while let Some(row) = query.next().unwrap() {
            let key: String = row.get(0).unwrap();
            let value: String = row.get(1).unwrap();
            rows.push((key, value));
        }
    }

    if dump {
        println!();
    }

    let mut total = 0usize;
    let mut by_type: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut by_part: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for (key, value) in &rows {
        total += value.len();
        // A journal entry is the JSON of one `JournalEvent`; the type names its kind.
        let event_type = if key.contains(":journal:") {
            serde_json::from_str::<serde_json::Value>(value)
                .ok()
                .and_then(|event| {
                    ["eventType", "type", "kind"].iter().find_map(|field| {
                        event
                            .get(*field)
                            .and_then(|t| t.as_str())
                            .map(str::to_string)
                    })
                })
                .unwrap_or_else(|| "journal:unparsed".into())
        } else {
            prefix_of(key)
        };
        let entry = by_type.entry(event_type).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += value.len();
        let part = prefix_of(key);
        let entry = by_part.entry(part).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += value.len();
    }

    println!("rows={} stored={} bytes", rows.len(), total);
    println!("\nby event type:");
    let mut types: Vec<_> = by_type.iter().collect();
    types.sort_by_key(|(_, (_, bytes))| std::cmp::Reverse(*bytes));
    for (kind, (count, bytes)) in types {
        println!("{:<44} {:>6} {:>10}", kind, count, bytes);
    }

    println!("\nby key prefix:");
    let mut parts: Vec<_> = by_part.iter().collect();
    parts.sort_by_key(|(_, (_, bytes))| std::cmp::Reverse(*bytes));
    for (part, (count, bytes)) in parts {
        let shown = if part.len() > 90 {
            &part[part.len() - 90..]
        } else {
            part.as_str()
        };
        println!("{:<60} {:>6} {:>10}", shown, count, bytes);
    }

    if dump {
        println!("\nall rows:");
        for (key, value) in &rows {
            println!("{:>9}  {key}", value.len());
        }
    }
}
