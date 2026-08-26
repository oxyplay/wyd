//! Durable runtime ownership store (SQLite, local).
//!
//! Persists exactly what Wyd observed: sessions, owned resources, their
//! member processes, and the exact ownership edges. No resolver candidates,
//! scores or heuristic evidence yet — those arrive with PHASE 2.
//!
//! Provenance survives both process ancestry loss and Wyd restarts because
//! process identity is `boot_id + pid + start_time`, not PID alone.

use crate::classify::ownership::OwnershipResult;
use crate::classify::ownership::resolver::{AttributionDecision, Evidence, EvidenceKind};
use crate::model::boot::{BootEpoch, BootId};
use crate::model::process::ProcessIdentity;
use crate::model::session::RuntimeSessionId;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SCHEMA_VERSION: i64 = 1;

/// Local SQLite store for runtime ownership provenance.
pub struct RuntimeStore {
    conn: Connection,
}

impl RuntimeStore {
    /// Open (creating) the store at `path`.
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).map_err(err)?;
        conn.busy_timeout(Duration::from_secs(2)).map_err(err)?;
        // WAL: concurrent readers (TUI/CLI/MCP) don't block the writer, and
        // the writer (serve/tracker) doesn't block readers.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .map_err(err)?;
        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    /// In-memory store, for tests.
    #[cfg(test)]
    pub fn open_in_memory() -> io::Result<Self> {
        let conn = Connection::open_in_memory().map_err(err)?;
        conn.busy_timeout(Duration::from_secs(2)).map_err(err)?;
        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    /// The default platform state location:
    /// `$XDG_DATA_HOME/wyd/state.db`, or `~/Library/Application Support/wyd`
    /// on macOS.
    pub fn default_path() -> PathBuf {
        if cfg!(target_os = "macos") {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join("Library/Application Support/wyd/state.db")
        } else {
            let xdg = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_default();
                format!("{home}/.local/share")
            });
            PathBuf::from(xdg).join("wyd/state.db")
        }
    }

    fn init(&self) -> io::Result<()> {
        self.conn
            .execute_batch(
                "BEGIN;
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS boots (
                boot_id BLOB PRIMARY KEY,
                platform_epoch BLOB NOT NULL,
                first_seen INTEGER NOT NULL,
                last_seen INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS sessions (
                session_id INTEGER PRIMARY KEY,
                boot_id BLOB NOT NULL,
                agent TEXT NOT NULL,
                root_pid INTEGER NOT NULL,
                root_start_time INTEGER NOT NULL,
                project TEXT,
                started_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL,
                ended_at INTEGER
            );
            CREATE TABLE IF NOT EXISTS resources (
                resource_id INTEGER PRIMARY KEY,
                kind TEXT NOT NULL,
                root_boot_id BLOB NOT NULL,
                root_pid INTEGER NOT NULL,
                root_start_time INTEGER NOT NULL,
                first_seen_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL,
                stopped_at INTEGER
            );
            CREATE TABLE IF NOT EXISTS resource_members (
                resource_id INTEGER NOT NULL,
                boot_id BLOB NOT NULL,
                pid INTEGER NOT NULL,
                start_time INTEGER NOT NULL,
                PRIMARY KEY (resource_id, boot_id, pid, start_time)
            );
            CREATE TABLE IF NOT EXISTS exact_ownership (
                resource_id INTEGER PRIMARY KEY,
                session_id INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS attribution_decisions (
                decision_id INTEGER PRIMARY KEY AUTOINCREMENT,
                resource_id INTEGER NOT NULL,
                observed_at INTEGER NOT NULL,
                resolver_version INTEGER NOT NULL,
                verdict TEXT NOT NULL,
                winner_session_id INTEGER
            );
            CREATE TABLE IF NOT EXISTS attribution_candidates (
                decision_id INTEGER NOT NULL,
                session_id INTEGER NOT NULL,
                anchor_kind TEXT NOT NULL,
                anchor_score INTEGER NOT NULL,
                project_score INTEGER NOT NULL,
                temporal_score INTEGER NOT NULL,
                relationship_score INTEGER NOT NULL,
                total_score INTEGER NOT NULL,
                rejected_reason TEXT,
                PRIMARY KEY (decision_id, session_id)
            );
            CREATE TABLE IF NOT EXISTS evidence (
                decision_id INTEGER NOT NULL,
                session_id INTEGER NOT NULL,
                kind TEXT NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY (decision_id, session_id, kind)
            );
            CREATE TABLE IF NOT EXISTS session_aliases (
                vendor TEXT NOT NULL,
                vendor_session_id TEXT NOT NULL,
                session_id INTEGER NOT NULL,
                vendor_started_at INTEGER,
                vendor_ended_at INTEGER,
                PRIMARY KEY (vendor, vendor_session_id)
            );
            COMMIT;",
            )
            .map_err(err)?;

        // Idempotent migrations for tables created before a column existed.
        // Keep existing DBs usable without a full re-create.
        self.add_column_if_missing(
            "session_aliases",
            "vendor_started_at",
            "vendor_started_at INTEGER",
        )?;
        self.add_column_if_missing(
            "session_aliases",
            "vendor_ended_at",
            "vendor_ended_at INTEGER",
        )?;

        match self.meta("schema_version")? {
            Some(v) if v != SCHEMA_VERSION.to_string() => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "state.db schema version {v} != supported {SCHEMA_VERSION}; \
                     remove or migrate the file"
                ),
            )),
            _ => {
                self.set_meta("schema_version", &SCHEMA_VERSION.to_string())?;
                Ok(())
            }
        }
    }

    /// Add `column` to `table` if it does not already exist (idempotent
    /// migration for DBs created before the column).
    fn add_column_if_missing(&self, table: &str, column: &str, ddl: &str) -> io::Result<()> {
        let sql = format!("PRAGMA table_info({table})");
        let mut stmt = self.conn.prepare(&sql).map_err(err)?;
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(err)?
            .collect::<rusqlite::Result<_>>()
            .map_err(err)?;
        drop(stmt);
        if cols.iter().any(|c| c == column) {
            return Ok(());
        }
        self.conn
            .execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {ddl}"))
            .map_err(err)?;
        Ok(())
    }

    fn meta(&self, key: &str) -> io::Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM meta WHERE key = ?1")
            .map_err(err)?;
        let mut rows = stmt.query_map(params![key], |r| r.get(0)).map_err(err)?;
        rows.next().transpose().map_err(err).map(|o| o.flatten())
    }

    fn set_meta(&self, key: &str, value: &str) -> io::Result<()> {
        self.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(err)?;
        Ok(())
    }

    /// Resolve a platform boot epoch to a stable `BootId`, persisting the
    /// macOS epoch→UUID mapping. Linux uses the boot UUID directly.
    pub fn boot_id_for_epoch(&mut self, epoch: BootEpoch, now: u64) -> io::Result<BootId> {
        epoch.to_boot_id(|e| {
            self.resolve_macos_epoch(e, now)
                .map_err(|e| io::Error::other(e.to_string()))
        })
    }

    fn resolve_macos_epoch(&mut self, epoch: BootEpoch, now: u64) -> rusqlite::Result<BootId> {
        let bytes = epoch.to_bytes();
        let existing = self
            .conn
            .query_row(
                "SELECT boot_id FROM boots WHERE platform_epoch = ?1",
                params![bytes],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        if let Some(b) = existing {
            self.conn.execute(
                "UPDATE boots SET last_seen = ?1 WHERE boot_id = ?2",
                params![now as i64, b],
            )?;
            return Ok(bytes_to_boot(&b));
        }
        let id = BootId::random();
        let ib = id.to_le_bytes().to_vec();
        self.conn.execute(
            "INSERT INTO boots (boot_id, platform_epoch, first_seen, last_seen)
             VALUES (?1, ?2, ?3, ?3)",
            params![ib, bytes, now as i64],
        )?;
        Ok(id)
    }

    /// Persist the exact-observed sessions and owned resources of one pass.
    pub fn apply_ownership(&mut self, result: &OwnershipResult, now: u64) -> io::Result<()> {
        let tx = self.conn.transaction().map_err(err)?;
        for s in &result.sessions {
            let project = s
                .project
                .as_ref()
                .map(|p| p.root.to_string_lossy().to_string());
            tx.execute(
                "INSERT INTO sessions
                   (session_id, boot_id, agent, root_pid, root_start_time,
                    project, started_at, last_seen_at, ended_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(session_id) DO UPDATE SET
                    last_seen_at = excluded.last_seen_at,
                    ended_at = excluded.ended_at,
                    project = COALESCE(excluded.project, sessions.project)",
                params![
                    s.id.as_u64() as i64,
                    s.root.boot_id.to_le_bytes().to_vec(),
                    s.agent,
                    s.root.pid as i64,
                    s.root.start_time as i64,
                    project,
                    s.started_at as i64,
                    now as i64,
                    s.ended_at.map(|e| e as i64),
                ],
            )
            .map_err(err)?;
        }

        for o in &result.owned {
            tx.execute(
                "INSERT INTO resources
                   (resource_id, kind, root_boot_id, root_pid, root_start_time,
                    first_seen_at, last_seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
                 ON CONFLICT(resource_id) DO UPDATE SET
                    last_seen_at = excluded.last_seen_at",
                params![
                    o.resource.as_u64() as i64,
                    format!("{:?}", o.kind),
                    o.root.boot_id.to_le_bytes().to_vec(),
                    o.root.pid as i64,
                    o.root.start_time as i64,
                    now as i64,
                ],
            )
            .map_err(err)?;

            tx.execute(
                "DELETE FROM resource_members WHERE resource_id = ?1",
                params![o.resource.as_u64() as i64],
            )
            .map_err(err)?;
            for m in &o.members {
                tx.execute(
                    "INSERT INTO resource_members (resource_id, boot_id, pid, start_time)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        o.resource.as_u64() as i64,
                        m.boot_id.to_le_bytes().to_vec(),
                        m.pid as i64,
                        m.start_time as i64,
                    ],
                )
                .map_err(err)?;
            }

            tx.execute(
                "INSERT INTO exact_ownership (resource_id, session_id)
                 VALUES (?1, ?2)
                 ON CONFLICT(resource_id) DO UPDATE SET session_id = excluded.session_id",
                params![o.resource.as_u64() as i64, o.session.as_u64() as i64],
            )
            .map_err(err)?;
        }
        tx.commit().map_err(err)?;
        Ok(())
    }

    /// Strict `boot_id + pid + start_time` lookup: the session that owns the
    /// resource containing exactly this process invocation, if any.
    /// A start_time mismatch never matches — it is a different process.
    pub fn owning_session_for_process(
        &self,
        boot_id: &BootId,
        pid: u32,
        start_time: u64,
    ) -> io::Result<Option<RuntimeSessionId>> {
        let boot = boot_id.to_le_bytes().to_vec();
        let session: Option<i64> = self
            .conn
            .query_row(
                "SELECT o.session_id
                 FROM resource_members m
                 JOIN exact_ownership o ON o.resource_id = m.resource_id
                 WHERE m.boot_id = ?1 AND m.pid = ?2 AND m.start_time = ?3
                 UNION
                 SELECT o.session_id
                 FROM resources r
                 JOIN exact_ownership o ON o.resource_id = r.resource_id
                 WHERE r.root_boot_id = ?1 AND r.root_pid = ?2
                   AND r.root_start_time = ?3",
                params![boot, pid as i64, start_time as i64],
                |row| row.get(0),
            )
            .optional()
            .map_err(err)?;
        Ok(session.map(|s| RuntimeSessionId::from_u64(s as u64)))
    }

    /// Mark any active session whose root process is no longer live as ended.
    /// `live_roots` are the exact root identities observed in the current
    /// scan. A session whose root is absent has ended; `ended_at` is the first
    /// observation time where the process was absent.
    pub fn end_absent_sessions(
        &mut self,
        live_roots: &std::collections::HashSet<ProcessIdentity>,
        now: u64,
    ) -> io::Result<()> {
        let tx = self.conn.transaction().map_err(err)?;
        let mut stmt = tx
            .prepare(
                "SELECT session_id, boot_id, root_pid, root_start_time
                      FROM sessions WHERE ended_at IS NULL",
            )
            .map_err(err)?;
        let active: Vec<(i64, Vec<u8>, i64, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(err)?
            .collect::<rusqlite::Result<_>>()
            .map_err(err)?;
        drop(stmt);

        for (id, boot, pid, start) in active {
            let root = ProcessIdentity {
                boot_id: bytes_to_boot(&boot),
                pid: pid as u32,
                start_time: start as u64,
            };
            if !live_roots.contains(&root) {
                tx.execute(
                    "UPDATE sessions SET ended_at = ?1 WHERE session_id = ?2",
                    params![now as i64, id],
                )
                .map_err(err)?;
            }
        }
        tx.commit().map_err(err)?;
        Ok(())
    }

    /// Delete provenance older than `retention_secs`: ended sessions and their
    /// now-orphaned, old resources. A session is kept while any resource that
    /// references it has a member (or root) process still alive on the system,
    /// so a survived orphan keeps its origin regardless of age.
    ///
    /// `live` is the set of process identities currently observed by the
    /// collector — the real liveness signal, not retention arithmetic.
    pub fn gc(
        &mut self,
        retention_secs: u64,
        now: u64,
        live: &HashSet<ProcessIdentity>,
    ) -> io::Result<()> {
        let cutoff = now.saturating_sub(retention_secs) as i64;
        let tx = self.conn.transaction().map_err(err)?;

        // Which resources have a live member or root?
        let mut live_resources: HashSet<i64> = HashSet::new();
        let roots = quads(
            &tx,
            "SELECT resource_id, root_boot_id, root_pid, root_start_time FROM resources",
            None,
        )?;
        for (rid, boot, pid, start) in roots {
            if live.contains(&ProcessIdentity {
                boot_id: bytes_to_boot(&boot),
                pid: pid as u32,
                start_time: start as u64,
            }) {
                live_resources.insert(rid);
            }
        }
        let members = quads(
            &tx,
            "SELECT resource_id, boot_id, pid, start_time FROM resource_members",
            None,
        )?;
        for (rid, boot, pid, start) in members {
            if live.contains(&ProcessIdentity {
                boot_id: bytes_to_boot(&boot),
                pid: pid as u32,
                start_time: start as u64,
            }) {
                live_resources.insert(rid);
            }
        }

        // Sessions referenced by a live resource are protected.
        let mut protected: HashSet<i64> = HashSet::new();
        let edges: Vec<(i64, i64)> = pairs(
            &tx,
            "SELECT resource_id, session_id FROM exact_ownership",
            None,
        )?;
        for (rid, sid) in edges {
            if live_resources.contains(&rid) {
                protected.insert(sid);
            }
        }

        // Delete ended, old, unprotected sessions and their ownership edges.
        let candidates: Vec<i64> = scalars(
            &tx,
            "SELECT session_id FROM sessions WHERE ended_at IS NOT NULL AND ended_at < ?1",
            Some(&cutoff),
        )?;
        for sid in candidates {
            if protected.contains(&sid) {
                continue;
            }
            tx.execute("DELETE FROM sessions WHERE session_id = ?1", params![sid])
                .map_err(err)?;
            tx.execute(
                "DELETE FROM exact_ownership WHERE session_id = ?1",
                params![sid],
            )
            .map_err(err)?;
        }

        // Old orphaned resources (no ownership edge) and their members.
        tx.execute(
            "DELETE FROM resources
             WHERE resource_id NOT IN (SELECT resource_id FROM exact_ownership)
               AND last_seen_at < ?1",
            params![cutoff],
        )
        .map_err(err)?;
        tx.execute(
            "DELETE FROM resource_members
             WHERE resource_id NOT IN (SELECT resource_id FROM resources)",
            params![],
        )
        .map_err(err)?;
        tx.commit().map_err(err)?;
        Ok(())
    }

    /// Persist one resolver decision with its full candidate set (§16).
    ///
    /// This is the durable layer: verdict + candidate scores + resolver
    /// version never expire. Raw evidence (best-effort) is not stored here.
    pub fn persist_decision(
        &mut self,
        resource_id: u64,
        d: &AttributionDecision,
        now: u64,
    ) -> io::Result<()> {
        let tx = self.conn.transaction().map_err(err)?;
        tx.execute(
            "INSERT INTO attribution_decisions
               (resource_id, observed_at, resolver_version, verdict, winner_session_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                resource_id as i64,
                now as i64,
                d.resolver_version as i64,
                format!("{:?}", d.verdict),
                d.winner.map(|w| w.as_u64() as i64),
            ],
        )
        .map_err(err)?;
        let decision_id = tx.last_insert_rowid();
        for c in &d.candidates {
            let sid = c.session.as_u64() as i64;
            tx.execute(
                "INSERT INTO attribution_candidates
                   (decision_id, session_id, anchor_kind, anchor_score,
                    project_score, temporal_score, relationship_score,
                    total_score, rejected_reason)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    decision_id,
                    sid,
                    format!("{:?}", c.anchor),
                    c.anchor_score as i64,
                    c.project_support as i64,
                    c.temporal_support as i64,
                    c.relationship_support as i64,
                    c.total as i64,
                    c.rejected.map(|r| format!("{:?}", r)),
                ],
            )
            .map_err(err)?;
            for e in &c.evidence {
                tx.execute(
                    "INSERT INTO evidence (decision_id, session_id, kind, value)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(decision_id, session_id, kind)
                     DO UPDATE SET value = excluded.value",
                    params![decision_id, sid, e.kind.as_str(), e.value],
                )
                .map_err(err)?;
            }
        }
        tx.commit().map_err(err)?;
        Ok(())
    }

    /// All known sessions (including ended, pending GC).
    pub fn sessions(&self) -> io::Result<Vec<SessionRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT session_id, agent, project, started_at, last_seen_at, ended_at
                 FROM sessions",
            )
            .map_err(err)?;
        let rows: rusqlite::Result<Vec<_>> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                ))
            })
            .map_err(err)?
            .collect();
        drop(stmt);
        rows.map(|v| {
            v.into_iter()
                .map(
                    |(id, agent, project, started, last_seen, ended)| SessionRecord {
                        id: RuntimeSessionId::from_u64(id as u64),
                        agent,
                        project,
                        started_at: started as u64,
                        last_seen_at: last_seen as u64,
                        ended_at: ended.map(|e| e as u64),
                    },
                )
                .collect()
        })
        .map_err(err)
    }

    /// One session's durable record, or `None`.
    pub fn session_record(&self, id: RuntimeSessionId) -> io::Result<Option<SessionRecord>> {
        let row: Option<SessionRow> = self
            .conn
            .query_row(
                "SELECT agent, project, started_at, last_seen_at, ended_at
                 FROM sessions WHERE session_id = ?1",
                params![id.as_u64() as i64],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .map_err(err)?;
        Ok(row.map(
            |(agent, project, started, last_seen, ended)| SessionRecord {
                id,
                agent,
                project,
                started_at: started as u64,
                last_seen_at: last_seen as u64,
                ended_at: ended.map(|e| e as u64),
            },
        ))
    }

    /// The session whose root is exactly this process invocation, if any.
    /// Used by `wyd why` for an agent-root pid.
    pub fn session_for_root(
        &self,
        boot_id: &BootId,
        pid: u32,
        start_time: u64,
    ) -> io::Result<Option<SessionRecord>> {
        let boot = boot_id.to_le_bytes().to_vec();
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT session_id FROM sessions
                 WHERE boot_id = ?1 AND root_pid = ?2 AND root_start_time = ?3",
                params![boot, pid as i64, start_time as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)?;
        match id {
            Some(id) => self.session_record(RuntimeSessionId::from_u64(id as u64)),
            None => Ok(None),
        }
    }

    /// Ensure a session exists for an explicitly registered agent process
    /// (vendor `session_start`, contract §17). Inserts if absent and returns
    /// its Wyd id.
    pub fn ensure_session(
        &mut self,
        boot: &BootId,
        agent: &str,
        pid: u32,
        start_time: u64,
        now: u64,
    ) -> io::Result<RuntimeSessionId> {
        let id = RuntimeSessionId::new(
            boot,
            &ProcessIdentity {
                boot_id: *boot,
                pid,
                start_time,
            },
            agent,
        );
        self.conn
            .execute(
                "INSERT OR IGNORE INTO sessions
                   (session_id, boot_id, agent, root_pid, root_start_time,
                    started_at, last_seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                params![
                    id.as_u64() as i64,
                    boot.to_le_bytes().to_vec(),
                    agent,
                    pid as i64,
                    start_time as i64,
                    now as i64,
                ],
            )
            .map_err(err)?;
        Ok(id)
    }

    /// Map a vendor session id to the Wyd session (metadata, §2 — does not
    /// redefine Wyd's local identity).
    pub fn register_alias(
        &mut self,
        session_id: RuntimeSessionId,
        vendor: &str,
        vendor_session_id: &str,
        now: u64,
    ) -> io::Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO session_aliases
                   (vendor, vendor_session_id, session_id, vendor_started_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    vendor,
                    vendor_session_id,
                    session_id.as_u64() as i64,
                    now as i64
                ],
            )
            .map_err(err)?;
        Ok(())
    }

    /// Record a vendor session as ended. This is **metadata** on the alias —
    /// it does not end the Wyd runtime session (which ends only on process
    /// death). A vendor session can be one chat/task within a longer-lived
    /// agent process (§17).
    pub fn end_vendor_alias(
        &mut self,
        vendor: &str,
        vendor_session_id: &str,
        now: u64,
    ) -> io::Result<()> {
        self.conn
            .execute(
                "UPDATE session_aliases SET vendor_ended_at = ?1
                 WHERE vendor = ?2 AND vendor_session_id = ?3",
                params![now as i64, vendor, vendor_session_id],
            )
            .map_err(err)?;
        Ok(())
    }

    pub fn session_id_for_alias(
        &self,
        vendor: &str,
        vendor_session_id: &str,
    ) -> io::Result<Option<RuntimeSessionId>> {
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT session_id FROM session_aliases
                 WHERE vendor = ?1 AND vendor_session_id = ?2",
                params![vendor, vendor_session_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)?;
        Ok(id.map(|i| RuntimeSessionId::from_u64(i as u64)))
    }

    /// Which known resource owns a process invocation, plus that session —
    /// the restore/`wyd why` primitive.
    pub fn explain_process(
        &self,
        boot_id: &BootId,
        pid: u32,
        start_time: u64,
    ) -> io::Result<Option<Explanation>> {
        let Some(owner) = self.owning_session_for_process(boot_id, pid, start_time)? else {
            return Ok(None);
        };
        let boot = boot_id.to_le_bytes().to_vec();
        let resource_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT resource_id FROM (
                    SELECT m.resource_id FROM resource_members m
                    WHERE m.boot_id = ?1 AND m.pid = ?2 AND m.start_time = ?3
                    UNION
                    SELECT r.resource_id FROM resources r
                    WHERE r.root_boot_id = ?1 AND r.root_pid = ?2
                      AND r.root_start_time = ?3
                )",
                params![boot, pid as i64, start_time as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)?;
        let Some(resource_id) = resource_id else {
            return Ok(None);
        };
        let session = self
            .session_record(owner)?
            .ok_or_else(|| io::Error::other("ownership references a missing session row"))?;
        Ok(Some(Explanation {
            resource_id: resource_id as u64,
            owner,
            session,
        }))
    }

    /// How many decisions have been persisted for a resource.
    #[cfg(test)]
    pub fn decision_count(&self, resource_id: u64) -> io::Result<u64> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT count(*) FROM attribution_decisions WHERE resource_id = ?1",
                params![resource_id as i64],
                |r| r.get(0),
            )
            .map_err(err)?;
        Ok(n as u64)
    }

    /// The most recent resolver decision for a resource, if any.
    pub fn latest_decision(&self, resource_id: u64) -> io::Result<Option<DecisionRecord>> {
        let head: Option<(i64, i64, String, Option<i64>)> = self
            .conn
            .query_row(
                "SELECT decision_id, resolver_version, verdict, winner_session_id
                 FROM attribution_decisions WHERE resource_id = ?1
                 ORDER BY observed_at DESC, decision_id DESC LIMIT 1",
                params![resource_id as i64],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(err)?;
        let Some((decision_id, version, verdict, winner)) = head else {
            return Ok(None);
        };
        let candidates = {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT session_id, anchor_kind, anchor_score, project_score,
                            temporal_score, relationship_score, total_score,
                            rejected_reason
                     FROM attribution_candidates WHERE decision_id = ?1
                     ORDER BY total_score DESC",
                )
                .map_err(err)?;
            let rows: rusqlite::Result<Vec<_>> = stmt
                .query_map(params![decision_id], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, Option<String>>(7)?,
                    ))
                })
                .map_err(err)?
                .collect();
            let mut cands = rows
                .map_err(err)?
                .into_iter()
                .map(|(sid, anchor, a, p, t, rel, total, rejected)| {
                    (
                        sid,
                        CandidateRecord {
                            session: RuntimeSessionId::from_u64(sid as u64),
                            anchor_kind: anchor,
                            anchor_score: a as u8,
                            project_support: p as u8,
                            temporal_support: t as u8,
                            relationship_support: rel as u8,
                            total: total as u8,
                            rejected_reason: rejected,
                            evidence: Vec::new(),
                        },
                    )
                })
                .collect::<Vec<_>>();
            let mut by_sid: HashMap<i64, &mut CandidateRecord> =
                cands.iter_mut().map(|(sid, c)| (*sid, c)).collect();
            let mut estmt = self
                .conn
                .prepare("SELECT session_id, kind, value FROM evidence WHERE decision_id = ?1")
                .map_err(err)?;
            let erows: Vec<(i64, String, String)> = estmt
                .query_map(params![decision_id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .map_err(err)?
                .collect::<rusqlite::Result<_>>()
                .map_err(err)?;
            drop(estmt);
            for (sid, kind, value) in erows {
                if let Some(c) = by_sid.get_mut(&sid) {
                    let kind = match kind.as_str() {
                        "persisted ownership" => EvidenceKind::PersistedOwnership,
                        "cwd match" => EvidenceKind::CwdMatch,
                        "git root" => EvidenceKind::GitRoot,
                        "start-time correlation" => EvidenceKind::StartTimeCorrelation,
                        "known tool relationship" => EvidenceKind::ToolRelationship,
                        "predates session" => EvidenceKind::PredatesSession,
                        "reaches another agent" => EvidenceKind::ReachesOtherAgent,
                        _ => continue,
                    };
                    c.evidence.push(Evidence { kind, value });
                }
            }
            cands.into_iter().map(|(_, c)| c).collect()
        };
        Ok(Some(DecisionRecord {
            resolver_version: version as u32,
            verdict,
            winner_session: winner.map(|w| RuntimeSessionId::from_u64(w as u64)),
            candidates,
        }))
    }
}

