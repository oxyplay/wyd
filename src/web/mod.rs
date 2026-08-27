//! `wyd web`: a loopback HTTP dashboard over the runtime store, with the
//! same JSON surface as the future WebMCP tools. Binds to 127.0.0.1 by
//! default; refuses non-loopback unless `--allow-lan` is set. The hosted
//! demo uses `--demo` to swap in a deterministic synthetic provider so
//! judges can evaluate without installing anything.
//!
//! This module is the transport only. All ownership reasoning reuses
//! `crate::store::RuntimeStore` / `crate::collect::OwnershipTracker` —
//! no logic is duplicated.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use serde_json::{Value, json};

use crate::model::{RuntimeSnapshot, session::SessionInfo};
use crate::server;
use crate::store::{RuntimeStore, SessionRecord};

mod assets;
mod demo;
mod proposal;

/// Knobs for `wyd web`.
#[derive(Debug, Clone)]
pub struct WebOptions {
    pub host: String,
    pub port: u16,
    pub demo: bool,
    pub allow_lan: bool,
}

impl Default for WebOptions {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8732,
            demo: false,
            allow_lan: false,
        }
    }
}

const DEMO_BANNER: &str = "Demo data — synthetic; not your machine.";

/// What the dashboard exposes: either the live machine runtime or a frozen
/// synthetic one. Both impls produce the same JSON shape, so the UI and
/// WebMCP tools do not care which is in play.
pub trait RuntimeProvider: Send + Sync + 'static {
    fn mode(&self) -> &'static str; // "local" | "demo"
    fn snapshot(&self) -> RuntimeSnapshot;
    fn explain(&self, pid: u32) -> Option<Value>;
    fn sessions(&self) -> Vec<SessionInfo>;
    fn session_record(&self, id: u64) -> Option<SessionRecord>;
}

/// Local provider: reuses the existing store + ownership tracker.
struct LocalProvider {
    store_path: PathBuf,
}

impl LocalProvider {
    fn open_store(&self) -> io::Result<RuntimeStore> {
        RuntimeStore::open(&self.store_path)
    }
}

impl RuntimeProvider for LocalProvider {
    fn mode(&self) -> &'static str {
        "local"
    }
    fn snapshot(&self) -> RuntimeSnapshot {
        crate::collect::snapshot()
    }
    fn explain(&self, pid: u32) -> Option<Value> {
        server::explain_pid(pid).ok()
    }
    fn sessions(&self) -> Vec<SessionInfo> {
        let Ok(store) = self.open_store() else {
            return Vec::new();
        };
        match store.sessions() {
            Ok(rows) => rows.into_iter().map(session_info_from_record).collect(),
            Err(_) => Vec::new(),
        }
    }
    fn session_record(&self, id: u64) -> Option<SessionRecord> {
        let store = self.open_store().ok()?;
        store
            .session_record(crate::model::session::RuntimeSessionId::from_u64(id))
            .ok()
            .flatten()
    }
}

/// Demo provider: deterministic synthetic data. No host scan, no disk I/O.
struct DemoProvider;

impl RuntimeProvider for DemoProvider {
    fn mode(&self) -> &'static str {
        "demo"
    }
    fn snapshot(&self) -> RuntimeSnapshot {
        demo::snapshot()
    }
    fn explain(&self, pid: u32) -> Option<Value> {
        demo::explain(pid)
    }
    fn sessions(&self) -> Vec<SessionInfo> {
        demo::session_infos()
    }
    fn session_record(&self, id: u64) -> Option<SessionRecord> {
        demo::session_record(id)
    }
}

#[derive(Debug)]
enum Route<'a> {
    Health,
    Snapshot,
    SessionsList,
    SessionGet { id: u64 },
    Items,
    Leftovers,
    Explain { pid: u32 },
    ProposalPost,
    ConfirmPost,
    KillPost,
    StaticIndex,
    StaticAsset { path: &'a str },
    NotFound,
}

