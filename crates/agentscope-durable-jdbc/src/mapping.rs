//! The mapping layer: how a `RecordStore` scope becomes rows, and how a page becomes a query.
//!
//! Nothing here talks to a server, and that is the point: every function returns SQL plus the
//! exact parameter values it expects, so what a driver will send is a value a test can assert on.
//! The driver itself is a small `impl SqlSession` and holds no decisions — scope encoding, prefix
//! bounds, cursors, page limits, value encoding and the compare-and-set token are all decided
//! here, where they can be tested without a database.

use agentscope_durable::{DurableError, Result};
use serde_json::Value;

use crate::statements as sql;

/// The longest scope key the schema accepts (`agentscope_scope_key_len`).
pub const MAX_SCOPE_CHARS: usize = 256;

/// The largest page one query will ask for.
///
/// A page is a round trip. Asking for more than this is a caller that meant to stream, and the cap
/// keeps one query from turning into a whole-history read on a store the whole cluster shares.
/// It matches the backend's own journal page, so the two never disagree about what a page is.
pub const MAX_PAGE: usize = 1000;

/// One bound parameter, in the shape a driver needs to bind it.
///
/// The variants exist because the statements need exactly these types, not because a driver is
/// missing: `jsonb` is sent as JSON text and cast in the statement, so a payload is serialised
/// once and the store never holds a second representation of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Param {
    Text(String),
    BigInt(i64),
    /// A `text[]`; `k = ANY($n::text[])` is how several records are read in one round trip.
    TextArray(Vec<String>),
    /// SQL `NULL`. Only a `jsonb` column takes one, and only an absent work payload uses it.
    Null,
}

impl Param {
    /// The cast a driver applies to the placeholder this parameter is bound to.
    pub fn cast(&self) -> &'static str {
        match self {
            Param::Text(_) => "text",
            Param::BigInt(_) => "bigint",
            Param::TextArray(_) => "text[]",
            Param::Null => "unknown",
        }
    }
}

/// A statement with the parameters that belong to it.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub sql: &'static str,
    pub params: Vec<Param>,
}

/// The scope key a scope name maps to, after checking it against the schema's constraint.
///
/// The scope string is used as it is — no hash, no escape — because it is a row identity the
/// whole cluster can read in a query, and one scope per session is far from the size at which
/// hashing it into a shorter key would matter. What the schema cannot hold is rejected here, with
/// a message that names the reason, instead of surfacing as a driver error.
pub fn scope_key(scope: &str) -> Result<&str> {
    if scope.is_empty() {
        return Err(DurableError::invalid("scope key must not be empty"));
    }
    if scope.contains('\u{0}') {
        return Err(DurableError::invalid(
            "scope key must not contain U+0000: PostgreSQL text cannot store it",
        ));
    }
    if scope.chars().count() > MAX_SCOPE_CHARS {
        return Err(DurableError::invalid(format!(
            "scope key must be at most {MAX_SCOPE_CHARS} characters"
        )));
    }
    Ok(scope)
}

/// The part key a `Txn` record maps to, after checking that a row can hold it.
///
/// The in-memory and file stores keep every record in one document, so `\u{0}` was only ever an
/// internal separator there. In a `text` column it cannot be stored at all, and a key that would
/// have to be silently rewritten must be a refusal instead: the key is what a later read asks for.
pub fn part_key(part: &str) -> Result<&str> {
    if part.contains('\u{0}') {
        return Err(DurableError::invalid(
            "part key must not contain U+0000: PostgreSQL text cannot store it",
        ));
    }
    Ok(part)
}

/// A page bound (a prefix or a cursor) as the text a statement compares against.
///
/// A bound is not a stored key: an empty prefix means "every key of the scope" and an empty
/// cursor means "the start", so neither is refused. Only `U+0000` is, because it cannot be bound
/// as `text` at all — and it could never match a stored key either.
fn bound<'a>(value: &'a str, what: &str) -> Result<&'a str> {
    if value.contains('\u{0}') {
        return Err(DurableError::invalid(format!(
            "{what} must not contain U+0000: PostgreSQL text cannot store it"
        )));
    }
    Ok(value)
}

