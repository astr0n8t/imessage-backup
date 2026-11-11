//! The server's merged index of every snapshot in the archive.
//!
//! Each snapshot's `sms.db` is parsed with imessage-database. Message rows are
//! unique per `(device, guid)`: every device keeps its own copy, because the
//! sender's and the recipient's copies of an iMessage share a GUID and must
//! not be merged when both people's phones are archived. Each row also
//! records its *owner* (the iMessage ID / number it used), so accounts can be
//! granted devices and/or iMessage IDs (see [`crate::view`]); copies held by
//! several visible devices are de-duplicated at query time.
//!
//! Conversations are keyed on `chat_identifier`, which merges a contact's
//! iMessage, SMS and RCS threads the way the Messages app does. Their names
//! and participants are stored per device so accounts only see their own.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use imessage_database::rusqlite as idb_sqlite;
use imessage_database::tables::capabilities::Capabilities;
use imessage_database::tables::chat::Chat;
use imessage_database::tables::chat_handle::ChatToHandle;
use imessage_database::tables::handle::Handle;
use imessage_database::tables::messages::Message;
use imessage_database::tables::table::get_connection;
use imessage_database::util::query_context::QueryContext;
use imsg_core::archive::{self, CONTACTS_FILE, SMS_DB_FILE, SnapshotRef};
use imsg_core::contacts::handle_key;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use tracing::{info, warn};

use crate::render;

/// Bump when the schema changes; an older index is discarded and rebuilt
/// from the archive (it is only a cache).
const SCHEMA_VERSION: &str = "2";

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS snapshots (
    device TEXT NOT NULL, snapshot_id TEXT NOT NULL, device_name TEXT, backup_ms INTEGER, ingested_ms INTEGER,
    messages INTEGER, has_contacts INTEGER, PRIMARY KEY (device, snapshot_id));
CREATE TABLE IF NOT EXISTS conversations (id INTEGER PRIMARY KEY, key TEXT UNIQUE NOT NULL);
CREATE TABLE IF NOT EXISTS conv_meta (
    conv_id INTEGER NOT NULL, device TEXT NOT NULL, display_name TEXT, is_group INTEGER NOT NULL DEFAULT 0,
    services TEXT NOT NULL DEFAULT '', snap_order INTEGER NOT NULL, PRIMARY KEY (conv_id, device));
CREATE TABLE IF NOT EXISTS participants (
    conv_id INTEGER NOT NULL, device TEXT NOT NULL, handle TEXT NOT NULL, PRIMARY KEY (conv_id, device, handle));
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY, device TEXT NOT NULL, guid TEXT NOT NULL, owner TEXT NOT NULL DEFAULT '',
    conv_id INTEGER NOT NULL, sender TEXT,
    is_from_me INTEGER NOT NULL, date_ms INTEGER NOT NULL, read_ms INTEGER, delivered_ms INTEGER,
    edited_ms INTEGER, service TEXT, kind TEXT NOT NULL, text TEXT, body TEXT NOT NULL,
    assoc_guid TEXT, assoc_part INTEGER, reaction TEXT, reaction_removed INTEGER NOT NULL DEFAULT 0,
    thread_guid TEXT, expressive TEXT, snap_order INTEGER NOT NULL, UNIQUE (device, guid));
CREATE INDEX IF NOT EXISTS messages_guid ON messages (guid);
CREATE INDEX IF NOT EXISTS messages_conv_date ON messages (conv_id, date_ms, id);
CREATE INDEX IF NOT EXISTS messages_assoc ON messages (assoc_guid);
CREATE INDEX IF NOT EXISTS messages_thread ON messages (thread_guid);
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5 (text, tokenize = 'unicode61 remove_diacritics 2');
CREATE TABLE IF NOT EXISTS message_files (message_id INTEGER NOT NULL, file_id TEXT NOT NULL, PRIMARY KEY (file_id, message_id));
CREATE TABLE IF NOT EXISTS attachments (
    file_id TEXT NOT NULL, device TEXT NOT NULL, mime TEXT, name TEXT, size INTEGER,
    PRIMARY KEY (file_id, device));
