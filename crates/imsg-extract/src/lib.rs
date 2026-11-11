//! Incremental extraction of `sms.db` + `AddressBook.sqlitedb` (and message
//! attachments) from an iOS backup into an archive folder.
//!
//! See [`imsg_core::archive`] for the output layout and
//! [`imsg_core::state`] for the progress file.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use imsg_core::archive::{
    self, CONTACTS_FILE, DeviceMeta, FORMAT_VERSION, SMS_DB_FILE, SNAPSHOT_META, SnapshotKind, SnapshotMeta,
};
use imsg_core::backup::{ADDRESS_BOOK_DB, Backup, SMS_DB, attachment_file_id};
use imsg_core::state::{Anchor, DeviceState, SnapshotSummary, State};
use rusqlite::{Connection, OptionalExtension, params};

pub struct Options {
    /// Backup folder (`.../MobileSync/Backup/<UDID>`).
    pub backup: PathBuf,
    /// Archive / output folder.
    pub out: PathBuf,
    /// State file; defaults to `<out>/imsg-state.json`.
    pub state: Option<PathBuf>,
    pub password: Option<String>,
    /// Ignore the state file and export every message.
    pub full: bool,
    /// Run even when the backup date has not changed since the last run.
    pub force: bool,
    pub attachments: bool,
    pub now: DateTime<Utc>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub snapshot: Option<SnapshotMeta>,
    pub snapshot_dir: Option<PathBuf>,
    pub skipped: Option<String>,
    pub attachments_copied: u64,
    pub attachment_bytes: u64,
    pub attachments_pending: u64,
    pub contacts_updated: bool,
    pub fell_back_to_full: bool,
}

pub fn default_state_path(out: &Path) -> PathBuf {
    out.join("imsg-state.json")
}

