//! The server's merged index of every snapshot in the archive.
//!
//! Each snapshot's `sms.db` is parsed with imessage-database and merged into a
//! single SQLite index keyed on message GUIDs, so overlapping or repeated
//! snapshots (full re-extractions, multiple phones synced through iCloud) are
//! de-duplicated. Conversations are keyed on `chat_identifier`, which merges
//! a contact's iMessage, SMS and RCS threads the way the Messages app does.

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
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use tracing::{info, warn};

use crate::render;

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS snapshots (
    device TEXT NOT NULL, snapshot_id TEXT NOT NULL, backup_ms INTEGER, ingested_ms INTEGER,
    messages INTEGER, has_contacts INTEGER, PRIMARY KEY (device, snapshot_id));
CREATE TABLE IF NOT EXISTS conversations (
    id INTEGER PRIMARY KEY, key TEXT UNIQUE NOT NULL, display_name TEXT, is_group INTEGER NOT NULL DEFAULT 0,
    services TEXT, last_ms INTEGER, msg_count INTEGER NOT NULL DEFAULT 0, last_preview TEXT,
    last_from_me INTEGER, last_sender TEXT);
CREATE TABLE IF NOT EXISTS participants (conv_id INTEGER NOT NULL, handle TEXT NOT NULL, PRIMARY KEY (conv_id, handle));
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY, guid TEXT UNIQUE NOT NULL, conv_id INTEGER NOT NULL, sender TEXT,
    is_from_me INTEGER NOT NULL, date_ms INTEGER NOT NULL, read_ms INTEGER, delivered_ms INTEGER,
    edited_ms INTEGER, service TEXT, kind TEXT NOT NULL, text TEXT, body TEXT NOT NULL,
    assoc_guid TEXT, assoc_part INTEGER, reaction TEXT, reaction_removed INTEGER NOT NULL DEFAULT 0,
    thread_guid TEXT, expressive TEXT, device TEXT, snap_order INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS messages_conv_date ON messages (conv_id, date_ms, id);
CREATE INDEX IF NOT EXISTS messages_assoc ON messages (assoc_guid);
CREATE INDEX IF NOT EXISTS messages_thread ON messages (thread_guid);
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5 (text, tokenize = 'unicode61 remove_diacritics 2');
CREATE TABLE IF NOT EXISTS attachments (
    file_id TEXT NOT NULL, device TEXT NOT NULL, mime TEXT, name TEXT, size INTEGER,
    PRIMARY KEY (file_id, device));
CREATE TABLE IF NOT EXISTS contacts (key TEXT PRIMARY KEY, name TEXT NOT NULL);
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
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
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
            refresh_conversations(&conn)?;
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
                "INSERT INTO conversations (key, display_name, is_group, services) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(key) DO UPDATE SET
                    display_name = coalesce(excluded.display_name, conversations.display_name),
                    is_group = max(conversations.is_group, excluded.is_group),
                    services = CASE WHEN instr(',' || conversations.services || ',', ',' || excluded.services || ',') > 0
                               THEN conversations.services ELSE conversations.services || ',' || excluded.services END
                 RETURNING id",
                params![chat.chat_identifier, name, is_group, service],
                |r| r.get(0),
            )?;
            for p in &parts {
                tx.execute(
                    "INSERT OR IGNORE INTO participants VALUES (?1, ?2)",
                    params![id, p],
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
                expressive, device, snap_order)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
             ON CONFLICT(guid) DO UPDATE SET
                conv_id = excluded.conv_id, sender = excluded.sender, is_from_me = excluded.is_from_me,
                date_ms = excluded.date_ms, read_ms = excluded.read_ms, delivered_ms = excluded.delivered_ms,
                edited_ms = excluded.edited_ms, service = excluded.service, kind = excluded.kind,
                text = excluded.text, body = excluded.body, assoc_guid = excluded.assoc_guid,
                assoc_part = excluded.assoc_part, reaction = excluded.reaction,
                reaction_removed = excluded.reaction_removed, thread_guid = excluded.thread_guid,
                expressive = excluded.expressive, device = excluded.device, snap_order = excluded.snap_order
             WHERE excluded.snap_order >= messages.snap_order
             RETURNING id",
        )?;
        let mut fts_del = tx.prepare("DELETE FROM messages_fts WHERE rowid = ?1")?;
        let mut fts_ins = tx.prepare("INSERT INTO messages_fts (rowid, text) VALUES (?1, ?2)")?;
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
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(id) = id {
                fts_del.execute([id])?;
                if r.kind == "message" && !r.text.is_empty() {
                    fts_ins.execute(params![id, r.text])?;
                }
                count += 1;
            }
            for (file_id, mime, name, size) in &r.files {
                att.execute(params![file_id, device, mime, name, size])?;
            }
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO snapshots VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            device,
            snap.meta.snapshot_id,
            order,
            chrono::Utc::now().timestamp_millis(),
            count as i64,
            snap.meta.has_file(CONTACTS_FILE)
        ],
    )?;
    tx.commit()?;
    Ok(count)
}

/// Rebuild the contact name table from the newest Contacts DB of each device
/// (newer devices win when they disagree).
fn rebuild_contacts(conn: &mut Connection, all: &[SnapshotRef]) -> Result<()> {
    let mut newest: HashMap<&str, &SnapshotRef> = HashMap::new();
    for s in all.iter().filter(|s| s.meta.has_file(CONTACTS_FILE)) {
        if s.meta.verify(&s.dir, false).unwrap_or(false) {
            newest.insert(&s.meta.device_udid, s); // `all` is sorted oldest-first
        }
    }
    let mut sources: Vec<&SnapshotRef> = newest.into_values().collect();
    sources.sort_by_key(|s| snap_order(s));
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM contacts", [])?;
    for s in sources {
        match imsg_core::contacts::load_contacts(&s.dir.join(CONTACTS_FILE)) {
            Ok(contacts) => {
                for (key, name) in imsg_core::contacts::name_index(&contacts) {
                    tx.execute(
                        "INSERT OR REPLACE INTO contacts VALUES (?1, ?2)",
                        params![key, name],
                    )?;
                }
            }
            Err(e) => warn!("could not read contacts from {}: {e:#}", s.dir.display()),
        }
    }
    tx.commit()?;
    Ok(())
}

fn refresh_conversations(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "UPDATE conversations SET
            msg_count = (SELECT count(*) FROM messages m WHERE m.conv_id = conversations.id AND m.kind = 'message'),
            last_ms = (SELECT max(date_ms) FROM messages m WHERE m.conv_id = conversations.id AND m.kind IN ('message', 'announcement'));
         UPDATE conversations SET
            last_preview = (SELECT CASE WHEN m.text <> '' THEN m.text
                                        WHEN m.kind = 'announcement' THEN '(group update)'
                                        WHEN m.body LIKE '%\"t\":\"attachment\"%' THEN '📎 Attachment'
                                        ELSE '' END
                            FROM messages m WHERE m.conv_id = conversations.id AND m.kind IN ('message', 'announcement')
                            ORDER BY m.date_ms DESC, m.id DESC LIMIT 1),
            last_from_me = (SELECT m.is_from_me FROM messages m WHERE m.conv_id = conversations.id
                            AND m.kind = 'message' ORDER BY m.date_ms DESC, m.id DESC LIMIT 1),
            last_sender = (SELECT m.sender FROM messages m WHERE m.conv_id = conversations.id
                            AND m.kind = 'message' ORDER BY m.date_ms DESC, m.id DESC LIMIT 1);",
    )?;
    Ok(())
}
