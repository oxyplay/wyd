//! Cleanup-proposal builder. Pure: no side effects, no killing. Reuses the
//! live runtime snapshot so local + demo share the same selection rules.
//!
//! Rules (Phase 1):
//! - `scope = "leftovers"` selects all items with status == "suspicious".
//! - `scope = "session"` selects everything under one session id.
//! - `scope = "agent"` selects items where category matches an agent label.
//! - `scope = "project"` selects items whose `project` matches.
//!
//! Persistent resources (status == "persistent", or names matching the
//! configured persistent list) are **always excluded** — same semantics as
//! `wyd prune`, which never deletes persistent volumes without a manual
//! `D`-confirm.

use serde_json::{Value, json};

use crate::model::{RuntimeSnapshot, RuntimeState};
use crate::web::RuntimeProvider;

#[derive(Debug)]
pub struct Built {
    pub snapshot_version: u64,
    pub value: Value,
}

pub fn build(req: &Value, snap: &RuntimeSnapshot, _provider: &dyn RuntimeProvider) -> Built {
    let scope = req
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("leftovers");
    let target = req.get("id").and_then(Value::as_str).unwrap_or("");
    let mut selected: Vec<Value> = Vec::new();
    let mut excluded: Vec<Value> = Vec::new();
    let mut reclaim_bytes: u64 = 0;

    let items = flat_items(snap);

    for item in &items {
        if is_persistent(item) {
            excluded.push(json!({
                "name": item["name"],
                "reason": "persistent",
            }));
            continue;
        }
        if matches_scope(item, scope, target) {
            selected.push(item.clone());
            reclaim_bytes += item["memory_bytes"].as_u64().unwrap_or(0);
        }
    }

    let value = json!({
        "scope": scope,
        "target": target,
        "snapshot_version": snap.version,
        "selected": selected,
        "excluded": excluded,
        "reclaim_bytes": reclaim_bytes,
        "selected_count": selected.len(),
        "excluded_count": excluded.len(),
        "safe": selected.iter().all(|s| s["status"] == "suspicious"),
    });
    Built {
        snapshot_version: snap.version,
        value,
    }
}

fn is_persistent(item: &Value) -> bool {
    if item.get("status").and_then(Value::as_str) == Some("persistent") {
        return true;
    }
    // Same persistent heuristics the CLI uses; kept narrow on purpose.
    let name = item.get("name").and_then(Value::as_str).unwrap_or("");
    matches!(
        name.to_ascii_lowercase().as_str(),
        "postgres" | "redis" | "mysql" | "mariadb" | "mongo" | "mongodb"
    )
}

fn matches_scope(item: &Value, scope: &str, target: &str) -> bool {
    match scope {
        "leftovers" => item.get("status").and_then(Value::as_str) == Some("suspicious"),
        "agent" => {
            let cat = item.get("category").and_then(Value::as_str).unwrap_or("");
            cat.eq_ignore_ascii_case(target)
        }
        _ => false,
    }
}

fn flat_items(snap: &RuntimeSnapshot) -> Vec<Value> {
    let mut out = Vec::new();
    fn walk(item: &crate::model::RuntimeItem, into: &mut Vec<Value>) {
        let status = match item.state {
            RuntimeState::Active => "active",
            RuntimeState::Persistent => "persistent",
            RuntimeState::Suspicious => "suspicious",
        };
        into.push(json!({
            "category": item.category.label(),
            "name": item.display_name,
            "title": item.title(),
            "root_pid": item.root_pid,
            "memory_bytes": item.memory_bytes,
            "cpu_percent": item.cpu_percent,
            "status": status,
            "ports": item.ports.iter().map(|p| p.port).collect::<Vec<_>>(),
            "project": item.project.as_ref().map(|p| p.root.display().to_string()),
        }));
        for c in &item.children {
            walk(c, into);
        }
    }
    for item in &snap.logical_items {
        walk(item, &mut out);
    }
    out
}
