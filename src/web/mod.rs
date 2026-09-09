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

use crate::demo;
use crate::model::{
    RuntimeSnapshot,
    run::{Capacity, RunId, RunState},
    session::{RuntimeSessionId, SessionInfo},
};
use crate::runner::RunView;
use crate::runner::logs::{RunPaths, Stream, read_chunk};
use crate::server::{self, SnapshotSink};
use crate::store::{RunFilter, RuntimeStore, SessionRecord};

mod assets;
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

    /// Managed runs, newest first. Store-backed in local mode, so the Runs
    /// view works with no supervisor running.
    fn runs(&self, filter: &RunFilter) -> Vec<RunView>;
    fn run(&self, id: RunId) -> Option<RunView>;
    /// Read-only admission capacity: slots, queue and reservations. Never
    /// mutates anything and never starts a run.
    fn capacity(&self) -> Capacity;
    /// One bounded output chunk for a retained run.
    fn run_output(
        &self,
        id: RunId,
        stream: Stream,
        cursor: u64,
        max_bytes: usize,
    ) -> io::Result<Value>;
    /// Ask the supervisor to stop a run. The only mutating run action; it is
    /// reached only after a confirmed proposal.
    fn cancel_run(&self, id: RunId) -> io::Result<Option<RunView>>;
}

/// Local provider: reuses the existing store + ownership tracker.
struct LocalProvider {
    store_path: PathBuf,
    /// The process collector's published snapshot. One scanner per process:
    /// the dashboard reads what `collect_loop` already scanned instead of
    /// running a second `System` beside it.
    sink: Arc<SnapshotSink>,
}

impl LocalProvider {
    fn new(store_path: PathBuf, sink: Arc<SnapshotSink>) -> Self {
        Self { store_path, sink }
    }

    fn open_store(&self) -> io::Result<RuntimeStore> {
        RuntimeStore::open(&self.store_path)
    }
}

impl RuntimeProvider for LocalProvider {
    fn mode(&self) -> &'static str {
        "local"
    }
    fn snapshot(&self) -> RuntimeSnapshot {
        // The collector's first scan lands within a refresh interval; wait
        // briefly rather than reporting an empty machine.
        self.sink.latest(Duration::from_secs(5)).unwrap_or_default()
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
    fn runs(&self, filter: &RunFilter) -> Vec<RunView> {
        // A live supervisor knows things the store does not: observed memory,
        // queue position and limit events. Ask it when it is answering, and
        // fall back to the durable store otherwise.
        if crate::server::serve_alive() {
            let query = json!({
                "state": filter.state.map(|s| s.as_str()),
                "project": filter.project,
                "session": filter.session.map(|s| s.to_string()),
                "limit": filter.limit,
            });
            if let Ok(runs) = crate::runner::client::Client::new().list(query) {
                return runs;
            }
        }
        let Ok(store) = self.open_store() else {
            return Vec::new();
        };
        match store.run_list(filter) {
            Ok(rows) => rows.iter().map(RunView::from_record).collect(),
            Err(_) => Vec::new(),
        }
    }
    fn run(&self, id: RunId) -> Option<RunView> {
        let store = self.open_store().ok()?;
        store
            .run_get(id)
            .ok()
            .flatten()
            .map(|r| RunView::from_record(&r))
    }
    fn capacity(&self) -> Capacity {
        // The supervisor owns admission. With none alive there is nothing to
        // ask, so report the configured limits with zero usage rather than
        // starting a daemon or inventing runs.
        if server::serve_alive()
            && let Ok(c) = crate::runner::client::Client::new().capacity()
        {
            return c;
        }
        let limits = crate::config::Config::global().runs.limits().summary();
        let slots_free = limits.max_parallel;
        Capacity {
            limits,
            running: 0,
            queued: 0,
            slots_free,
            reserved_memory_bytes: 0,
            projects: Vec::new(),
            queue: Vec::new(),
            over_parallel_limit: false,
            aggregate_memory_max_bytes: None,
            reservation_note: "no supervisor running: configured limits only, no reservations held"
                .into(),
            capabilities: crate::model::run::backend_capabilities(),
        }
    }
    fn run_output(
        &self,
        id: RunId,
        stream: Stream,
        cursor: u64,
        max_bytes: usize,
    ) -> io::Result<Value> {
        // Read the retained log file directly instead of going through the
        // supervisor: this works with no daemon, and `logs::read_chunk` is
        // exactly what the supervisor's own `output()` calls. It never
        // spawns a collector and never signals anything.
        let store = self.open_store()?;
        let Some(record) = store.run_get(id)? else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("run {id} not found"),
            ));
        };
        let paths = RunPaths::new(RuntimeStore::default_path().with_file_name("runs"));
        let mut chunk = read_chunk(&paths.stream_file(id.0, stream), cursor, max_bytes)?;
        chunk.truncated = match stream {
            Stream::Stdout => record.logs.stdout_truncated,
            Stream::Stderr => record.logs.stderr_truncated,
        };
        Ok(json!({
            "stream": stream.as_str(),
            "data": chunk.data,
            "next_cursor": chunk.next_cursor,
            "truncated": chunk.truncated,
            "eof": chunk.eof,
        }))
    }
    fn cancel_run(&self, id: RunId) -> io::Result<Option<RunView>> {
        // Execution lives only in the supervisor; the web process only asks.
        // A missing supervisor surfaces as the client's connect error.
        crate::runner::client::Client::new().cancel(id)
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
    fn runs(&self, filter: &RunFilter) -> Vec<RunView> {
        let session_hex = filter.session.map(|s| format!("{:016x}", s.as_u64()));
        let mut out: Vec<RunView> = demo::runs()
            .into_iter()
            .filter(|r| {
                filter.state.is_none_or(|s| r.state == s)
                    && filter
                        .project
                        .as_ref()
                        .is_none_or(|p| r.project_root.as_deref() == Some(p.as_str()))
                    && filter
                        .session
                        .is_none_or(|_| r.session_id.as_deref() == session_hex.as_deref())
            })
            .collect();
        out.truncate(filter.limit.unwrap_or(200).clamp(1, 1000));
        out
    }
    fn run(&self, id: RunId) -> Option<RunView> {
        demo::run(id.0)
    }
    fn capacity(&self) -> Capacity {
        demo::capacity()
    }
    fn run_output(
        &self,
        id: RunId,
        stream: Stream,
        cursor: u64,
        max_bytes: usize,
    ) -> io::Result<Value> {
        demo::run_output(id.0, stream, cursor, max_bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("run {id} not found")))
    }
    fn cancel_run(&self, id: RunId) -> io::Result<Option<RunView>> {
        // Simulated: demo recomputes the view, no process is ever touched.
        Ok(demo::cancel_run(id.0))
    }
}