/// The exclusive upper bound of every key that starts with `prefix`; `None` when there is none.
///
/// The bound is what makes a prefix page a plain b-tree range:
/// `k >= prefix AND k < bound` scans exactly the keys under the prefix, with no `LIKE` and no
/// escape rules. UTF-8 byte order equals code point order, and `COLLATE "C"` orders text by
/// bytes, so incrementing the last character of the prefix gives the same edge in PostgreSQL as
/// it does here. An empty prefix has no upper bound, and a prefix that ends in `U+10FFFF` has to
/// borrow the edge of its parent: `"ab\u{10FFFF}"` is bounded by `"ac"`.
pub fn prefix_bound(prefix: &str) -> Option<String> {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(last) = chars.pop() {
        if let Some(next) = successor(last) {
            chars.push(next);
            return Some(chars.into_iter().collect());
        }
    }
    None
}

/// The next character in code point order, skipping the surrogate range a `char` cannot enter.
///
/// `U+D7FF + 1` would be a surrogate, which no UTF-8 string contains, so the next text that sorts
/// above every string ending in `U+D7FF` ends in `U+E000`.
fn successor(last: char) -> Option<char> {
    let code = last as u32;
    let next = if code == 0xD7FF { 0xE000 } else { code + 1 };
    char::from_u32(next)
}

/// The page limit a query asks for: capped, never rejected.
///
/// The other two stores truncate a page rather than fail it, so a caller that asks for more than
/// [`MAX_PAGE`] gets a page instead of an error it would have to handle on a path that never
/// intended to be wrong. `0` stays `0`, which is how a caller reads "nothing".
pub fn page_limit(limit: usize) -> i64 {
    limit.min(MAX_PAGE) as i64
}

/// The keys of one scope under `prefix` after `after`, as one statement and its parameters.
///
/// Four statements rather than one with `($3 IS NULL OR k > $3)`: a disjunction over the cursor
/// is a filter applied *after* the index scan, which turns paging a long journal into a rescan of
/// its prefix for every page — the quadratic read this schema exists to avoid. The cursor is
/// chosen here instead, so each statement has the cursor as a real range bound.
///
/// A cursor that sorts below the prefix is ignored, exactly as the in-memory store ignores it: it
/// cannot be a position inside this prefix, so the page starts at the prefix.
pub fn keys_page(scope: &str, prefix: &str, after: Option<&str>, limit: usize) -> Result<Page> {
    let scope_key = scope_key(scope)?;
    let prefix = bound(prefix, "prefix")?;
    let cursor = match after {
        Some(after) if after >= prefix => Some(bound(after, "cursor")?),
        _ => None,
    };
    let bound = prefix_bound(prefix);
    let limit = Param::BigInt(page_limit(limit));
    let scope_param = Param::Text(scope_key.to_string());
    let page = match (cursor, bound) {
        (None, None) => Page {
            sql: sql::KEYS_FROM_PREFIX,
            params: vec![scope_param, Param::Text(prefix.to_string()), limit],
        },
        (None, Some(bound)) => Page {
            sql: sql::KEYS_FROM_PREFIX_BOUNDED,
            params: vec![
                scope_param,
                Param::Text(prefix.to_string()),
                Param::Text(bound),
                limit,
            ],
        },
        (Some(cursor), None) => Page {
            sql: sql::KEYS_AFTER,
            params: vec![scope_param, Param::Text(cursor.to_string()), limit],
        },
        (Some(cursor), Some(bound)) => Page {
            sql: sql::KEYS_AFTER_BOUNDED,
            params: vec![
                scope_param,
                Param::Text(cursor.to_string()),
                Param::Text(bound),
                limit,
            ],
        },
    };
    Ok(page)
}

/// The statement that advances a scope's revision from the one its transaction read.
pub fn cas_bump(scope: &str, expected_revision: i64, now: i64) -> Page {
    Page {
        sql: sql::BUMP_REVISION,
        params: vec![
            Param::Text(scope.to_string()),
            Param::BigInt(expected_revision),
            Param::BigInt(now),
        ],
    }
}