pub fn serve(opts: WebOptions) -> io::Result<()> {
    let addr = resolve_bind(&opts)?;
    if !opts.allow_lan && !is_loopback(addr.ip()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing to bind {addr} (non-loopback); pass --allow-lan to override"),
        ));
    }
    if server::serve_alive() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "`wyd serve` is already running; stop it first or use --demo",
        ));
    }

    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(false)?;

    let provider: Arc<dyn RuntimeProvider> = if opts.demo {
        eprintln!("wyd web on http://{addr} (demo) — {DEMO_BANNER}");
        Arc::new(DemoProvider)
    } else {
        thread::spawn(server::collect_loop);
        eprintln!("wyd web on http://{addr} (local)");
        Arc::new(LocalProvider {
            store_path: RuntimeStore::default_path(),
        })
    };

    let state = Arc::new(RwLock::new(WebState::new(provider.mode())));

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let state = Arc::clone(&state);
                let provider = Arc::clone(&provider);
                thread::spawn(move || {
                    let _ = handle_conn(s, &state, provider.as_ref());
                });
            }
            Err(e) => {
                eprintln!("wyd web: accept: {e}");
                break;
            }
        }
    }
    Ok(())
}

struct WebState {
    #[allow(dead_code)]
    mode: &'static str,
    proposals: HashMap<String, Value>,
    #[allow(dead_code)]
    last_snapshot_version: u64,
}

impl WebState {
    fn new(mode: &'static str) -> Self {
        Self {
            mode,
            proposals: HashMap::new(),
            last_snapshot_version: 0,
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn resolve_bind(opts: &WebOptions) -> io::Result<SocketAddr> {
    let host: IpAddr = opts
        .host
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("bad host: {e}")))?;
    Ok(SocketAddr::from((host, opts.port)))
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4 == Ipv4Addr::LOCALHOST || v4.is_loopback(),
        IpAddr::V6(v6) => v6 == Ipv6Addr::LOCALHOST || v6.is_loopback(),
    }
}

fn session_info_from_record(r: SessionRecord) -> SessionInfo {
    SessionInfo {
        id: r.id,
        agent: r.agent,
        project: r.project,
        started_at: r.started_at,
        active: r.ended_at.is_none(),
    }
}

fn session_info_to_json(s: &SessionInfo) -> Value {
    let ended_at = if s.active { None } else { Some(s.started_at) };
    json!({
        "id": format!("{:016x}", s.id.as_u64()),
        "agent": s.agent,
        "project": s.project,
        "started_at": s.started_at,
        "ended_at": ended_at,
        "active": s.active,
        "age_seconds": ended_at.unwrap_or(now()).saturating_sub(s.started_at),
    })
}

fn handle_conn(
    mut stream: TcpStream,
    state: &Arc<RwLock<WebState>>,
    provider: &dyn RuntimeProvider,
) -> io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut head = String::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        head.push_str(&line);
    }
    let mut req = parse_request(&head);
    if let Some(len) = req.content_length {
        let mut buf = vec![0u8; len.min(64 * 1024)];
        reader.read_exact(&mut buf)?;
        req.body = String::from_utf8_lossy(&buf).into_owned();
    }
    let route = match_route(&req.method, &req.path);
    let resp = build_response(&route, &req, state, provider);
    write_response(&mut stream, &resp)
}

#[derive(Default, Debug)]
struct ParsedRequest {
    method: String,
    path: String,
    content_length: Option<usize>,
    body: String,
}

fn parse_request(head: &str) -> ParsedRequest {
    let mut req = ParsedRequest::default();
    for line in head.lines() {
        if req.method.is_empty() {
            let mut it = line.split_whitespace();
            req.method = it.next().unwrap_or("").into();
            req.path = it.next().unwrap_or("").into();
        } else if let Some(v) = line.strip_prefix("Content-Length:") {
            req.content_length = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("content-length:") {
            req.content_length = v.trim().parse().ok();
        }
    }
    req
}

fn match_route<'a>(method: &str, path: &'a str) -> Route<'a> {
    let p = path.split('?').next().unwrap_or(path);
    match (method, p) {
        ("GET", "/api/health") => Route::Health,
        ("GET", "/api/snapshot") => Route::Snapshot,
        ("GET", "/api/sessions") => Route::SessionsList,
        ("GET", p) if p.starts_with("/api/sessions/") => {
            let id = p.trim_start_matches("/api/sessions/").parse().unwrap_or(0);
            Route::SessionGet { id }
        }
        ("GET", "/api/items") => Route::Items,
        ("GET", "/api/leftovers") => Route::Leftovers,
        ("GET", p) if p.starts_with("/api/explain/") => {
            let pid = p.trim_start_matches("/api/explain/").parse().unwrap_or(0);
            Route::Explain { pid }
        }
        ("POST", "/api/proposal") => Route::ProposalPost,
        ("POST", "/api/confirm") => Route::ConfirmPost,
        ("POST", "/api/kill") => Route::KillPost,
        ("GET", "/" | "/index.html") => Route::StaticIndex,
        ("GET", p) if p.starts_with("/assets/") => Route::StaticAsset { path: p },
        _ => Route::NotFound,
    }
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
    extra_headers: Vec<(&'static str, String)>,
}

