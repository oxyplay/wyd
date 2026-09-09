//! Optional HistoryStore Phase A trace hook.
//!
//! No-op unless env `HISTORYSTORE_TRACE` names a file; then every committed
//! row mutation in `store.rs` is appended there as one trace-schema JSONL
//! line (see historystore phase-a/trace-schema.md). Never changes pragmas,
//! batching, or wyd behavior; write failures are swallowed.

use base64::Engine as _;
use rusqlite::Connection;
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

static TRACE_PATH: LazyLock<Option<PathBuf>> =
    LazyLock::new(|| match std::env::var("HISTORYSTORE_TRACE") {
        Ok(p) if !p.trim().is_empty() => Some(PathBuf::from(p)),
        _ => None,
    });

pub fn enabled() -> bool {
    TRACE_PATH.is_some()
}

pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Lowercase hex for BLOB columns (boot ids, epochs).
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Fetch one row as canonical JSON (sorted keys) by primary key value.
/// `pk` may be text or integer.
pub fn fetch_row(conn: &Connection, table: &str, pk_col: &str, pk: &Value) -> Option<Value> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table} WHERE {pk_col} = ?1"))
        .ok()?;
    let cols: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
    let mut rows = if pk.is_i64() || pk.is_u64() {
        stmt.query(rusqlite::params![pk.as_i64().unwrap_or(0)])
            .ok()?
    } else {
        stmt.query(rusqlite::params![pk.as_str()?]).ok()?
    };
    let row = rows.next().ok()??;
    let mut obj = serde_json::Map::new();
    for (i, name) in cols.iter().enumerate() {
        let v = match row.get_ref(i).ok()? {
            rusqlite::types::ValueRef::Null => Value::Null,
            rusqlite::types::ValueRef::Integer(n) => Value::from(n),
            rusqlite::types::ValueRef::Real(f) => Value::from(f),
            rusqlite::types::ValueRef::Text(t) => {
                Value::from(String::from_utf8_lossy(t).into_owned())
            }
            rusqlite::types::ValueRef::Blob(b) => {
                Value::from(String::from_utf8_lossy(b).into_owned())
            }
        };
        obj.insert(name.clone(), v);
    }
    Some(Value::Object(obj))
}

/// Append one committed row mutation as a trace-schema JSONL line.
/// `old`/`new` are the row values before/after; pass `None` for absent.
pub fn emit(
    table: &str,
    session: &str,
    logical_key: &str,
    op: &str,
    old: Option<&Value>,
    new: Option<&Value>,
) {
    let Some(p) = TRACE_PATH.as_ref() else {
        return;
    };
    let canon = |v: Option<&Value>| -> (u64, Option<String>, Option<String>) {
        match v {
            Some(v) => {
                let bytes = serde_json::to_vec(v).unwrap_or_default();
                (
                    bytes.len() as u64,
                    Some(blake3::hash(&bytes).to_hex().to_string()),
                    Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
                )
            }
            None => (0, None, None),
        }
    };
    let (old_len, hash_old, old_b64) = canon(old);
    let (new_len, hash_new, new_b64) = canon(new);
    let ev = serde_json::json!({
        "ts_unix_us": now_us(),
        "pid": std::process::id(),
        "source": "wyd",
        "session": session,
        "stream": table,
        "kind": table,
        "logical_key": logical_key,
        "op": op,
        "old_len": old_len,
        "new_len": new_len,
        "hash_old": hash_old,
        "hash_new": hash_new,
        "payload_b64": new_b64,
        "payload_old_b64": old_b64,
        "commit": true,
        "commit_source": "hooked",
        "dataset": "unredacted",
    });
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(p) {
        let _ = writeln!(f, "{ev}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_without_env() {
        // env not set in test process unless the runner sets it
        if std::env::var("HISTORYSTORE_TRACE").is_err() {
            assert!(!enabled());
        }
    }

    #[test]
    fn hex_encoding() {
        assert_eq!(hex(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
        assert_eq!(hex(&[]), "");
    }
}