/// One session's durable record.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: RuntimeSessionId,
    pub agent: String,
    pub project: Option<String>,
    pub started_at: u64,
    pub last_seen_at: u64,
    pub ended_at: Option<u64>,
}

/// Which resource owns a process invocation, and that session.
#[derive(Debug, Clone)]
pub struct Explanation {
    pub resource_id: u64,
    pub owner: RuntimeSessionId,
    pub session: SessionRecord,
}

/// One resolver decision with its full candidate set (§16).
#[derive(Debug, Clone)]
pub struct DecisionRecord {
    pub resolver_version: u32,
    pub verdict: String,
    pub winner_session: Option<RuntimeSessionId>,
    pub candidates: Vec<CandidateRecord>,
}

#[derive(Debug, Clone)]
pub struct CandidateRecord {
    pub session: RuntimeSessionId,
    pub anchor_kind: String,
    pub anchor_score: u8,
    pub project_support: u8,
    pub temporal_support: u8,
    pub relationship_support: u8,
    pub total: u8,
    pub rejected_reason: Option<String>,
    pub evidence: Vec<Evidence>,
}

/// `(resource_id, boot_id/root_boot_id, pid, start_time)` rows.
type Quad = (i64, Vec<u8>, i64, i64);

/// `(agent, project, started_at, last_seen_at, ended_at)` row.
type SessionRow = (String, Option<String>, i64, i64, Option<i64>);

