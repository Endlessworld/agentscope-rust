//! Temporary analysis tool: what a session database actually holds.
//!
//! Prints per-key byte counts, groups them by key prefix, and measures how much of the stored text
//! is a duplicate of another row. That is the redundancy rate the store model talks about:
//! stored bytes over unique bytes.

use std::collections::{BTreeMap, HashMap};

use rusqlite::Connection;

fn hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: db_report <db> [--keys]");
    let dump_keys = std::env::args().any(|arg| arg == "--keys");
    let conn = Connection::open(&path).expect("open");

    let mut rows = Vec::new();
    {
        let mut stmt = conn
            .prepare("SELECT scope_key, k, v FROM agentscope_part ORDER BY k")
            .unwrap();
        let mut query = stmt.query([]).unwrap();
        while let Some(row) = query.next().unwrap() {
            let scope: String = row.get(0).unwrap();
            let key: String = row.get(1).unwrap();
            let value: String = row.get(2).unwrap();
            rows.push((scope, key, value));
        }
    }

    let mut total = 0usize;
    let mut unique: HashMap<u64, usize> = HashMap::new();
    let mut by_prefix: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for (scope, key, value) in &rows {
        total += value.len();
        unique.entry(hash(value)).or_insert(value.len());
        let prefix: String = scope
            .split(':')
            .next()
            .unwrap_or(scope)
            .trim_end_matches(|c: char| c.is_ascii_digit())
            .to_string();
        let entry = by_prefix.entry(prefix).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += value.len();
        if dump_keys {
            println!("{:>9}  {scope} | {key}", value.len());
        }
    }
    let unique_total: usize = unique.values().sum();
    println!(
        "rows={} stored={} bytes unique={} bytes redundancy={:.2}x",
        rows.len(),
        total,
        unique_total,
        total as f64 / unique_total.max(1) as f64
    );
    println!("{:<28} {:>6} {:>12}", "prefix", "rows", "bytes");
    for (prefix, (count, bytes)) in &by_prefix {
        println!("{:<28} {:>6} {:>12}", prefix, count, bytes);
    }

    // The largest rows usually answer "which record repeats the conversation".
    let mut largest: Vec<&(String, String, String)> = rows.iter().collect();
    largest.sort_by_key(|(_, _, value)| std::cmp::Reverse(value.len()));
    println!("\nlargest rows:");
    for (scope, key, value) in largest.iter().take(12) {
        println!("{:>9}  {scope} | {key}", value.len());
    }
}