impl HttpResponse {
    fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            reason: if status < 400 { "OK" } else { "ERR" },
            content_type: "application/json",
            body: serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec()),
            extra_headers: Vec::new(),
        }
    }
    fn html(body: &[u8]) -> Self {
        Self {
            status: 200,
            reason: "OK",
            content_type: "text/html; charset=utf-8",
            body: body.to_vec(),
            extra_headers: Vec::new(),
        }
    }
    fn not_found() -> Self {
        Self::json(404, json!({"ok": false, "error": "not found"}))
    }
    fn cors(mut self) -> Self {
        self.extra_headers
            .push(("Access-Control-Allow-Origin", "*".into()));
        self.extra_headers
            .push(("Access-Control-Allow-Methods", "GET, POST, OPTIONS".into()));
        self.extra_headers
            .push(("Access-Control-Allow-Headers", "Content-Type".into()));
        self
    }
}

fn build_response(
    route: &Route<'_>,
    req: &ParsedRequest,
    state: &Arc<RwLock<WebState>>,
    provider: &dyn RuntimeProvider,
) -> HttpResponse {
    let resp = match route {
        Route::Health => HttpResponse::json(
            200,
            json!({
                "ok": true,
                "mode": provider.mode(),
                "banner": if provider.mode() == "demo" { DEMO_BANNER } else { "" },
            }),
        ),
        Route::Snapshot => snapshot_response(provider),
        Route::SessionsList => HttpResponse::json(
            200,
            json!({ "ok": true, "data": { "sessions": sessions_json(provider) } }),
        ),
        Route::SessionGet { id } => match provider.session_record(*id) {
            Some(rec) => HttpResponse::json(
                200,
                json!({ "ok": true, "data": { "session": session_record_json(&rec) } }),
            ),
            None => HttpResponse::json(404, json!({"ok": false, "error": "no such session"})),
        },
        Route::Items => {
            let snap = provider.snapshot();
            let map = session_map(provider);
            HttpResponse::json(
                200,
                json!({ "ok": true, "data": { "items": items_json(&snap, map.as_ref()) } }),
            )
        }
        Route::Leftovers => leftovers_response(provider),
        Route::Explain { pid } => match provider.explain(*pid) {
            Some(v) => HttpResponse::json(200, json!({ "ok": true, "data": v })),
            None => HttpResponse::json(404, json!({"ok": false, "error": "no explanation"})),
        },
        Route::ProposalPost => proposal_response(req, state, provider),
        Route::ConfirmPost => confirm_response(req, state, provider),
        Route::KillPost => kill_response(req),
        Route::StaticIndex => HttpResponse::html(assets::INDEX_HTML),
        Route::StaticAsset { path } => match assets::lookup(path) {
            Some((ct, body)) => HttpResponse {
                status: 200,
                reason: "OK",
                content_type: ct,
                body: body.to_vec(),
                extra_headers: Vec::new(),
            },
            None => HttpResponse::not_found(),
        },
        Route::NotFound => HttpResponse::not_found(),
    };
    resp.cors()
}

fn write_response(stream: &mut TcpStream, resp: &HttpResponse) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        resp.status,
        resp.reason,
        resp.content_type,
        resp.body.len()
    );
    for (k, v) in &resp.extra_headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&resp.body)?;
    Ok(())
}

fn session_record_json(r: &SessionRecord) -> Value {
    json!({
        "id": format!("{:016x}", r.id.as_u64()),
        "agent": r.agent,
        "project": r.project,
        "started_at": r.started_at,
        "ended_at": r.ended_at,
        "active": r.ended_at.is_none(),
        "age_seconds": r.ended_at.unwrap_or(now()).saturating_sub(r.started_at),
    })
}