-- Per (conversation, device, owner): visible-message counts and recency, for listing.
CREATE TABLE IF NOT EXISTS conv_stats (
    conv_id INTEGER NOT NULL, device TEXT NOT NULL, owner TEXT NOT NULL, msg_count INTEGER NOT NULL,
    last_ms INTEGER, PRIMARY KEY (conv_id, device, owner));
CREATE TABLE IF NOT EXISTS contacts (device TEXT NOT NULL, key TEXT NOT NULL, name TEXT NOT NULL, PRIMARY KEY (device, key));
CREATE TABLE IF NOT EXISTS contact_sources (device TEXT PRIMARY KEY, snap_order INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS sessions (token_hash TEXT PRIMARY KEY, user TEXT NOT NULL, expires_ms INTEGER NOT NULL);
"#;

pub struct Index {
    pub path: PathBuf,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct IngestReport {
    pub snapshots: usize,
    pub messages: usize,
    pub waiting: usize,
    pub failed: usize,
}

impl Index {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        if path.exists() {
            let old = Connection::open(path)?;
            let version: Option<String> = old
                .query_row(
                    "SELECT value FROM meta WHERE key = 'schema_version'",
                    [],
                    |r| r.get(0),
                )
                .optional()
                .unwrap_or(None);
            drop(old);
            if version.as_deref() != Some(SCHEMA_VERSION) {
                info!("index schema changed; rebuilding {}", path.display());
                for suffix in ["", "-wal", "-shm"] {
                    let mut p = path.as_os_str().to_owned();
                    p.push(suffix);
                    let _ = std::fs::remove_file(PathBuf::from(p));
                }
            }
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        conn.execute(
            "INSERT OR REPLACE INTO meta VALUES ('schema_version', ?1)",
            [SCHEMA_VERSION],
        )?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    pub fn write(&self) -> Result<Connection> {
        let c = Connection::open(&self.path)?;
        c.busy_timeout(std::time::Duration::from_secs(30))?;
        Ok(c)
    }

    pub fn read(&self) -> Result<Connection> {
        let c = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        c.busy_timeout(std::time::Duration::from_secs(10))?;
        Ok(c)
    }

    /// Ingest every complete, not-yet-ingested snapshot below `data`.
    pub fn ingest_all(&self, data: &Path) -> Result<IngestReport> {
        let mut report = IngestReport::default();
        let mut conn = self.write()?;
        let all = archive::discover_snapshots(data)?;
        let mut contacts_changed = false;
        for snap in &all {
            let done: bool = conn
                .query_row(
                    "SELECT 1 FROM snapshots WHERE device = ?1 AND snapshot_id = ?2",
                    [&snap.meta.device_udid, &snap.meta.snapshot_id],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if done {
                continue;
            }
            if !snap.meta.verify(&snap.dir, true)? {
                report.waiting += 1;
                continue;
            }
            match ingest_snapshot(&mut conn, snap) {
                Ok(n) => {
                    info!(snapshot = %snap.meta.snapshot_id, device = %snap.meta.device_udid, messages = n, "ingested");
                    report.snapshots += 1;
                    report.messages += n;
                    contacts_changed |= snap.meta.has_file(CONTACTS_FILE);
                }
                Err(e) => {
                    warn!(snapshot = %snap.dir.display(), "failed to ingest: {e:#}");
                    report.failed += 1;
                }
            }
        }
        if contacts_changed {
            rebuild_contacts(&mut conn, &all)?;
        }
        if report.snapshots > 0 {
            refresh_stats(&conn)?;
        }
        Ok(report)
    }
}

fn snap_order(snap: &SnapshotRef) -> i64 {
    snap.meta
        .backup_date
        .unwrap_or(snap.meta.created_at)
        .timestamp_millis()
}

fn ingest_snapshot(conn: &mut Connection, snap: &SnapshotRef) -> Result<usize> {
    let tx = conn.transaction()?;
    let device = &snap.meta.device_udid;
    let order = snap_order(snap);
    let mut count = 0usize;

    let sms = snap.dir.join(SMS_DB_FILE);
    if snap.meta.has_file(SMS_DB_FILE) {
        let db: idb_sqlite::Connection = get_connection(&sms)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("opening sms.db")?;
        let caps = Capabilities::determine(&db).map_err(|e| anyhow::anyhow!("{e}"))?;
        let handles = Handle::ids_by_rowid(Handle::load(&db).map_err(|e| anyhow::anyhow!("{e}"))?);
        let chats = Chat::by_rowid(&db).map_err(|e| anyhow::anyhow!("{e}"))?;
        let members =
            ChatToHandle::participants_by_chat(&db).map_err(|e| anyhow::anyhow!("{e}"))?;

        let mut conv_ids: HashMap<i32, i64> = HashMap::new();
        for (rowid, chat) in &chats {
            let parts: BTreeSet<String> = members
                .get(rowid)
                .map(|s| s.iter().filter_map(|h| handles.get(h).cloned()).collect())
                .unwrap_or_default();
            let is_group = parts.len() > 1 || chat.chat_identifier.starts_with("chat");
            let name = chat.display_name().map(str::to_owned);
            let service = chat.service_name.clone().unwrap_or_default();
            let id: i64 = tx.query_row(
                "INSERT INTO conversations (key) VALUES (?1)
                 ON CONFLICT(key) DO UPDATE SET key = excluded.key RETURNING id",
                params![chat.chat_identifier],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO conv_meta (conv_id, device, display_name, is_group, services, snap_order)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(conv_id, device) DO UPDATE SET
                    display_name = CASE WHEN excluded.snap_order >= conv_meta.snap_order
                        THEN coalesce(excluded.display_name, conv_meta.display_name) ELSE conv_meta.display_name END,
                    is_group = max(conv_meta.is_group, excluded.is_group),
                    services = CASE WHEN excluded.services = '' OR instr(',' || conv_meta.services || ',', ',' || excluded.services || ',') > 0
                               THEN conv_meta.services
                               WHEN conv_meta.services = '' THEN excluded.services
                               ELSE conv_meta.services || ',' || excluded.services END,
                    snap_order = max(conv_meta.snap_order, excluded.snap_order)",
                params![id, device, name, is_group, service, order],
            )?;
            for p in &parts {
                tx.execute(
                    "INSERT OR IGNORE INTO participants VALUES (?1, ?2, ?3)",
                    params![id, device, p],
                )?;
            }
            conv_ids.insert(*rowid, id);
        }

        let ctx = QueryContext::default();
        let mut stmt =
            Message::stream_rows(&db, &caps, &ctx).map_err(|e| anyhow::anyhow!("{e}"))?;
        let rows = Message::rows(&mut stmt, []).map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut upsert = tx.prepare(
            "INSERT INTO messages (guid, conv_id, sender, is_from_me, date_ms, read_ms, delivered_ms, edited_ms,
                service, kind, text, body, assoc_guid, assoc_part, reaction, reaction_removed, thread_guid,
                expressive, device, snap_order, owner)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)
             ON CONFLICT(device, guid) DO UPDATE SET owner = excluded.owner,
                conv_id = excluded.conv_id, sender = excluded.sender, is_from_me = excluded.is_from_me,
                date_ms = excluded.date_ms, read_ms = excluded.read_ms, delivered_ms = excluded.delivered_ms,
                edited_ms = excluded.edited_ms, service = excluded.service, kind = excluded.kind,
                text = excluded.text, body = excluded.body, assoc_guid = excluded.assoc_guid,
                assoc_part = excluded.assoc_part, reaction = excluded.reaction,
                reaction_removed = excluded.reaction_removed, thread_guid = excluded.thread_guid,
                expressive = excluded.expressive, snap_order = excluded.snap_order
             WHERE excluded.snap_order >= messages.snap_order
             RETURNING id",
        )?;
        let mut fts_del = tx.prepare("DELETE FROM messages_fts WHERE rowid = ?1")?;
        let mut fts_ins = tx.prepare("INSERT INTO messages_fts (rowid, text) VALUES (?1, ?2)")?;
        let mut files_del = tx.prepare("DELETE FROM message_files WHERE message_id = ?1")?;
        let mut files_ins = tx.prepare("INSERT OR IGNORE INTO message_files VALUES (?1, ?2)")?;
        let mut att = tx.prepare(
            "INSERT INTO attachments (file_id, device, mime, name, size) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(file_id, device) DO UPDATE SET mime = coalesce(excluded.mime, attachments.mime),
                name = coalesce(excluded.name, attachments.name), size = excluded.size",
        )?;
        let mut seen = BTreeSet::new();
        for msg in rows {
            let m = match msg {
                Ok(m) => m,
                Err(e) => {
                    warn!("skipping unreadable message: {e}");
                    continue;
                }
            };
            // A message in several chats appears once per chat; keep the first.
            if !seen.insert(m.guid.clone()) {
                continue;
            }
            let Some(conv) = m.chat_id.or(m.deleted_from).and_then(|c| conv_ids.get(&c)) else {
                continue;
            };
            let r = render::render(&db, &caps, &m, &handles);
            let sender = if m.is_from_me() {
                None
            } else {
                m.handle_id.and_then(|h| handles.get(&h).cloned())
            };
            let owner = m
                .destination_caller_id
                .as_deref()
                .and_then(handle_key)
                .unwrap_or_default();
            let id: Option<i64> = upsert
                .query_row(
                    params![
                        m.guid,
                        conv,
                        sender,
                        m.is_from_me(),
                        r.date_ms,
                        r.read_ms,
                        r.delivered_ms,
                        r.edited_ms,
                        m.service,
                        r.kind,
                        r.text,
                        serde_json::to_string(&r.body)?,
                        r.assoc_guid,
                        r.assoc_part,
                        r.reaction,
                        r.reaction_removed,
                        m.thread_originator_guid,
                        r.expressive,
                        device,
                        order,
                        owner,
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(id) = id {
                fts_del.execute([id])?;
                if r.kind == "message" && !r.text.is_empty() {
                    fts_ins.execute(params![id, r.text])?;
                }
                files_del.execute([id])?;
                for (file_id, ..) in &r.files {
                    files_ins.execute(params![id, file_id])?;
                }
                count += 1;
            }
            for (file_id, mime, name, size) in &r.files {
                att.execute(params![file_id, device, mime, name, size])?;
            }
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO snapshots VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            device,
            snap.meta.snapshot_id,
            snap.meta.device_name,
            order,
            chrono::Utc::now().timestamp_millis(),
            count as i64,
            snap.meta.has_file(CONTACTS_FILE)
        ],
    )?;
    tx.commit()?;
    Ok(count)
}

/// Rebuild each device's contact names from its newest Contacts DB.
fn rebuild_contacts(conn: &mut Connection, all: &[SnapshotRef]) -> Result<()> {
    let mut newest: HashMap<&str, &SnapshotRef> = HashMap::new();
    for s in all.iter().filter(|s| s.meta.has_file(CONTACTS_FILE)) {
        if s.meta.verify(&s.dir, false).unwrap_or(false) {
            newest.insert(&s.meta.device_udid, s); // `all` is sorted oldest-first
        }
    }
    let tx = conn.transaction()?;
    for (device, s) in newest {
        match imsg_core::contacts::load_contacts(&s.dir.join(CONTACTS_FILE)) {
            Ok(contacts) => {
                tx.execute("DELETE FROM contacts WHERE device = ?1", [device])?;
                for (key, name) in imsg_core::contacts::name_index(&contacts) {
                    tx.execute(
                        "INSERT OR REPLACE INTO contacts VALUES (?1, ?2, ?3)",
                        params![device, key, name],
                    )?;
                }
                tx.execute(
                    "INSERT OR REPLACE INTO contact_sources VALUES (?1, ?2)",
                    params![device, snap_order(s)],
                )?;
            }
            Err(e) => warn!("could not read contacts from {}: {e:#}", s.dir.display()),
        }
    }
    tx.commit()?;
    Ok(())
}

fn refresh_stats(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "BEGIN;
         DELETE FROM conv_stats;
         INSERT INTO conv_stats
            SELECT conv_id, device, owner, sum(kind = 'message'), max(date_ms)
            FROM messages WHERE kind IN ('message', 'announcement') GROUP BY conv_id, device, owner;
         COMMIT;",
    )?;
    Ok(())
}