/// Interprets the row count of [`cas_bump`]: exactly one row is the successful case.
///
/// The scope row lock makes a miss impossible while it is held, so a miss means the assumption
/// the lock rests on did not hold — a statement issued outside its transaction, a connection that
/// committed early, or a writer this backend does not know about. The answer is a conflict, not a
/// silent lost update: this transaction's writes are rolled back and the caller retries against
/// the newer revision.
pub fn verify_cas(updated: u64) -> Result<()> {
    match updated {
        1 => Ok(()),
        0 => Err(DurableError::conflict(
            "scope revision moved during the transaction; retry against the newer revision",
        )),
        other => Err(DurableError::invalid(format!(
            "scope revision compare-and-set touched {other} rows"
        ))),
    }
}

/// A `Value` as the JSON text bound to a `jsonb` column.
///
/// # The one lossy point
///
/// `jsonb` cannot hold `U+0000` inside a string — it is the one escape the type refuses. Silently
/// replacing it would corrupt a reply, and storing the record as `text` would cost the operators
/// this schema exists for, so the value is refused here with a message naming the reason. A caller
/// that sees this error has a value that no PostgreSQL deployment can hold; the file and in-memory
/// stores keep it, which is why the check lives in this crate.
pub fn encode(value: &Value) -> Result<String> {
    check_storable(value)?;
    serde_json::to_string(value).map_err(|error| {
        DurableError::invalid(format!("value is not serialisable as JSON: {error}"))
    })
}