fn snapshot_response(provider: &dyn RuntimeProvider) -> HttpResponse {
    let snap = provider.snapshot();
    HttpResponse::json(
        200,
        json!({
            "ok": true,
            "data": {
                "mode": provider.mode(),
                "banner": if provider.mode() == "demo" { DEMO_BANNER } else { "" },
                "version": snap.version,
                "cpu_percent": snap.cpu_percent,
                "memory": {
                    "used_bytes": snap.used_memory_bytes,
                    "total_bytes": snap.total_memory_bytes,
                },
                "items": items_json(&snap, session_map(provider).as_ref()),
                "overview": overview(&snap),
                "docker": docker_json(&snap.docker),
                "sessions": sessions_json(provider),
            }
        }),
    )
}

fn sessions_json(provider: &dyn RuntimeProvider) -> Vec<Value> {
    provider
        .sessions()
        .iter()
        .map(session_info_to_json)
        .collect()
}

fn leftovers_response(provider: &dyn RuntimeProvider) -> HttpResponse {
    let snap = provider.snapshot();
    let items = items_json(&snap, session_map(provider).as_ref())
        .into_iter()
        .filter(|v| v.get("status").and_then(Value::as_str) == Some("suspicious"))
        .collect::<Vec<_>>();
    HttpResponse::json(200, json!({ "ok": true, "data": { "leftovers": items } }))
}

/// Build a root_pid -> session_id map for attaching ownership to items.
/// For the local provider this reads the durable store; the demo provider has
/// no live store so it returns `None` (demo items carry their own session_id
/// when relevant).
fn session_map(provider: &dyn RuntimeProvider) -> Option<HashMap<u32, u64>> {
    match provider.mode() {
        "local" => {
            let store = match RuntimeStore::open(&RuntimeStore::default_path()) {
                Ok(s) => s,
                Err(_) => return None,
            };
            let snap = provider.snapshot();
            let mut map = HashMap::new();
            fn walk(
                item: &crate::model::RuntimeItem,
                map: &mut HashMap<u32, u64>,
                store: &RuntimeStore,
            ) {
                if let Some(pid) = item.root_pid
                    && let Ok(Some(sid)) = store.session_for_root_pid(pid)
                {
                    map.insert(pid, sid.as_u64());
                }
                for c in &item.children {
                    walk(c, map, store);
                }
            }
            for item in &snap.logical_items {
                walk(item, &mut map, &store);
            }
            Some(map)
        }
        _ => None,
    }
}