fn has_column(db: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = db.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for n in names {
        if n?.eq_ignore_ascii_case(column) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Do any of the stored `(ROWID, guid)` anchors still match?
fn anchors_match(db: &Connection, anchors: &[Anchor]) -> Result<bool> {
    for a in anchors {
        let guid: Option<String> = db
            .query_row("SELECT guid FROM message WHERE ROWID = ?1", [a.rowid], |r| r.get(0))
            .optional()?;
        if guid.as_deref() == Some(a.guid.as_str()) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Pick up to 8 anchors spread over the message table.
fn pick_anchors(db: &Connection, max_rowid: i64) -> Result<Vec<Anchor>> {
    let mut out = BTreeSet::new();
    for i in 0..8i64 {
        let target = max_rowid - (max_rowid * i) / 8;
        if let Some((rowid, guid)) = db
            .query_row(
                "SELECT ROWID, guid FROM message WHERE ROWID <= ?1 ORDER BY ROWID DESC LIMIT 1",
                [target],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
        {
            out.insert(Anchor { rowid, guid });
        }
    }
    Ok(out.into_iter().rev().collect())
}

/// Fold any WAL into the main file so the copy is self-contained.
fn consolidate(db: &Connection) -> Result<()> {
    db.query_row("PRAGMA journal_mode=DELETE", [], |_| Ok(()))?;
    Ok(())
}

fn sha256_with_wal(path: &Path) -> Result<String> {
    let mut parts = vec![archive::sha256_file(path)?];
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let wal = PathBuf::from(wal);
    if wal.exists() {
        parts.push(archive::sha256_file(&wal)?);
    }
    Ok(parts.join(":"))
}

/// Trim the working copy of `sms.db` down to the messages in `keep` and the
/// rows that depend on them. Chats, handles and their join table are kept in
/// full (they are small and give every snapshot a complete picture).
fn trim_messages(db: &Connection) -> Result<()> {
    // iOS triggers call functions that only exist on the device.
    let triggers: Vec<String> = db
        .prepare("SELECT name FROM sqlite_master WHERE type = 'trigger'")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for t in triggers {
        db.execute_batch(&format!("DROP TRIGGER IF EXISTS \"{}\";", t.replace('"', "\"\"")))?;
    }
    db.execute_batch("PRAGMA foreign_keys = OFF;")?;
    db.execute_batch("DELETE FROM message WHERE ROWID NOT IN (SELECT id FROM temp.keep);")?;

    // Every other table keyed to messages (chat_message_join, message_attachment_join,
    // chat_recoverable_message_join, message_processing_task, ...).
    let tables: Vec<String> = db
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for t in &tables {
        for col in ["message_id", "message_row_id"] {
            if t != "message" && has_column(db, t, col)? {
                db.execute_batch(&format!(
                    "DELETE FROM \"{t}\" WHERE {col} NOT IN (SELECT id FROM temp.keep);"
                ))?;
            }
        }
    }
    db.execute_batch(
        "DELETE FROM attachment WHERE ROWID NOT IN (SELECT attachment_id FROM message_attachment_join);",
    )?;
    Ok(())
}

fn vacuum_into(db: &Connection, dest: &Path) -> Result<()> {
    let _ = fs::remove_file(dest);
    db.execute("VACUUM INTO ?1", [dest.to_string_lossy()])
        .with_context(|| format!("writing {}", dest.display()))?;
    Ok(())
}

pub fn run(opts: &Options, log: &mut dyn FnMut(String)) -> Result<Report> {
    let mut report = Report::default();
    let backup = Backup::open(&opts.backup, opts.password.as_deref())?;
    let info = backup.info.clone();
    log(format!(
        "Backup of {} ({}, iOS {}) from {}{}",
        info.device_name.as_deref().unwrap_or("unknown device"),
        info.udid,
        info.product_version.as_deref().unwrap_or("?"),
        info.last_backup_date.map(|d| d.to_rfc3339()).unwrap_or_else(|| "unknown date".into()),
        if backup.is_encrypted() { ", encrypted" } else { "" },
    ));

    let state_path = opts.state.clone().unwrap_or_else(|| default_state_path(&opts.out));
    let mut state = State::load(&state_path)?;
    let mut dev: DeviceState = state.devices.get(&info.udid).cloned().unwrap_or_default();

    if !opts.force
        && !opts.full
        && dev.last_backup_date.is_some()
        && dev.last_backup_date == info.last_backup_date
        && dev.pending_attachments.is_empty()
    {
        report.skipped = Some("backup has not changed since the last extraction (use --force)".into());
        return Ok(report);
    }

    let device_dir = archive::device_dir(&opts.out, &info.udid);
    let snapshots = archive::snapshots_dir(&device_dir);
    fs::create_dir_all(&snapshots)?;
    let work = tempfile::tempdir()?;

    // ---- Messages -------------------------------------------------------
    let sms_dir = work.path().join("sms");
    fs::create_dir_all(&sms_dir)?;
    let sms = backup
        .copy_sqlite(SMS_DB.0, SMS_DB.1, &sms_dir)?
        .ok_or_else(|| anyhow!("backup does not contain Library/SMS/sms.db"))?;
    let db = Connection::open(&sms)?;
    consolidate(&db)?;

    let max_rowid: i64 = db.query_row("SELECT coalesce(max(ROWID), 0) FROM message", [], |r| r.get(0))?;
    let had_history = dev.last_rowid > 0;
    let incremental = !opts.full && had_history && anchors_match(&db, &dev.anchors)?;
    if had_history && !incremental && !opts.full {
        report.fell_back_to_full = true;
        log("Messages database no longer matches the previous extraction (restore or new phone?); \
             doing a full extraction. The server de-duplicates messages by GUID."
            .into());
    }
    let after_rowid = if incremental { dev.last_rowid } else { 0 };

    let edit_cols: Vec<&str> = ["date_edited", "date_retracted"]
        .into_iter()
        .filter(|c| has_column(&db, "message", c).unwrap_or(false))
        .collect();
    let edit_expr = match edit_cols.as_slice() {
        [] => "0".to_owned(),
        [one] => format!("coalesce({one}, 0)"),
        many => format!(
            "max({})",
            many.iter().map(|c| format!("coalesce({c}, 0)")).collect::<Vec<_>>().join(", ")
        ),
    };
    let max_edit: i64 =
        db.query_row(&format!("SELECT coalesce(max({edit_expr}), 0) FROM message"), [], |r| r.get(0))?;

    db.execute_batch("CREATE TEMP TABLE keep (id INTEGER PRIMARY KEY);")?;
    db.execute(
        &format!(
            "INSERT INTO temp.keep SELECT ROWID FROM message WHERE ROWID > ?1 OR (?2 AND {edit_expr} > ?3)"
        ),
        params![after_rowid, incremental, dev.last_edit_date],
    )?;
    let message_count: i64 = db.query_row("SELECT count(*) FROM temp.keep", [], |r| r.get(0))?;
    let anchors = pick_anchors(&db, max_rowid)?;
    log(format!(
        "{} messages to export ({}; highest ROWID {max_rowid})",
        message_count,
        if incremental { format!("incremental after ROWID {after_rowid}") } else { "full".into() }
    ));

    let snapshot_id = {
        let base = archive::snapshot_id(opts.now);
        let mut id = base.clone();
        let mut n = 1;
        while snapshots.join(&id).exists() {
            n += 1;
            id = format!("{base}-{n}");
        }
        id
    };
    let staging = snapshots.join(format!(".tmp-{snapshot_id}"));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;

    let mut file_ids: BTreeSet<String> = BTreeSet::new();
    if message_count > 0 {
        trim_messages(&db)?;
        let mut stmt = db.prepare("SELECT filename FROM attachment WHERE filename IS NOT NULL")?;
        for f in stmt.query_map([], |r| r.get::<_, String>(0))? {
            if let Some(id) = attachment_file_id(&f?) {
                file_ids.insert(id);
            }
        }
        vacuum_into(&db, &staging.join(SMS_DB_FILE))?;
    }
    drop(db);

    // ---- Attachments ----------------------------------------------------
    let mut pending = BTreeSet::new();
    let mut attachment_count = 0u64;
    if opts.attachments {
        let wanted: BTreeSet<String> = file_ids.iter().chain(dev.pending_attachments.iter()).cloned().collect();
        for id in &wanted {
            let dest = archive::attachment_path(&device_dir, id);
            if dest.exists() {
                continue;
            }
            match backup.find_by_id(id)? {
                Some(entry) if entry.flags == 1 && backup.stored_path(&entry).exists() => {
                    report.attachment_bytes += backup
                        .copy_to(&entry, &dest)
                        .with_context(|| format!("copying attachment {}", entry.relative_path))?;
                    attachment_count += 1;
                }
                _ => {
                    pending.insert(id.clone());
                }
            }
        }
        report.attachments_copied = attachment_count;
        report.attachments_pending = pending.len() as u64;
        if !pending.is_empty() {
            log(format!(
                "{} attachments are not in this backup (possibly offloaded to iCloud); will retry next run",
                pending.len()
            ));
        }
    }

    // ---- Contacts -------------------------------------------------------
    let ab_dir = work.path().join("ab");
    fs::create_dir_all(&ab_dir)?;
    if let Some(ab) = backup.copy_sqlite(ADDRESS_BOOK_DB.0, ADDRESS_BOOK_DB.1, &ab_dir)? {
        let hash = sha256_with_wal(&ab)?;
        if !incremental || dev.contacts_sha256.as_deref() != Some(hash.as_str()) {
            let conn = Connection::open(&ab)?;
            consolidate(&conn)?;
            vacuum_into(&conn, &staging.join(CONTACTS_FILE))?;
            dev.contacts_sha256 = Some(hash);
            report.contacts_updated = true;
        }
    }

    // ---- Finalize -------------------------------------------------------
    let mut files = Vec::new();
    for name in [SMS_DB_FILE, CONTACTS_FILE] {
        if staging.join(name).exists() {
            files.push(archive::file_record(&staging, name)?);
        }
    }
    let device_meta = DeviceMeta {
        format_version: FORMAT_VERSION,
        udid: info.udid.clone(),
        device_name: info.device_name.clone(),
        product_type: info.product_type.clone(),
        product_version: info.product_version.clone(),
        phone_number: info.phone_number.clone(),
        updated_at: opts.now,
    };
    archive::write_json_atomic(&device_dir.join(archive::DEVICE_META), &device_meta)?;

    if files.is_empty() {
        fs::remove_dir_all(&staging)?;
        if attachment_count == 0 {
            report.skipped = Some("no new messages or contact changes".into());
        }
    } else {
        let meta = SnapshotMeta {
            format_version: FORMAT_VERSION,
            snapshot_id: snapshot_id.clone(),
            device_udid: info.udid.clone(),
            device_name: info.device_name.clone(),
            ios_version: info.product_version.clone(),
            backup_date: info.last_backup_date,
            created_at: opts.now,
            kind: if incremental { SnapshotKind::Incremental } else { SnapshotKind::Full },
            after_rowid,
            max_rowid,
            message_count: message_count as u64,
            attachment_count: file_ids.len() as u64,
            attachments_missing: pending.iter().filter(|p| file_ids.contains(*p)).count() as u64,
            files,
            extractor_version: env!("CARGO_PKG_VERSION").into(),
        };
        archive::write_json_atomic(&staging.join(SNAPSHOT_META), &meta)?;
        let final_dir = snapshots.join(&snapshot_id);
        fs::rename(&staging, &final_dir)?;
        dev.snapshots.push(SnapshotSummary {
            snapshot_id,
            kind: meta.kind,
            backup_date: meta.backup_date,
            messages: meta.message_count,
            attachments: meta.attachment_count,
        });
        report.snapshot = Some(meta);
        report.snapshot_dir = Some(final_dir);
    }

    dev.device_name = info.device_name.clone();
    dev.last_backup_date = info.last_backup_date;
    dev.last_run = Some(opts.now);
    dev.last_rowid = max_rowid.max(if incremental { dev.last_rowid } else { 0 });
    dev.last_edit_date = max_edit;
    dev.anchors = anchors;
    if opts.attachments {
        dev.pending_attachments = pending;
    }
    state.version = imsg_core::state::STATE_VERSION;
    state.devices.insert(info.udid.clone(), dev);
    state.save(&state_path)?;
    Ok(report)
}