fn check_storable(value: &Value) -> Result<()> {
    match value {
        Value::String(text) if text.contains('\u{0}') => Err(unstorable()),
        Value::Array(items) => items.iter().try_for_each(check_storable),
        Value::Object(fields) => {
            for (name, value) in fields {
                if name.contains('\u{0}') {
                    return Err(unstorable());
                }
                check_storable(value)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn unstorable() -> DurableError {
    DurableError::invalid(
        "value contains U+0000, which a jsonb column cannot store; the record was refused \
         rather than rewritten",
    )
}

/// The `Value` a `jsonb` column answers with.
pub fn decode(text: &str) -> Result<Value> {
    serde_json::from_str(text)
        .map_err(|error| DurableError::invalid(format!("stored jsonb is not valid JSON: {error}")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ErrorCode;

    #[test]
    fn a_scope_key_the_schema_accepts_is_used_unchanged() {
        assert_eq!(
            scope_key("ns:thread:main").expect("scope"),
            "ns:thread:main"
        );
        assert_eq!(scope_key("a").expect("single char"), "a");
        let longest = "x".repeat(MAX_SCOPE_CHARS);
        assert_eq!(scope_key(&longest).expect("longest"), longest);
    }

    #[test]
    fn a_scope_key_the_schema_rejects_is_refused_with_its_reason() {
        assert_eq!(scope_key("").expect_err("empty").code(), ErrorCode::Invalid);
        let nul = scope_key("a\u{0}b").expect_err("nul");
        assert!(nul.message().contains("U+0000"), "{nul}");
        let long = "x".repeat(MAX_SCOPE_CHARS + 1);
        let error = scope_key(&long).expect_err("too long");
        assert!(error.message().contains("at most 256"), "{error}");
    }

    #[test]
    fn a_part_key_is_a_row_identity_and_must_be_storable() {
        assert_eq!(part_key("turn:t1").expect("part"), "turn:t1");
        // Any key the other two stores accept is accepted here too; only what a `text` column
        // cannot hold is refused.
        assert_eq!(part_key("").expect("empty"), "");
        assert_eq!(
            part_key("a\u{0}b").expect_err("nul").code(),
            ErrorCode::Invalid
        );
    }

    #[test]
    fn an_empty_cursor_asks_for_the_start_of_the_prefix() {
        // The backend pages a queue with a cursor it holds, and it may hold the empty one: below
        // this prefix, so the page starts at the prefix — the same answer the in-memory store
        // gives for a cursor that is not inside the range.
        let page = keys_page("s:1", "turn:", Some(""), 5).expect("page");
        assert_eq!(page.sql, sql::KEYS_FROM_PREFIX_BOUNDED);
        assert_eq!(page.params[1], Param::Text("turn:".to_string()));
        // With no prefix, the empty cursor is the start of the whole scope.
        let page = keys_page("s:1", "", Some(""), 5).expect("page");
        assert_eq!(page.sql, sql::KEYS_AFTER);
        assert_eq!(page.params[1], Param::Text(String::new()));
    }

    #[test]
    fn a_bound_that_no_text_column_could_hold_is_refused() {
        assert_eq!(
            keys_page("s:1", "a\u{0}", None, 5)
                .expect_err("nul prefix")
                .code(),
            ErrorCode::Invalid
        );
        assert_eq!(
            keys_page("s:1", "a", Some("a\u{0}"), 5)
                .expect_err("nul cursor")
                .code(),
            ErrorCode::Invalid
        );
    }

    #[test]
    fn the_prefix_bound_is_the_exclusive_edge_of_the_prefix() {
        assert_eq!(prefix_bound("abc").as_deref(), Some("abd"));
        assert_eq!(prefix_bound("journal:").as_deref(), Some("journal;"));
        // `9` is followed by `:` in code point order, not by `0`.
        assert_eq!(prefix_bound("9").as_deref(), Some(":"));
        assert_eq!(prefix_bound("z").as_deref(), Some("{"));
        // `U+D7FF` is followed by `U+E000`: the code points between them are surrogates, which no
        // UTF-8 string can contain.
        assert_eq!(prefix_bound("\u{d7ff}").as_deref(), Some("\u{e000}"));
        // The last code point has no successor, so the edge comes from its parent.
        assert_eq!(prefix_bound("ab\u{10ffff}").as_deref(), Some("ac"));
        assert_eq!(prefix_bound("\u{10ffff}"), None);
        assert_eq!(prefix_bound(""), None);
    }

    #[test]
    fn a_key_is_in_the_range_exactly_when_it_starts_with_the_prefix() {
        let prefixes = ["a", "journal:abc-", "turn:t", "", "\u{e9}"];
        let keys = [
            "a",
            "ab",
            "b",
            "journal:abc-0",
            "journal:abc-z",
            "journal:abd",
            "journal:",
            "turn:t1",
            "turn:u1",
            "\u{e9}x",
            "\u{e8}x",
        ];
        for prefix in prefixes {
            let bound = prefix_bound(prefix);
            for key in keys {
                let in_range = key >= prefix && bound.as_deref().map_or(true, |bound| key < bound);
                assert_eq!(
                    in_range,
                    key.starts_with(prefix),
                    "prefix {prefix:?}, key {key:?}, bound {bound:?}"
                );
            }
        }
    }

    #[test]
    fn a_page_without_a_cursor_starts_at_the_prefix() {
        let page = keys_page("s:1", "journal:", None, 50).expect("page");
        assert_eq!(page.sql, sql::KEYS_FROM_PREFIX_BOUNDED);
        assert_eq!(
            page.params,
            vec![
                Param::Text("s:1".to_string()),
                Param::Text("journal:".to_string()),
                Param::Text("journal;".to_string()),
                Param::BigInt(50),
            ]
        );
    }

    #[test]
    fn a_cursor_is_an_exclusive_range_bound() {
        let page = keys_page("s:1", "journal:", Some("journal:8"), 10).expect("page");
        assert_eq!(page.sql, sql::KEYS_AFTER_BOUNDED);
        assert_eq!(page.params[1], Param::Text("journal:8".to_string()));
        assert_eq!(page.params[2], Param::Text("journal;".to_string()));
        assert!(
            !page.sql.contains("IS NULL"),
            "the cursor must be a bound, not a filter: {}",
            page.sql
        );
    }

    #[test]
    fn a_cursor_below_the_prefix_is_ignored_like_the_in_memory_store_ignores_it() {
        let page = keys_page("s:1", "m", Some("a"), 5).expect("page");
        assert_eq!(page.sql, sql::KEYS_FROM_PREFIX_BOUNDED);
        assert_eq!(page.params[1], Param::Text("m".to_string()));
    }

    #[test]
    fn a_cursor_at_or_above_the_upper_bound_answers_nothing() {
        // The bound is `n`, so a cursor of `n` selects an empty range: the statement is still the
        // cursor form, and the server answers no rows without scanning.
        let page = keys_page("s:1", "m", Some("n"), 5).expect("page");
        assert_eq!(page.sql, sql::KEYS_AFTER_BOUNDED);
        assert_eq!(page.params[1], Param::Text("n".to_string()));
        assert_eq!(page.params[2], Param::Text("n".to_string()));
    }

    #[test]
    fn an_empty_prefix_has_no_upper_bound() {
        assert_eq!(
            keys_page("s:1", "", None, 5).expect("page").sql,
            sql::KEYS_FROM_PREFIX
        );
        assert_eq!(
            keys_page("s:1", "", Some("turn:t1"), 5).expect("page").sql,
            sql::KEYS_AFTER
        );
    }

    #[test]
    fn a_page_limit_is_capped_and_never_rejected() {
        assert_eq!(page_limit(0), 0);
        assert_eq!(page_limit(1), 1);
        assert_eq!(page_limit(MAX_PAGE), MAX_PAGE as i64);
        assert_eq!(page_limit(MAX_PAGE * 5), MAX_PAGE as i64);
        let page = keys_page("s:1", "k", None, MAX_PAGE * 5).expect("page");
        assert_eq!(page.params.last(), Some(&Param::BigInt(MAX_PAGE as i64)));
    }

    #[test]
    fn the_cas_bump_carries_the_revision_the_transaction_read() {
        let page = cas_bump("ns:thread:main", 7, 1_700_000_000_000);
        assert_eq!(page.sql, sql::BUMP_REVISION);
        assert_eq!(
            page.params,
            vec![
                Param::Text("ns:thread:main".to_string()),
                Param::BigInt(7),
                Param::BigInt(1_700_000_000_000),
            ]
        );
        assert!(page.sql.contains("AND revision = $2"), "{}", page.sql);
    }

    #[test]
    fn a_compare_and_set_that_moved_no_row_is_a_conflict() {
        assert!(verify_cas(1).is_ok());
        assert_eq!(
            verify_cas(0).expect_err("conflict").code(),
            ErrorCode::Conflict
        );
        assert_eq!(
            verify_cas(2).expect_err("impossible").code(),
            ErrorCode::Invalid
        );
    }

    #[test]
    fn a_value_round_trips_through_its_jsonb_text() {
        let values = [
            Value::Null,
            json!({}),
            json!(""),
            json!([1, 2.5, -0.0, true, null]),
            json!({ "text": "会话", "nested": { "list": [ { "k": "v" } ] } }),
            json!({ "control": "line\nbreak\ttab", "quote": "a\"b" }),
            json!({ "big": 9007199254740993i64 }),
        ];
        for value in values {
            let encoded = encode(&value).expect("encode");
            assert_eq!(decode(&encoded).expect("decode"), value, "{encoded}");
        }
    }

    #[test]
    fn a_nul_in_a_value_is_refused_before_a_driver_sees_it() {
        // Top level, nested, inside an array, inside a key: `jsonb` refuses every one, and a
        // refusal that names the reason beats a driver error at the bottom of a commit.
        let values = [
            json!("a\u{0}b"),
            json!({ "text": "a\u{0}b" }),
            json!(["ok", "a\u{0}b"]),
            json!({ "a\u{0}b": 1 }),
        ];
        for value in values {
            let error = encode(&value).expect_err("NUL is unstorable");
            assert_eq!(error.code(), ErrorCode::Invalid);
            assert!(error.message().contains("U+0000"), "{error}");
        }
        // A value that merely looks like one is fine.
        assert!(encode(&json!({ "escaped": "\\u0000" })).is_ok());
    }

    #[test]
    fn stored_text_that_is_not_json_is_reported_as_such() {
        let error = decode("not json").expect_err("invalid");
        assert_eq!(error.code(), ErrorCode::Invalid);
        assert!(error.message().contains("jsonb"), "{error}");
    }
}