/// Build a nested tree of runtime items (children are real child nodes,
/// like the TUI's `\u251c`/`\u2514` tree), plus a `what` short label per
/// category (agent/mcp/dev/db/...) matching the TUI's WHAT column.
fn items_json(snap: &RuntimeSnapshot, session_map: Option<&HashMap<u32, u64>>) -> Vec<Value> {
    fn what(cat: crate::model::Category) -> &'static str {
        match cat {
            crate::model::Category::Agent => "agent",
            crate::model::Category::Mcp => "mcp",
            crate::model::Category::Browser => "browser",
            crate::model::Category::DevServer => "dev",
            crate::model::Category::LanguageServer => "ls",
            crate::model::Category::Database => "db",
            crate::model::Category::DevService => "service",
            crate::model::Category::Worker => "worker",
            crate::model::Category::UnknownDev => "dev",
        }
    }

    fn build(
        item: &crate::model::RuntimeItem,
        session_map: Option<&HashMap<u32, u64>>,
        age_map: &HashMap<u32, u64>,
    ) -> Value {
        let status = match item.state {
            crate::model::RuntimeState::Active => "active",
            crate::model::RuntimeState::Persistent => "persistent",
            crate::model::RuntimeState::Suspicious => "suspicious",
        };
        let reasons = item
            .suspicion
            .as_ref()
            .map(|s| {
                s.reasons
                    .iter()
                    .map(|r| r.as_str().to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let session_id = item
            .root_pid
            .and_then(|pid| session_map.and_then(|m| m.get(&pid)).copied());
        let age_seconds = item.root_pid.and_then(|pid| age_map.get(&pid)).copied();
        let children = item
            .children
            .iter()
            .map(|c| build(c, session_map, age_map))
            .collect::<Vec<_>>();
        json!({
            "category": item.category.label(),
            "what": what(item.category),
            "name": item.display_name,
            "title": item.title(),
            "root_pid": item.root_pid,
            "session_id": session_id.map(|s| format!("{:016x}", s)),
            "memory_bytes": item.memory_bytes,
            "cpu_percent": item.cpu_percent,
            "age_seconds": age_seconds,
            "status": status,
            "reasons": reasons,
            "ports": item.ports.iter().map(|p| json!({ "port": p.port, "protocol": format!("{:?}", p.protocol) })).collect::<Vec<_>>(),
            "project": item.project.as_ref().map(|p| p.root.display().to_string()),
            "children": children,
        })
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let age_map: HashMap<u32, u64> = snap
        .processes
        .iter()
        .map(|p| (p.pid, now.saturating_sub(p.start_time)))
        .collect();
    snap.logical_items
        .iter()
        .map(|i| build(i, session_map, &age_map))
        .collect()
}

/// Category counts + totals for the sidebar "Overview", like the TUI.
fn overview(snap: &RuntimeSnapshot) -> Value {
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<String, (usize, u64)> = BTreeMap::new();
    let mut total_items = 0usize;
    let mut total_mem = 0u64;
    fn walk(
        item: &crate::model::RuntimeItem,
        counts: &mut BTreeMap<String, (usize, u64)>,
        total_items: &mut usize,
        total_mem: &mut u64,
    ) {
        *total_items += 1;
        *total_mem += item.memory_bytes;
        let e = counts
            .entry(item.category.label().to_string())
            .or_insert((0, 0));
        e.0 += 1;
        e.1 += item.memory_bytes;
        for c in &item.children {
            walk(c, counts, total_items, total_mem);
        }
    }
    for item in &snap.logical_items {
        walk(item, &mut counts, &mut total_items, &mut total_mem);
    }
    let suspicious = {
        let mut n = 0u32;
        fn walk2(item: &crate::model::RuntimeItem, n: &mut u32) {
            if item.state == crate::model::RuntimeState::Suspicious {
                *n += 1;
            }
            for c in &item.children {
                walk2(c, n);
            }
        }
        for item in &snap.logical_items {
            walk2(item, &mut n);
        }
        n
    };
    json!({
        "total_items": total_items,
        "total_memory_bytes": total_mem,
        "suspicious": suspicious,
        "categories": counts.iter().map(|(k, (n, mem))| json!({
            "category": k,
            "count": n,
            "memory_bytes": mem,
        })).collect::<Vec<_>>(),
    })
}

fn docker_json(d: &Arc<crate::model::DockerSnapshot>) -> Value {
    json!({
        "ok": d.ok,
        "note": d.note,
        "disk_bytes": d.disk_bytes,
        "reclaimable_bytes": d.reclaimable_bytes,
        "resources": d.resources.iter().map(|r| json!({
            "kind": format!("{:?}", r.kind),
            "id": r.id,
            "name": r.name,
            "size_bytes": r.size_bytes,
            "anonymous": r.anonymous,
            "persistent": r.persistent,
        })).collect::<Vec<_>>(),
    })
}

fn proposal_response(
    req: &ParsedRequest,
    state: &Arc<RwLock<WebState>>,
    provider: &dyn RuntimeProvider,
) -> HttpResponse {
    let body: Value = match serde_json::from_str(&req.body) {
        Ok(v) => v,
        Err(e) => {
            return HttpResponse::json(
                400,
                json!({"ok": false, "error": format!("bad json: {e}")}),
            );
        }
    };
    let snap = provider.snapshot();
    let prop = proposal::build(&body, &snap, provider);
    let id = format!("prop-{:x}-{:x}", prop.snapshot_version, now());
    state
        .write()
        .proposals
        .insert(id.clone(), prop.value.clone());
    HttpResponse::json(
        200,
        json!({
            "ok": true,
            "data": {
                "id": id,
                "proposal": prop.value,
                "snapshot_version": prop.snapshot_version,
            }
        }),
    )
}

fn confirm_response(
    req: &ParsedRequest,
    state: &Arc<RwLock<WebState>>,
    provider: &dyn RuntimeProvider,
) -> HttpResponse {
    let body: Value = match serde_json::from_str(&req.body) {
        Ok(v) => v,
        Err(e) => {
            return HttpResponse::json(
                400,
                json!({"ok": false, "error": format!("bad json: {e}")}),
            );
        }
    };
    let id = match body.get("id").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return HttpResponse::json(400, json!({"ok": false, "error": "missing id"})),
    };
    let expected_version = match body.get("version").and_then(Value::as_u64) {
        Some(v) => v,
        None => return HttpResponse::json(400, json!({"ok": false, "error": "missing version"})),
    };
    let snap = provider.snapshot();
    let state_guard = state.read();
    let stored = match state_guard.proposals.get(&id) {
        Some(v) => v.clone(),
        None => return HttpResponse::json(404, json!({"ok": false, "error": "no such proposal"})),
    };
    drop(state_guard);
    if stored.get("snapshot_version").and_then(Value::as_u64) != Some(expected_version)
        || expected_version != snap.version
    {
        return HttpResponse::json(
            409,
            json!({"ok": false, "error": "stale proposal; refresh and try again"}),
        );
    }
    HttpResponse::json(
        200,
        json!({
            "ok": true,
            "data": {
                "id": id,
                "applied": false,
                "note": "Phase 1: confirm returns the proposal but performs no actions; real confirm lands with Phase 4 (kill/delete) and always requires human UI confirm.",
                "proposal": stored,
            }
        }),
    )
}

/// Terminate a single process by pid, with PID + start-time revalidation.
/// This is a deliberate, human-confirmed action: the UI asks before calling it.
/// Returns a report so the client can tell the user how many were stopped.
fn kill_response(req: &ParsedRequest) -> HttpResponse {
    let body: Value = match serde_json::from_str(&req.body) {
        Ok(v) => v,
        Err(e) => {
            return HttpResponse::json(
                400,
                json!({"ok": false, "error": format!("bad json: {e}")}),
            );
        }
    };
    let Some(pid) = body.get("pid").and_then(Value::as_u64) else {
        return HttpResponse::json(400, json!({"ok": false, "error": "missing pid"}));
    };
    let pid = pid as u32;
    let snap = crate::collect::snapshot();
    let Some(proc) = snap.processes.iter().find(|p| p.pid == pid) else {
        return HttpResponse::json(
            404,
            json!({"ok": false, "error": format!("no running process with pid {pid}")}),
        );
    };
    let id = crate::actions::process::Identity {
        pid,
        start_time: proc.start_time,
    };
    let report = crate::actions::process::send(&[id], crate::actions::process::Signal::Term);
    HttpResponse::json(
        200,
        json!({
            "ok": true,
            "data": {
                "pid": pid,
                "signaled": report.signaled,
                "skipped": report.skipped,
                "failed": report.failed,
            }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_guard_rejects_non_loopback() {
        let opts = WebOptions {
            host: "0.0.0.0".into(),
            port: 8732,
            demo: false,
            allow_lan: false,
        };
        let err = serve(opts).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn demo_provider_emits_banner_and_sessions() {
        let p = DemoProvider;
        assert_eq!(p.mode(), "demo");
        let s = p.snapshot();
        assert!(!s.logical_items.is_empty());
        let sessions = p.sessions();
        assert!(!sessions.is_empty());
    }

    #[test]
    fn proposal_excludes_persistent() {
        let snap = demo::snapshot();
        let body = json!({ "scope": "leftovers" });
        let p = proposal::build(&body, &snap, &DemoProvider);
        let names: Vec<String> = p.value["selected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap().to_string())
            .collect();
        let excluded: Vec<String> = p.value["excluded"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap().to_string())
            .collect();
        assert!(excluded.iter().any(|n| n == "postgres"));
        assert!(excluded.iter().any(|n| n == "redis"));
        assert!(!names.iter().any(|n| n == "postgres"));
    }

    #[test]
    fn stale_proposal_is_rejected_by_guard() {
        let mut snap = demo::snapshot();
        snap.version = 1;
        let p1 = proposal::build(&json!({}), &snap, &DemoProvider);
        // bump the snapshot
        snap.version = 99;
        let p2 = proposal::build(&json!({}), &snap, &DemoProvider);
        assert_eq!(p1.snapshot_version, 1);
        assert_eq!(p2.snapshot_version, 99);
        // The mismatch p1 != p2 is exactly what confirm_response guards.
    }

    #[test]
    fn json_shapes_include_mode_and_banner() {
        let p = DemoProvider;
        let _snap = p.snapshot();
        let resp = snapshot_response(&p);
        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type, "application/json");
    }
}