fn quads(conn: &Connection, sql: &str, arg: Option<&i64>) -> io::Result<Vec<Quad>> {
    query_rows(conn, sql, arg, |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    })
}

/// Two-column rows.
fn pairs(conn: &Connection, sql: &str, arg: Option<&i64>) -> io::Result<Vec<(i64, i64)>> {
    query_rows(conn, sql, arg, |r| Ok((r.get(0)?, r.get(1)?)))
}

/// Single-column rows.
fn scalars(conn: &Connection, sql: &str, arg: Option<&i64>) -> io::Result<Vec<i64>> {
    query_rows(conn, sql, arg, |r| r.get(0))
}

/// Run `sql` (with optional `?1` bound), mapping each row with `map`.
fn query_rows<T>(
    conn: &Connection,
    sql: &str,
    arg: Option<&i64>,
    map: impl Fn(&rusqlite::Row) -> rusqlite::Result<T>,
) -> io::Result<Vec<T>> {
    let mut stmt = conn.prepare(sql).map_err(err)?;
    let rows = match arg {
        Some(a) => stmt
            .query_map(params![a], &map)
            .map_err(err)?
            .collect::<rusqlite::Result<Vec<T>>>(),
        None => stmt
            .query_map([], &map)
            .map_err(err)?
            .collect::<rusqlite::Result<Vec<T>>>(),
    }
    .map_err(err)?;
    drop(stmt);
    Ok(rows)
}