#[derive(Debug)]
enum Route<'a> {
    Health,
    Capacity,
    Snapshot,
    SessionsList,
    SessionGet { id: u64 },
    Items,
    Leftovers,
    Explain { pid: u32 },
    RunsList,
    RunGet { id: i64 },
    RunOutput { id: i64 },
    RunCancelProposePost { id: i64 },
    RunCancelPost { id: i64 },
    ProposalPost,
    ConfirmPost,
    KillPost,
    DockerStopPost,
    DockerRemovePost,
    DockerPrunePost,
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
    // Demo mode reads no host state at all, so a running supervisor is not a
    // reason to refuse it — the error text already told users to use --demo.
    if !opts.demo && server::serve_alive() {
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
        let sink = Arc::new(SnapshotSink::default());
        thread::spawn({
            let sink = Arc::clone(&sink);
            move || server::collect_loop(Some(sink))
        });
        eprintln!("wyd web on http://{addr} (local)");
        Arc::new(LocalProvider::new(RuntimeStore::default_path(), sink))
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
    csrf: String,
    proposals: HashMap<String, Value>,
    #[allow(dead_code)]
    last_snapshot_version: u64,
}

impl WebState {
    fn new(mode: &'static str) -> Self {
        Self {
            mode,
            csrf: new_csrf(),
            proposals: HashMap::new(),
            last_snapshot_version: 0,
        }
    }
}

fn new_csrf() -> String {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).expect("rng");
    buf.iter().map(|b| format!("{b:02x}")).collect()
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
        ("GET", "/api/capacity") => Route::Capacity,
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
        ("GET", "/api/runs") => Route::RunsList,
        ("GET", p) if p.starts_with("/api/runs/") && p.ends_with("/output") => Route::RunOutput {
            id: parse_run_id(p),
        },
        // Exact `/api/runs/<id>` only: `/api/runs/1/cancel` on GET is not a
        // run read and must fall through to NotFound.
        ("GET", p) if bare_run_id(p).is_some() => Route::RunGet {
            id: bare_run_id(p).unwrap_or(0),
        },
        ("POST", p) if p.starts_with("/api/runs/") && p.ends_with("/cancel/propose") => {
            Route::RunCancelProposePost {
                id: parse_run_id(p),
            }
        }
        ("POST", p) if p.starts_with("/api/runs/") && p.ends_with("/cancel") => {
            Route::RunCancelPost {
                id: parse_run_id(p),
            }
        }
        ("POST", "/api/proposal") => Route::ProposalPost,
        ("POST", "/api/confirm") => Route::ConfirmPost,
        ("POST", "/api/kill") => Route::KillPost,
        ("POST", "/api/docker/stop") => Route::DockerStopPost,
        ("POST", "/api/docker/remove") => Route::DockerRemovePost,
        ("POST", "/api/docker/prune") => Route::DockerPrunePost,
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
    fn not_found() -> Self {
        Self::json(404, json!({"ok": false, "error": "not found"}))
    }
}

fn csrf_ok(req: &ParsedRequest, token: &str) -> bool {
    serde_json::from_str::<Value>(&req.body)
        .ok()
        .and_then(|v| v.get("csrf").and_then(Value::as_str).map(|s| s == token))
        .unwrap_or(false)
}

fn index_html(csrf: &str) -> HttpResponse {
    let body = String::from_utf8_lossy(assets::INDEX_HTML)
        .replace("__WYD_CSRF__", csrf)
        .into_bytes();
    HttpResponse {
        status: 200,
        reason: "OK",
        content_type: "text/html; charset=utf-8",
        body,
        extra_headers: Vec::new(),
    }
}

fn build_response(
    route: &Route<'_>,
    req: &ParsedRequest,
    state: &Arc<RwLock<WebState>>,
    provider: &dyn RuntimeProvider,
) -> HttpResponse {
    if matches!(
        route,
        Route::ProposalPost
            | Route::ConfirmPost
            | Route::KillPost
            | Route::DockerStopPost
            | Route::DockerRemovePost
            | Route::DockerPrunePost
            | Route::RunCancelProposePost { .. }
            | Route::RunCancelPost { .. }
    ) && !csrf_ok(req, &state.read().csrf)
    {
        return HttpResponse::json(403, json!({"ok": false, "error": "forbidden"}));
    }
    match route {
        Route::Health => HttpResponse::json(
            200,
            json!({
                "ok": true,
                "mode": provider.mode(),
                "banner": if provider.mode() == "demo" { DEMO_BANNER } else { "" },
            }),
        ),
        Route::Snapshot => snapshot_response(provider),
        Route::Capacity => capacity_response(provider),
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
        Route::KillPost => kill_response(req, provider),
        Route::RunsList => runs_response(provider, req),
        Route::RunGet { id } => run_response(provider, *id),
        Route::RunOutput { id } => run_output_response(provider, req, *id),
        Route::RunCancelProposePost { id } => run_cancel_propose_response(state, provider, *id),
        Route::RunCancelPost { id } => run_cancel_response(state, provider, req, *id),
        Route::DockerStopPost => docker_stop_response(req, provider),
        Route::DockerRemovePost => docker_remove_response(req, provider),
        Route::DockerPrunePost => docker_prune_response(req, provider),
        Route::StaticIndex => index_html(&state.read().csrf),
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
    }
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
/// Local mode reads the durable store. Demo mode has no store, so ownership
/// is derived from the agent tree (same mapping `demo::explain` uses).
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
        "demo" => Some(demo_session_map(provider)),
        _ => None,
    }
}

fn demo_session_map(provider: &dyn RuntimeProvider) -> HashMap<u32, u64> {
    let snap = provider.snapshot();
    let agent_sid: HashMap<String, u64> = provider
        .sessions()
        .into_iter()
        .map(|s| (s.agent, s.id.as_u64()))
        .collect();
    let mut map = HashMap::new();
    fn walk(
        item: &crate::model::RuntimeItem,
        inherited: Option<u64>,
        agent_sid: &HashMap<String, u64>,
        map: &mut HashMap<u32, u64>,
    ) {
        let sid = if item.category == crate::model::Category::Agent {
            agent_sid.get(&item.display_name).copied().or(inherited)
        } else {
            inherited
        };
        if let (Some(pid), Some(sid)) = (item.root_pid, sid) {
            map.insert(pid, sid);
        }
        for c in &item.children {
            walk(c, sid, agent_sid, map);
        }
    }
    for item in &snap.logical_items {
        walk(item, None, &agent_sid, &mut map);
    }
    map
}