fn bytes_to_boot(b: &[u8]) -> BootId {
    let mut arr = [0u8; 16];
    let n = b.len().min(16);
    arr[..n].copy_from_slice(&b[..n]);
    BootId::from_le_bytes(arr)
}

fn err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::group;
    use crate::classify::ownership::derive_ownership;
    use crate::model::ProcessInfo;
    use crate::model::process::ProcessIdentity;

    fn proc(pid: u32, ppid: Option<u32>, name: &str, cmd: &[&str], start: u64) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid: ppid,
            name: name.into(),
            command: cmd.iter().map(|s| (*s).to_string()).collect(),
            executable: None,
            cwd: None,
            cpu_percent: 0.0,
            memory_bytes: 40 << 20,
            start_time: start,
            tty: None,
        }
    }

    fn identities(
        procs: &[ProcessInfo],
        boot: &BootId,
    ) -> std::collections::HashMap<u32, ProcessIdentity> {
        procs
            .iter()
            .filter_map(|p| ProcessIdentity::from_process(boot, p).map(|id| (p.pid, id)))
            .collect()
    }

    fn apply_chain(store: &mut RuntimeStore, now: u64) {
        let procs = vec![
            proc(1, None, "launchd", &["launchd"], 1),
            proc(100, Some(1), "omp", &["omp"], 1000),
            proc(
                110,
                Some(100),
                "node",
                &["node", "chrome-devtools-mcp"],
                1004,
            ),
            proc(120, Some(110), "Chromium", &["Chromium"], 1007),
        ];
        let boot = BootId::from_u128(7);
        let items = group(&procs);
        let out = derive_ownership(&items, &identities(&procs, &boot), now);
        store.apply_ownership(&out, now).unwrap();
    }

    #[test]
    fn persists_sessions_and_ownership() {
        let mut store = RuntimeStore::open_in_memory().unwrap();
        apply_chain(&mut store, 5000);

        let boot = BootId::from_u128(7);
        // Chromium (a member, not the root) is matched by strict identity.
        let owner = store
            .owning_session_for_process(&boot, 120, 1007)
            .unwrap()
            .expect("Chromium should be owned");
        let _ = owner;
        // The MCP root process is also owned.
        let owner_root = store
            .owning_session_for_process(&boot, 110, 1004)
            .unwrap()
            .expect("MCP root should be owned");
        assert_eq!(owner, owner_root);
    }

    #[test]
    fn start_time_mismatch_never_matches() {
        let mut store = RuntimeStore::open_in_memory().unwrap();
        apply_chain(&mut store, 5000);

        let boot = BootId::from_u128(7);
        // Same PID, different start_time = a different process invocation.
        assert!(
            store
                .owning_session_for_process(&boot, 120, 9999)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn different_boot_never_matches() {
        let mut store = RuntimeStore::open_in_memory().unwrap();
        apply_chain(&mut store, 5000);

        // Same pid/start_time but a different boot.
        let other = BootId::from_u128(99);
        assert!(
            store
                .owning_session_for_process(&other, 120, 1007)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn macos_epoch_maps_to_stable_uuid() {
        let mut store = RuntimeStore::open_in_memory().unwrap();
        let epoch = BootEpoch::Macos { sec: 1000, usec: 5 };
        let a = store.boot_id_for_epoch(epoch, 5000).unwrap();
        let b = store.boot_id_for_epoch(epoch, 6000).unwrap();
        assert_eq!(a, b, "same boot epoch → same persisted BootId");

        let other = BootEpoch::Macos { sec: 2000, usec: 0 };
        let c = store.boot_id_for_epoch(other, 7000).unwrap();
        assert_ne!(a, c, "different epoch → new BootId");
    }

    #[test]
    fn linux_epoch_uses_uuid_directly() {
        let mut store = RuntimeStore::open_in_memory().unwrap();
        let epoch = BootEpoch::Linux(0x1234);
        let id = store.boot_id_for_epoch(epoch, 5000).unwrap();
        assert_eq!(id, BootId::from_u128(0x1234));
    }

    #[test]
    fn schema_version_mismatch_is_rejected() {
        let dir = std::env::temp_dir().join(format!("wyd-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        {
            let store = RuntimeStore::open(&path).unwrap();
            store.set_meta("schema_version", "999").unwrap();
        }
        let reopened = RuntimeStore::open(&path);
        assert!(
            reopened.is_err(),
            "future schema must be rejected, not silently read"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn provenance_survives_wyd_restart() {
        // Run 1: Wyd observes Claude → Playwright → Chromium and persists it,
        // then exits (store dropped).
        let dir = std::env::temp_dir().join(format!("wyd-restore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        let boot = BootId::from_u128(7);
        {
            let mut store = RuntimeStore::open(&path).unwrap();
            apply_chain(&mut store, 5000);
        }

        // Run 2: Claude and Playwright are gone; only Chromium survives,
        // re-parented under launchd. OS ancestry cannot recover Claude, but
        // the store matches the surviving process by strict identity.
        let store = RuntimeStore::open(&path).unwrap();
        let owner = store
            .owning_session_for_process(&boot, 120, 1007)
            .expect("store query succeeds")
            .expect("Chromium's origin session must survive a Wyd restart");
        let _ = owner;

        // A different process invocation (PID reuse) is not bridged.
        assert!(
            store
                .owning_session_for_process(&boot, 120, 9999)
                .unwrap()
                .is_none()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ended_session_is_kept_while_a_surviving_member_is_alive() {
        let mut store = RuntimeStore::open_in_memory().unwrap();
        apply_chain(&mut store, 1000);

        // The agent dies; Chromium survives (re-parented). No live roots.
        let gone = std::collections::HashSet::new();
        store.end_absent_sessions(&gone, 2000).unwrap();

        // Chromium's process is still alive on the system → its resource is
        // live → the origin session must survive GC regardless of age.
        let boot = BootId::from_u128(7);
        let live = std::collections::HashSet::from([ProcessIdentity {
            boot_id: boot,
            pid: 120,
            start_time: 1007,
        }]);
        store.gc(0, 1_000_000, &live).unwrap();
        assert!(
            store
                .owning_session_for_process(&boot, 120, 1007)
                .unwrap()
                .is_some(),
            "surviving member must keep its origin session across GC"
        );
    }

    #[test]
    fn ended_session_with_no_live_member_is_gced() {
        let mut store = RuntimeStore::open_in_memory().unwrap();
        apply_chain(&mut store, 1000);

        // Everything died: no live roots and no surviving member.
        let gone = std::collections::HashSet::new();
        store.end_absent_sessions(&gone, 2000).unwrap();
        let live = std::collections::HashSet::new();
        store.gc(0, 3000, &live).unwrap();
        let boot = BootId::from_u128(7);
        assert!(
            store
                .owning_session_for_process(&boot, 120, 1007)
                .unwrap()
                .is_none(),
            "fully-dead provenance must be collected"
        );
    }

    #[test]
    fn persists_resolver_decision_and_candidates() {
        use crate::classify::ownership::resolver::{
            AnchorKind, CandidateInput, ResolverRules, resolve,
        };
        let mut store = RuntimeStore::open_in_memory().unwrap();

        let sid = RuntimeSessionId::from_u64(1);
        let c = CandidateInput {
            session: sid,
            anchor: Some(AnchorKind::Descendant),
            anchor_score: None,
            propagate_cap: None,
            exact_cwd: true,
            same_git_root: false,
            start_delta_secs: Some(7),
            tool_relationship: true,
            predates_session: false,
            reaches_other_agent: false,
        };
        let decision = resolve(&ResolverRules::v1(), vec![c]);
        assert_eq!(
            decision.verdict,
            crate::classify::ownership::resolver::Verdict::Owned
        );

        store.persist_decision(42, &decision, 5000).unwrap();

        let verdict: String = store
            .conn
            .query_row(
                "SELECT verdict FROM attribution_decisions WHERE resource_id = 42",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(verdict, "Owned");
        let (winner, version): (i64, i64) = store
            .conn
            .query_row(
                "SELECT winner_session_id, resolver_version
                 FROM attribution_decisions WHERE resource_id = 42",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(winner, 1);
        assert_eq!(version, 1);
        let (total, project): (i64, i64) = store
            .conn
            .query_row(
                "SELECT total_score, project_score FROM attribution_candidates
                 WHERE session_id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(total, 95, "worked example total must round-trip");
        assert_eq!(project, 10);

        // Raw evidence (§14) round-trips too.
        let decision = store.latest_decision(42).unwrap().unwrap();
        let cand = decision.candidates.first().unwrap();
        assert!(
            cand.evidence
                .iter()
                .any(|e| e.kind == crate::classify::ownership::resolver::EvidenceKind::CwdMatch),
            "exact-cwd evidence must be persisted and readable"
        );
        assert!(
            cand.evidence.iter().any(|e| e.kind
                == crate::classify::ownership::resolver::EvidenceKind::StartTimeCorrelation),
            "start-time evidence must be persisted and readable"
        );
    }

    #[test]
    fn end_absent_sessions_marks_only_gone_roots() {
        let mut store = RuntimeStore::open_in_memory().unwrap();
        apply_chain(&mut store, 1000);

        // Session root (omp pid 100 / start 1000) is still live.
        let boot = BootId::from_u128(7);
        let live_root = ProcessIdentity {
            boot_id: boot,
            pid: 100,
            start_time: 1000,
        };
        let live = std::collections::HashSet::from([live_root]);
        store.end_absent_sessions(&live, 2000).unwrap();
        assert!(
            store
                .owning_session_for_process(&boot, 120, 1007)
                .unwrap()
                .is_some(),
            "live root keeps its session active"
        );

        // Root gone on the next scan → session ended (but provenance kept).
        let gone = std::collections::HashSet::new();
        store.end_absent_sessions(&gone, 3000).unwrap();
        assert!(
            store
                .owning_session_for_process(&boot, 120, 1007)
                .unwrap()
                .is_some(),
            "ended session still keeps provenance"
        );
    }

    #[test]
    fn vendor_registration_maps_alias_and_ends() {
        let mut store = RuntimeStore::open_in_memory().unwrap();
        let boot = BootId::from_u128(7);
        let sid = store
            .ensure_session(&boot, "junie", 100, 500, 1000)
            .unwrap();
        store
            .register_alias(sid, "junie", "junie-48372", 1000)
            .unwrap();

        let looked = store
            .session_id_for_alias("junie", "junie-48372")
            .unwrap()
            .expect("alias resolves to the Wyd session");
        assert_eq!(looked, sid);
        assert!(
            store
                .session_record(sid)
                .unwrap()
                .unwrap()
                .ended_at
                .is_none()
        );

        // Vendor session end is metadata — it must NOT end the runtime session
        // (the agent process may still be alive).
        store
            .end_vendor_alias("junie", "junie-48372", 2000)
            .unwrap();
        assert_eq!(
            store.session_record(sid).unwrap().unwrap().ended_at,
            None,
            "vendor session_end must not end the runtime session"
        );

        // Same process invocation is idempotent: one session, one alias.
        let again = store
            .ensure_session(&boot, "junie", 100, 500, 2000)
            .unwrap();
        assert_eq!(again, sid);
    }
}