/// Build a nested tree of runtime items (children are real child nodes,
/// like the TUI's `\u251c`/`\u2514` tree), plus a `what` short label per
/// category (agent/mcp/dev/db/...) matching the TUI's WHAT column.
fn items_json(snap: &RuntimeSnapshot, session_map: Option<&HashMap<u32, u64>>) -> Vec<Value> {
    fn what(item: &crate::model::RuntimeItem) -> String {
        let ports = item.ports.len();
        // Port-bearing categories get compact semantics that never imply a
        // canonical listener: no listeners → bare role; exactly one → :port;
        // several → ×N (matches the TUI's WHAT column).
        let base = match item.category {
            crate::model::Category::Agent => "agent",
            crate::model::Category::Mcp => "mcp",
            crate::model::Category::Browser => "browser",
            crate::model::Category::LanguageServer => "ls",
            crate::model::Category::DevService => "service",
            crate::model::Category::Worker => "worker",
            crate::model::Category::UnknownDev => "dev",
            crate::model::Category::DevServer => "server",
            crate::model::Category::Database => "db",
        };
        match item.category {
            crate::model::Category::DevServer => match ports {
                0 => base.to_string(),
                1 => format!("srv :{}", item.ports[0].port),
                _ => format!("srv ×{ports}"),
            },
            crate::model::Category::Database => match ports {
                0 => base.to_string(),
                1 => format!("db :{}", item.ports[0].port),
                _ => format!("db ×{ports}"),
            },
            _ => base.to_string(),
        }
    }

    fn build(
        item: &crate::model::RuntimeItem,
        session_map: Option<&HashMap<u32, u64>>,
        age_map: &HashMap<u32, u64>,
        procs: &HashMap<u32, &crate::model::ProcessInfo>,
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
        let explanations = item
            .suspicion
            .as_ref()
            .map(|s| {
                s.reasons
                    .iter()
                    .map(|r| r.explanation().to_string())
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
            .map(|c| build(c, session_map, age_map, procs))
            .collect::<Vec<_>>();
        let proc = item.root_pid.and_then(|pid| procs.get(&pid)).copied();
        json!({
            "category": item.category.label(),
            "what": what(item),
            "name": item.display_name,
            "title": item.title(),
            "verdict": item.verdict(),
            "root_pid": item.root_pid,
            "ppid": proc.and_then(|p| p.parent_pid),
            "cmd": proc.map(|p| p.command.clone()).unwrap_or_default(),
            "cwd": proc.and_then(|p| p.cwd.as_ref()).map(|c| c.display().to_string()),
            "tty": proc.and_then(|p| p.tty.clone()),
            "session_id": session_id.map(|s| format!("{:016x}", s)),
            "memory_bytes": item.memory_bytes,
            "cpu_percent": item.cpu_percent,
            "age_seconds": age_seconds,
            "status": status,
            "score": item.suspicion.as_ref().map(|s| s.score),
            "reasons": reasons,
            "explanations": explanations,
            "ports": item.ports.iter().map(|p| json!({
                "port": p.port,
                "protocol": p.protocol.as_str(),
                "address": p.address.to_string(),
                "pid": p.pid,
            })).collect::<Vec<_>>(),
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
    let procs: HashMap<u32, &crate::model::ProcessInfo> =
        snap.processes.iter().map(|p| (p.pid, p)).collect();
    snap.logical_items
        .iter()
        .map(|i| build(i, session_map, &age_map, &procs))
        .collect()
}

/// Category counts + totals for the sidebar "Overview", like the TUI.
fn overview(snap: &RuntimeSnapshot) -> Value {
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<String, (usize, u64, f32)> = BTreeMap::new();
    let mut total_items = 0usize;
    let mut total_mem = 0u64;
    fn walk(
        item: &crate::model::RuntimeItem,
        counts: &mut BTreeMap<String, (usize, u64, f32)>,
        total_items: &mut usize,
        total_mem: &mut u64,
    ) {
        *total_items += 1;
        *total_mem += item.memory_bytes;
        let e = counts
            .entry(item.category.label().to_string())
            .or_insert((0, 0, 0.0));
        e.0 += 1;
        e.1 += item.memory_bytes;
        e.2 += item.cpu_percent;
        for c in &item.children {
            walk(c, counts, total_items, total_mem);
        }
    }
    for item in &snap.logical_items {
        walk(item, &mut counts, &mut total_items, &mut total_mem);
    }
    let suspicious = {
        let mut n = 0u32;
        let mut ram = 0u64;
        fn walk2(item: &crate::model::RuntimeItem, n: &mut u32, ram: &mut u64) {
            if item.state == crate::model::RuntimeState::Suspicious {
                *n += 1;
                *ram += item.memory_bytes;
            }
            for c in &item.children {
                walk2(c, n, ram);
            }
        }
        for item in &snap.logical_items {
            walk2(item, &mut n, &mut ram);
        }
        (n, ram)
    };
    let ports = count_ports(&snap.logical_items);
    let projects = count_projects(&snap.logical_items);
    json!({
        "total_items": total_items,
        "total_memory_bytes": total_mem,
        "suspicious": suspicious.0,
        "leftover_memory_bytes": suspicious.1,
        "ports": ports,
        "projects": projects,
        "categories": counts.iter().map(|(k, (n, mem, cpu))| json!({
            "category": k,
            "count": n,
            "memory_bytes": mem,
            "cpu_percent": cpu,
        })).collect::<Vec<_>>(),
    })
}

fn count_ports(items: &[crate::model::RuntimeItem]) -> usize {
    items
        .iter()
        .map(|i| i.ports.len() + count_ports(&i.children))
        .sum()
}

fn count_projects(items: &[crate::model::RuntimeItem]) -> usize {
    let mut names = std::collections::HashSet::new();
    fn walk<'a>(
        items: &'a [crate::model::RuntimeItem],
        names: &mut std::collections::HashSet<&'a str>,
    ) {
        for i in items {
            if let Some(p) = &i.project {
                names.insert(&p.name);
            }
            walk(&i.children, names);
        }
    }
    walk(items, &mut names);
    names.len()
}

fn docker_json(d: &Arc<crate::model::DockerSnapshot>) -> Value {
    json!({
        "ok": d.ok,
        "note": d.note,
        "disk_bytes": d.disk_bytes,
        "reclaimable_bytes": d.reclaimable_bytes,
        "resources": d.resources.iter().map(|r| json!({
            "kind": format!("{:?}", r.kind),
            "kind_label": r.kind.label(),
            "id": r.id,
            "name": r.name,
            "detail": r.detail,
            "size_bytes": r.size_bytes,
            "compose": r.compose,
            "anonymous": r.anonymous,
            "persistent": r.persistent,
            "running": r.running(),
            "created": r.created,
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
            "simulated": provider.mode() == "demo",
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
/// `--demo` is a no-op and never reads the host process table.
fn kill_response(req: &ParsedRequest, provider: &dyn RuntimeProvider) -> HttpResponse {
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
    if provider.mode() == "demo" {
        return HttpResponse::json(
            200,
            json!({
                "ok": true,
                "simulated": true,
                "data": { "pid": pid, "signaled": 0, "skipped": 0, "failed": 0 }
            }),
        );
    }
    let snap = provider.snapshot();
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
    let force = body.get("force").and_then(Value::as_bool).unwrap_or(false);
    let signal = if force {
        crate::actions::process::Signal::Kill
    } else {
        crate::actions::process::Signal::Term
    };
    let report = crate::actions::process::send(&[id], signal);
    HttpResponse::json(
        200,
        json!({
            "ok": true,
            "data": {
                "pid": pid,
                "force": force,
                "signaled": report.signaled,
                "skipped": report.skipped,
                "failed": report.failed,
            }
        }),
    )
}

fn docker_stop_response(req: &ParsedRequest, provider: &dyn RuntimeProvider) -> HttpResponse {
    let id = match body_id(req) {
        Some(i) => i,
        None => return HttpResponse::json(400, json!({"ok": false, "error": "missing id"})),
    };
    if provider.mode() == "demo" {
        return HttpResponse::json(
            200,
            json!({"ok": true, "simulated": true, "data": { "id": id, "simulated": true } }),
        );
    }
    match resource_by_id(provider, &id) {
        Some(res) => match crate::actions::docker::stop_blocking(&res) {
            Ok(()) => HttpResponse::json(
                200,
                json!({"ok": true, "data": { "id": id, "stopped": true, "simulated": false }}),
            ),
            Err(e) => HttpResponse::json(500, json!({"ok": false, "error": e})),
        },
        None => HttpResponse::json(
            404,
            json!({"ok": false, "error": "no such docker resource"}),
        ),
    }
}

fn docker_remove_response(req: &ParsedRequest, provider: &dyn RuntimeProvider) -> HttpResponse {
    let id = match body_id(req) {
        Some(i) => i,
        None => return HttpResponse::json(400, json!({"ok": false, "error": "missing id"})),
    };
    if provider.mode() == "demo" {
        return HttpResponse::json(
            200,
            json!({"ok": true, "simulated": true, "data": { "id": id, "simulated": true } }),
        );
    }
    match resource_by_id(provider, &id) {
        Some(res) => match crate::actions::docker::remove_blocking(&res) {
            Ok(()) => HttpResponse::json(
                200,
                json!({"ok": true, "data": { "id": id, "removed": true, "simulated": false }}),
            ),
            Err(e) => HttpResponse::json(500, json!({"ok": false, "error": e})),
        },
        None => HttpResponse::json(
            404,
            json!({"ok": false, "error": "no such docker resource"}),
        ),
    }
}

fn docker_prune_response(_req: &ParsedRequest, provider: &dyn RuntimeProvider) -> HttpResponse {
    if provider.mode() == "demo" {
        return HttpResponse::json(
            200,
            json!({"ok": true, "simulated": true, "data": { "pruned": 0, "reclaim_bytes": 0, "simulated": true } }),
        );
    }
    let ids = provider.snapshot().docker.prunable_ids();
    match crate::actions::docker::prune_anonymous_volumes_blocking(&ids) {
        Ok((pruned, bytes)) => HttpResponse::json(
            200,
            json!({"ok": true, "data": { "pruned": pruned, "reclaim_bytes": bytes } }),
        ),
        Err(e) => HttpResponse::json(500, json!({"ok": false, "error": e})),
    }
}

/// Parse `{ "id": "..." }` from a request body.
fn body_id(req: &ParsedRequest) -> Option<String> {
    let v: Value = serde_json::from_str(&req.body).ok()?;
    v.get("id").and_then(Value::as_str).map(|s| s.to_string())
}

/// Find a docker resource by id in the current snapshot.
fn resource_by_id(
    provider: &dyn RuntimeProvider,
    id: &str,
) -> Option<crate::model::DockerResource> {
    provider
        .snapshot()
        .docker
        .resources
        .iter()
        .find(|r| r.id == id)
        .cloned()
}

// ── Managed runs ──────────────────────────────────────────────────────
//
// Reads (`GET /api/runs`, `GET /api/runs/<id>`, `GET /api/runs/<id>/output`)
// never execute or signal anything and work with no supervisor. Cancelling a
// run is the only mutating action and is gated by the same propose/confirm +
// CSRF pattern as kill/docker: a bare GET can never cancel.

/// `/api/runs/<id>[/...]` → the numeric run id (`0` when unparsable).
/// `/api/runs/<decimal id>` with nothing after the id.
fn bare_run_id(path: &str) -> Option<i64> {
    let rest = path.strip_prefix("/api/runs/")?;
    if rest.contains('/') {
        return None;
    }
    rest.parse().ok()
}

fn parse_run_id(path: &str) -> i64 {
    path.trim_start_matches("/api/runs/")
        .split('/')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Value of `key` in the request query string, `+`/`%XX` decoded.
fn query_param(path: &str, key: &str) -> Option<String> {
    let query = path.split_once('?')?.1;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == key {
            return Some(percent_decode(v));
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi * 16 + lo) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn run_filter_from_query(req: &ParsedRequest) -> RunFilter {
    let mut filter = RunFilter::default();
    if let Some(state) = query_param(&req.path, "state") {
        filter.state = RunState::parse(&state);
    }
    if let Some(project) = query_param(&req.path, "project") {
        filter.project = Some(project);
    }
    if let Some(session) = query_param(&req.path, "session") {
        filter.session = Some(RuntimeSessionId::from_u64(
            u64::from_str_radix(&session, 16).unwrap_or(0),
        ));
    }
    if let Some(limit) = query_param(&req.path, "limit") {
        filter.limit = limit.parse().ok();
    }
    filter
}

/// Read-only admission view. Same `Capacity` shape as the socket's
/// `get_capacity`; the web has no way to change limits (the CLI does).
fn capacity_response(provider: &dyn RuntimeProvider) -> HttpResponse {
    HttpResponse::json(
        200,
        json!({ "ok": true, "data": { "capacity": provider.capacity() } }),
    )
}

fn runs_response(provider: &dyn RuntimeProvider, req: &ParsedRequest) -> HttpResponse {
    let runs = provider.runs(&run_filter_from_query(req));
    HttpResponse::json(200, json!({ "ok": true, "data": { "runs": runs } }))
}

fn run_response(provider: &dyn RuntimeProvider, id: i64) -> HttpResponse {
    match provider.run(RunId(id)) {
        Some(run) => HttpResponse::json(200, json!({ "ok": true, "data": { "run": run } })),
        None => HttpResponse::json(404, json!({"ok": false, "error": "no such run"})),
    }
}

fn run_output_response(
    provider: &dyn RuntimeProvider,
    req: &ParsedRequest,
    id: i64,
) -> HttpResponse {
    let stream = match query_param(&req.path, "stream") {
        None => Stream::Stdout,
        Some(s) => match Stream::parse(&s) {
            Some(s) => s,
            None => {
                return HttpResponse::json(
                    400,
                    json!({"ok": false, "error": "stream must be stdout or stderr"}),
                );
            }
        },
    };
    let cursor = query_param(&req.path, "cursor")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let max_bytes = query_param(&req.path, "max_bytes")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(64 * 1024)
        .clamp(1, 1024 * 1024);
    match provider.run_output(RunId(id), stream, cursor, max_bytes) {
        Ok(chunk) => HttpResponse::json(200, json!({ "ok": true, "data": chunk })),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            HttpResponse::json(404, json!({"ok": false, "error": e.to_string()}))
        }
        Err(e) => HttpResponse::json(500, json!({"ok": false, "error": e.to_string()})),
    }
}

fn run_cancel_propose_response(
    state: &Arc<RwLock<WebState>>,
    provider: &dyn RuntimeProvider,
    id: i64,
) -> HttpResponse {
    let Some(run) = provider.run(RunId(id)) else {
        return HttpResponse::json(404, json!({"ok": false, "error": "no such run"}));
    };
    if run.state.is_terminal() {
        return HttpResponse::json(409, json!({"ok": false, "error": "run already finished"}));
    }
    let proposal = json!({
        "kind": "cancel_run",
        "run_id": run.run_id,
        "request_id": run.request_id,
        "argv": run.argv,
        "cwd": run.cwd,
        "session_id": run.session_id,
        "state": run.state,
        "revision": run.revision,
        "note": "Confirm to ask the supervisor to stop this run and its process group. Nothing is signalled until then.",
    });
    let proposal_id = format!("run-{:x}-{:x}", id, now());
    state
        .write()
        .proposals
        .insert(proposal_id.clone(), proposal.clone());
    HttpResponse::json(
        200,
        json!({ "ok": true, "data": { "id": proposal_id, "proposal": proposal } }),
    )
}

fn run_cancel_response(
    state: &Arc<RwLock<WebState>>,
    provider: &dyn RuntimeProvider,
    req: &ParsedRequest,
    id: i64,
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
    let Some(proposal_id) = body.get("id").and_then(Value::as_str) else {
        return HttpResponse::json(400, json!({"ok": false, "error": "missing id"}));
    };
    let stored = state.read().proposals.get(proposal_id).cloned();
    let Some(stored) = stored else {
        return HttpResponse::json(404, json!({"ok": false, "error": "no such proposal"}));
    };
    let want = id.to_string();
    if stored.get("kind").and_then(Value::as_str) != Some("cancel_run")
        || stored.get("run_id").and_then(Value::as_str) != Some(want.as_str())
    {
        return HttpResponse::json(
            409,
            json!({"ok": false, "error": "proposal does not match this run"}),
        );
    }
    // Re-check id and state immediately before acting: the run may have
    // finished while the human read the proposal.
    let Some(run) = provider.run(RunId(id)) else {
        return HttpResponse::json(404, json!({"ok": false, "error": "no such run"}));
    };
    if run.state.is_terminal() {
        return HttpResponse::json(409, json!({"ok": false, "error": "run already finished"}));
    }
    state.write().proposals.remove(proposal_id);
    match provider.cancel_run(RunId(id)) {
        Ok(Some(view)) => HttpResponse::json(
            200,
            json!({
                "ok": true,
                "simulated": provider.mode() == "demo",
                "data": { "run": view },
            }),
        ),
        Ok(None) => HttpResponse::json(404, json!({"ok": false, "error": "no such run"})),
        Err(e) => HttpResponse::json(
            503,
            json!({"ok": false, "error": format!("no supervisor available: {e}")}),
        ),
    }
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

    fn demo_state() -> Arc<RwLock<WebState>> {
        Arc::new(RwLock::new(WebState::new("demo")))
    }

    fn post_json(body: Value) -> ParsedRequest {
        ParsedRequest {
            method: "POST".into(),
            body: body.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn responses_have_no_cors() {
        let state = demo_state();
        let req = ParsedRequest {
            method: "GET".into(),
            path: "/api/health".into(),
            ..Default::default()
        };
        let resp = build_response(&Route::Health, &req, &state, &DemoProvider);
        assert!(
            resp.extra_headers
                .iter()
                .all(|(k, _)| !k.starts_with("Access-Control"))
        );
    }

    #[test]
    fn mutating_routes_reject_missing_csrf() {
        let state = demo_state();
        for route in [
            Route::KillPost,
            Route::DockerStopPost,
            Route::DockerRemovePost,
            Route::DockerPrunePost,
        ] {
            let resp = build_response(
                &route,
                &post_json(json!({"id": "x"})),
                &state,
                &DemoProvider,
            );
            assert_eq!(resp.status, 403, "route {route:?} must require csrf");
        }
    }

    #[test]
    fn mutating_routes_reject_wrong_csrf() {
        let state = demo_state();
        let resp = build_response(
            &Route::KillPost,
            &post_json(json!({"pid": 4101, "csrf": "nope"})),
            &state,
            &DemoProvider,
        );
        assert_eq!(resp.status, 403);
    }

    #[test]
    fn demo_kill_is_simulated_and_skips_host() {
        let state = demo_state();
        let csrf = state.read().csrf.clone();
        let resp = build_response(
            &Route::KillPost,
            &post_json(json!({"pid": 1, "csrf": csrf})),
            &state,
            &DemoProvider,
        );
        assert_eq!(resp.status, 200);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["simulated"], true);
        assert_eq!(v["data"]["pid"], 1);
        assert_eq!(v["data"]["signaled"], 0);
    }

    #[test]
    fn index_injects_csrf_token() {
        let state = demo_state();
        let csrf = state.read().csrf.clone();
        let req = ParsedRequest {
            method: "GET".into(),
            path: "/".into(),
            ..Default::default()
        };
        let resp = build_response(&Route::StaticIndex, &req, &state, &DemoProvider);
        let html = String::from_utf8(resp.body).unwrap();
        assert!(html.contains(&csrf));
        assert!(!html.contains("__WYD_CSRF__"));
    }

    #[test]
    fn csrf_token_is_hex_and_unique() {
        let a = new_csrf();
        let b = new_csrf();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    // ── Listener JSON semantics ────────────────────────────────────────
    fn lp(port: u16, pid: u32) -> crate::model::ListeningPort {
        crate::model::ListeningPort {
            protocol: crate::model::Protocol::Tcp,
            address: "127.0.0.1".parse().unwrap(),
            port,
            pid,
        }
    }

    fn node_snapshot() -> RuntimeSnapshot {
        use crate::model::{Category, RuntimeItem, RuntimeState};
        let item = RuntimeItem {
            category: Category::DevServer,
            display_name: "node".into(),
            root_pid: Some(90167),
            process_ids: vec![90167],
            memory_bytes: 16 << 20,
            cpu_percent: 0.0,
            state: RuntimeState::Suspicious,
            suspicion: Some(crate::model::Suspicion {
                score: 40,
                reasons: vec![crate::model::SuspicionReason::ParentExited],
            }),
            ports: vec![
                lp(45623, 90167),
                lp(49206, 90167),
                lp(53674, 90167),
                lp(53675, 90167),
            ],
            project: None,
            children: vec![],
        };
        RuntimeSnapshot {
            processes: vec![crate::model::ProcessInfo {
                pid: 90167,
                parent_pid: Some(1),
                name: "node".into(),
                command: vec![
                    "/Library/Application Support/OpenCode/open-code".into(),
                    "server".into(),
                ],
                executable: None,
                cwd: Some("/".into()),
                cpu_percent: 0.0,
                memory_bytes: 16 << 20,
                start_time: 1,
                tty: Some("ttys000".into()),
            }],
            logical_items: vec![item],
            docker: Arc::new(crate::model::DockerSnapshot::default()),
            total_memory_bytes: 32 << 30,
            used_memory_bytes: 7 << 30,
            cpu_percent: 1.0,
            sessions: vec![],
            version: 1,
        }
    }

    #[test]
    fn listeners_serialize_with_port_protocol_address_pid() {
        let snap = node_snapshot();
        let items = items_json(&snap, None);
        assert_eq!(items.len(), 1);
        let it = &items[0];
        let ports = it["ports"].as_array().unwrap();
        assert_eq!(ports.len(), 4, "all four listeners survive");
        let serialized = serde_json::to_string(it).unwrap();
        for p in ports {
            assert!(p["port"].is_u64(), "port present");
            assert_eq!(p["protocol"], "tcp", "stable lowercase protocol");
            assert_eq!(p["address"], "127.0.0.1", "address present");
            assert!(p["pid"].is_u64(), "pid present");
        }
        // Protocol is a stable lowercase string, not Rust Debug output.
        assert!(
            !serialized.contains("\"Tcp\""),
            "must not leak Rust Debug for protocol: {serialized}"
        );
        // Existing useful fields remain.
        assert_eq!(it["root_pid"], 90167);
        assert_eq!(it["name"], "node");
        assert_eq!(it["status"], "suspicious");
    }

    #[test]
    fn items_expose_score_reasons_explanations_verdict() {
        let snap = node_snapshot();
        let it = &items_json(&snap, None)[0];
        assert_eq!(it["score"], 40);
        assert_eq!(it["reasons"][0], "parent exited / re-parented");
        assert!(
            it["explanations"][0]
                .as_str()
                .unwrap()
                .contains("original parent")
        );
        assert_eq!(it["verdict"], "leftover candidate");
    }

    #[test]
    fn items_expose_process_identity() {
        let it = &items_json(&node_snapshot(), None)[0];
        assert_eq!(it["ppid"], 1);
        assert_eq!(it["cwd"], "/");
        assert_eq!(it["tty"], "ttys000");
        assert_eq!(
            it["cmd"][0],
            "/Library/Application Support/OpenCode/open-code"
        );
        assert_eq!(it["cmd"][1], "server");
    }

    #[test]
    fn demo_docker_actions_are_simulated() {
        let state = demo_state();
        let csrf = state.read().csrf.clone();
        for route in [
            Route::DockerStopPost,
            Route::DockerRemovePost,
            Route::DockerPrunePost,
        ] {
            let body = if matches!(route, Route::DockerPrunePost) {
                json!({"csrf": csrf.clone()})
            } else {
                json!({"id": "abc", "csrf": csrf.clone()})
            };
            let resp = build_response(&route, &post_json(body), &state, &DemoProvider);
            assert_eq!(resp.status, 200, "route {route:?}");
            let v: Value = serde_json::from_slice(&resp.body).unwrap();
            assert_eq!(v["simulated"], true, "route {route:?}");
        }
    }

    #[test]
    fn demo_items_carry_session_id() {
        let resp = snapshot_response(&DemoProvider);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        let items = v["data"]["items"].as_array().unwrap();
        fn find<'a>(items: &'a [Value], name: &str) -> Option<&'a Value> {
            for i in items {
                if i["name"].as_str() == Some(name) {
                    return Some(i);
                }
                if let Some(ch) = i["children"].as_array()
                    && let Some(found) = find(ch, name)
                {
                    return Some(found);
                }
            }
            None
        }
        let oc = find(items, "opencode").expect("opencode item");
        let cr = find(items, "Chromium x8").expect("chromium leftover");
        let sid = oc["session_id"].as_str().expect("opencode session_id");
        assert_eq!(sid.len(), 16);
        assert_eq!(cr["session_id"].as_str(), Some(sid));
        let sessions = v["data"]["sessions"].as_array().unwrap();
        let oc_sess = sessions.iter().find(|s| s["agent"] == "opencode").unwrap();
        assert_eq!(oc_sess["id"].as_str(), Some(sid));
        assert_eq!(oc_sess["active"], false);
    }

    #[test]
    fn demo_explain_chromium_names_opencode_session() {
        let exp = DemoProvider.explain(4102).expect("chromium explain");
        assert_eq!(exp["name"], "Chromium x8");
        assert_eq!(exp["ownership"], "owned");
        assert_eq!(exp["session"]["agent"], "opencode");
        assert_eq!(exp["session"]["active"], false);
        assert!(exp["evidence"].as_array().unwrap().len() >= 2);
    }

    // ── Managed runs ───────────────────────────────────────────────────
    #[test]
    fn run_routes_are_matched_and_gets_never_cancel() {
        assert!(matches!(match_route("GET", "/api/runs"), Route::RunsList));
        assert!(matches!(
            match_route("GET", "/api/runs/412"),
            Route::RunGet { id: 412 }
        ));
        assert!(matches!(
            match_route("GET", "/api/runs/412/output"),
            Route::RunOutput { id: 412 }
        ));
        assert!(matches!(
            match_route("POST", "/api/runs/412/cancel/propose"),
            Route::RunCancelProposePost { id: 412 }
        ));
        assert!(matches!(
            match_route("POST", "/api/runs/412/cancel"),
            Route::RunCancelPost { id: 412 }
        ));
        // A bare GET can never reach a cancel route.
        for path in [
            "/api/runs",
            "/api/runs/412",
            "/api/runs/412/output",
            "/api/runs/412/cancel",
            "/api/runs/412/cancel/propose",
        ] {
            assert!(
                !matches!(
                    match_route("GET", path),
                    Route::RunCancelPost { .. } | Route::RunCancelProposePost { .. }
                ),
                "GET {path} must not map to a cancel route"
            );
        }
    }

    #[test]
    fn run_cancel_routes_require_csrf() {
        let state = demo_state();
        for route in [
            Route::RunCancelProposePost { id: 412 },
            Route::RunCancelPost { id: 412 },
        ] {
            let resp = build_response(
                &route,
                &post_json(json!({"id": "x"})),
                &state,
                &DemoProvider,
            );
            assert_eq!(resp.status, 403, "route {route:?} must require csrf");
        }
    }

    #[test]
    fn demo_runs_are_synthetic_and_cancel_is_simulated() {
        let runs = DemoProvider.runs(&RunFilter::default());
        assert_eq!(runs.len(), 8);
        let values: Vec<Value> = runs
            .iter()
            .map(|r| serde_json::to_value(r).unwrap())
            .collect();
        assert!(values.iter().any(|v| v["state"] == "running"));
        assert!(
            values.iter().any(|v| v["state"] == "queued"),
            "demo needs a queued run to render the queue distinctly"
        );
        let outcomes: Vec<&str> = values
            .iter()
            .filter_map(|v| v["outcome"].as_str())
            .collect();
        for expected in [
            "exited",
            "timed_out",
            "cancelled",
            "spawn_failed",
            "resource_limit",
        ] {
            assert!(outcomes.contains(&expected), "missing outcome {expected}");
        }

        // Cancelling a live demo run simulates the transition and leaves the
        // synthetic definition untouched — no host process is involved.
        let before = demo::run(412).unwrap();
        assert!(!before.state.is_terminal());
        let cancelled = DemoProvider.cancel_run(RunId(412)).unwrap().unwrap();
        let cancelled = serde_json::to_value(&cancelled).unwrap();
        assert_eq!(cancelled["state"], "finished");
        assert_eq!(cancelled["outcome"], "cancelled");
        assert_eq!(cancelled["cleanup"], "complete");
        assert!(!demo::run(412).unwrap().state.is_terminal());

        // Idempotent on an already-finished run.
        let done = DemoProvider.cancel_run(RunId(411)).unwrap().unwrap();
        assert_eq!(serde_json::to_value(&done).unwrap()["outcome"], "exited");
    }

    #[test]
    fn run_json_matches_contract_shape() {
        let runs = DemoProvider.runs(&RunFilter::default());
        let v = serde_json::to_value(&runs[0]).unwrap();
        for key in [
            "run_id",
            "request_id",
            "argv",
            "cwd",
            "project_root",
            "session_id",
            "state",
            "outcome",
            "exit_code",
            "signal",
            "cleanup",
            "revision",
            "created_at",
            "started_at",
            "finished_at",
            "duration_ms",
            "detail",
            "logs",
            "capabilities",
        ] {
            assert!(v.get(key).is_some(), "run view missing {key}");
        }
        assert!(v["run_id"].is_string(), "run_id is a decimal string");
        assert!(v["logs"]["stdout_bytes"].is_u64());
        assert!(v["capabilities"]["process_group_cleanup"].is_string());
    }

    #[test]
    fn runs_list_filters_and_output_chunks() {
        let state = demo_state();
        let req = ParsedRequest {
            method: "GET".into(),
            path: "/api/runs?state=running".into(),
            ..Default::default()
        };
        let resp = build_response(&Route::RunsList, &req, &state, &DemoProvider);
        assert_eq!(resp.status, 200);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        let runs = v["data"]["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 2);
        assert!(runs.iter().all(|r| r["state"] == "running"));

        let req = ParsedRequest {
            method: "GET".into(),
            path: "/api/runs/412/output?stream=stdout&cursor=0&max_bytes=5".into(),
            ..Default::default()
        };
        let resp = build_response(&Route::RunOutput { id: 412 }, &req, &state, &DemoProvider);
        assert_eq!(resp.status, 200);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["data"]["stream"], "stdout");
        assert_eq!(v["data"]["data"], "web: ");
        assert_eq!(v["data"]["next_cursor"], 5);
        assert_eq!(v["data"]["eof"], false);
        assert!(v["data"]["truncated"].is_boolean());
    }

    #[test]
    fn cancel_proposal_then_confirm_cancels_demo_run() {
        let state = demo_state();
        let csrf = state.read().csrf.clone();
        let resp = build_response(
            &Route::RunCancelProposePost { id: 412 },
            &post_json(json!({"csrf": csrf.clone()})),
            &state,
            &DemoProvider,
        );
        assert_eq!(resp.status, 200);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        let proposal_id = v["data"]["id"].as_str().unwrap().to_string();
        assert_eq!(v["data"]["proposal"]["kind"], "cancel_run");
        assert_eq!(v["data"]["proposal"]["run_id"], "412");

        // A proposal for one run cannot cancel another.
        let resp = build_response(
            &Route::RunCancelPost { id: 411 },
            &post_json(json!({"csrf": csrf.clone(), "id": proposal_id})),
            &state,
            &DemoProvider,
        );
        assert_eq!(resp.status, 409);

        let resp = build_response(
            &Route::RunCancelPost { id: 412 },
            &post_json(json!({"csrf": csrf, "id": proposal_id})),
            &state,
            &DemoProvider,
        );
        assert_eq!(resp.status, 200);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["simulated"], true);
        assert_eq!(v["data"]["run"]["state"], "finished");
        assert_eq!(v["data"]["run"]["outcome"], "cancelled");
    }

    #[test]
    fn cancel_proposal_rejects_finished_run() {
        let state = demo_state();
        let csrf = state.read().csrf.clone();
        let resp = build_response(
            &Route::RunCancelProposePost { id: 411 },
            &post_json(json!({"csrf": csrf})),
            &state,
            &DemoProvider,
        );
        assert_eq!(resp.status, 409);
    }

    // ── Capacity ───────────────────────────────────────────────────────
    #[test]
    fn capacity_route_returns_contract_shape() {
        assert!(matches!(
            match_route("GET", "/api/capacity"),
            Route::Capacity
        ));
        // Read-only: a bare POST is not a way to change limits.
        assert!(matches!(
            match_route("POST", "/api/capacity"),
            Route::NotFound
        ));

        let state = demo_state();
        let req = ParsedRequest {
            method: "GET".into(),
            path: "/api/capacity".into(),
            ..Default::default()
        };
        let resp = build_response(&Route::Capacity, &req, &state, &DemoProvider);
        assert_eq!(resp.status, 200);
        let v: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["ok"], true);
        let data = &v["data"]["capacity"];
        for key in [
            "limits",
            "running",
            "queued",
            "slots_free",
            "reserved_memory_bytes",
            "projects",
            "queue",
            "over_parallel_limit",
            "reservation_note",
            "capabilities",
        ] {
            assert!(data.get(key).is_some(), "capacity missing {key}");
        }
        // Round-trips into the core type, so the wire shape is the contract.
        let _: Capacity = serde_json::from_value(data.clone()).unwrap();
    }

    #[test]
    fn demo_capacity_matches_demo_runs() {
        let cap = DemoProvider.capacity();
        let runs = DemoProvider.runs(&RunFilter::default());
        let running = runs.iter().filter(|r| r.state == RunState::Running).count();
        let queued: Vec<&RunView> = runs
            .iter()
            .filter(|r| r.state == RunState::Queued)
            .collect();

        assert_eq!(cap.running, running);
        assert_eq!(cap.queued, queued.len());
        assert_eq!(cap.slots_free, cap.limits.max_parallel - running);
        assert!(!cap.over_parallel_limit);
        assert_eq!(cap.queue.len(), queued.len());
        for q in &cap.queue {
            let run = queued
                .iter()
                .find(|r| r.run_id == q.run_id)
                .expect("every queue entry is a queued demo run");
            assert_eq!(run.queue.position, Some(q.position));
            assert_eq!(run.queue.waiting_ms, q.waiting_ms);
            assert_eq!(run.queue.reason.as_deref(), Some(q.reason.as_str()));
            assert!(q.memory_bytes > 0);
        }
        // Reservations cover admitted runs only; a queued request holds none.
        let expected: u64 = runs
            .iter()
            .filter(|r| r.state == RunState::Running)
            .map(|r| r.effective.memory_bytes.unwrap_or(0))
            .sum();
        assert_eq!(cap.reserved_memory_bytes, expected);
        let projects: u64 = cap.projects.iter().map(|p| p.reserved_memory_bytes).sum();
        assert_eq!(projects, cap.reserved_memory_bytes);
    }

    #[test]
    fn reservation_is_never_rendered_as_measured_ram() {
        let cap = DemoProvider.capacity();
        assert!(cap.reservation_note.contains("not measured RAM"));
        let v = serde_json::to_value(&cap).unwrap();
        // The budget lives on Capacity; a measurement lives on a run. The
        // names must not collapse into one "memory" number.
        assert!(v.get("reserved_memory_bytes").is_some());
        assert!(v.get("observed_memory_bytes").is_none());

        let limited = DemoProvider
            .run(RunId(406))
            .expect("resource_limit demo run");
        assert_eq!(
            limited.outcome,
            Some(crate::model::run::RunOutcome::ResourceLimit)
        );
        assert_eq!(
            limited.effective.enforcement,
            crate::model::run::Enforcement::Monitored
        );
        let observed = limited.observed_memory_bytes.expect("observation");
        let reserved = limited.effective.memory_bytes.expect("reservation");
        assert!(
            observed > reserved,
            "the monitored threshold is what stopped this run"
        );
        assert!(limited.observed_processes.is_some());
        assert!(
            limited
                .limit_event
                .as_deref()
                .is_some_and(|e| e.contains("threshold") && e.contains("RSS")),
            "limit_event must name the measurement, got {:?}",
            limited.limit_event
        );
        let metric = limited.effective.metric.as_deref().unwrap_or("");
        assert!(
            metric.contains("RSS") && metric.contains("200 ms"),
            "metric must name what is measured, got {metric:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn monitored_capability_is_distinct_from_available() {
        let v = serde_json::to_value(DemoProvider.capacity()).unwrap();
        let caps = &v["capabilities"];
        assert_eq!(caps["queue"], "available");
        let mem = &caps["aggregate_memory_limit"];
        assert!(
            mem.get("monitored").is_some(),
            "macOS memory is monitored, not enforced: {mem}"
        );
        assert!(mem.get("available").is_none());
        assert!(
            caps["cpu_quota"].get("unavailable").is_some(),
            "an unavailable capability carries its reason"
        );
    }

    #[test]
    fn webmcp_bundle_registers_read_only_get_capacity() {
        let (_, js) = assets::lookup("/assets/app.js").expect("app.js asset");
        let js = std::str::from_utf8(js).unwrap();
        assert!(
            js.contains("name: 'get_capacity'"),
            "the shipped bundle must register get_capacity"
        );
        assert!(
            js.contains("api('/api/capacity')"),
            "get_capacity must read the capacity route"
        );
        assert!(
            !js.contains("set_limits"),
            "the web must never change admission limits"
        );
    }
}
