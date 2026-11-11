//! On-disk layout of the archive folder shared by the extractor and the server.
//!
//! ```text
//! <archive>/
//!   <DEVICE-UDID>/
//!     device.json                     device metadata (latest)
//!     attachments/<xx>/<fileID>       attachment bodies, deduplicated by backup fileID
//!     snapshots/<SNAPSHOT-ID>/
//!       snapshot.json                 written last; lists files + checksums
//!       sms.db                        delta Messages DB (same schema as iOS sms.db)
//!       AddressBook.sqlitedb          Contacts DB (only when it changed)
//! ```
//!
//! Every snapshot's `sms.db` keeps the original iOS schema, containing only the
//! messages that are new (or edited) since the previous snapshot plus all chat
//! and handle rows, so any tool that reads `sms.db` (imessage-database,
//! sqlite3, ...) can open it. Attachments keep the backup's hashed naming
//! (`SHA-1("MediaDomain-" + path)`), the same scheme imessage-database uses to
//! resolve attachments of iOS backups.
//!
//! The folders are designed to be merged with a plain copy (`rsync`, Finder,
//! etc.): nothing is ever rewritten, only added.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const FORMAT_VERSION: u32 = 1;
pub const SNAPSHOT_META: &str = "snapshot.json";
pub const DEVICE_META: &str = "device.json";
pub const SMS_DB_FILE: &str = "sms.db";
pub const CONTACTS_FILE: &str = "AddressBook.sqlitedb";
pub const SNAPSHOTS_DIR: &str = "snapshots";
pub const ATTACHMENTS_DIR: &str = "attachments";

pub fn device_dir(archive: &Path, udid: &str) -> PathBuf {
    archive.join(udid)
}

pub fn snapshots_dir(device_dir: &Path) -> PathBuf {
    device_dir.join(SNAPSHOTS_DIR)
}

pub fn attachments_dir(device_dir: &Path) -> PathBuf {
    device_dir.join(ATTACHMENTS_DIR)
}

pub fn attachment_path(device_dir: &Path, file_id: &str) -> PathBuf {
    crate::backup::hashed_path(&attachments_dir(device_dir), file_id)
}

/// Snapshot IDs sort chronologically: `20261111T163800Z`.
pub fn snapshot_id(at: DateTime<Utc>) -> String {
    at.format("%Y%m%dT%H%M%SZ").to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceMeta {
    pub format_version: u32,
    pub udid: String,
    pub device_name: Option<String>,
    pub product_type: Option<String>,
    pub product_version: Option<String>,
    pub phone_number: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotKind {
    /// Every message in the backup.
    Full,
    /// Only messages added or edited since the previous snapshot.
    Incremental,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileRecord {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnapshotMeta {
    pub format_version: u32,
    pub snapshot_id: String,
    pub device_udid: String,
    pub device_name: Option<String>,
    pub ios_version: Option<String>,
    pub backup_date: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub kind: SnapshotKind,
    /// Messages with `ROWID` greater than this were included (plus edits).
    pub after_rowid: i64,
    /// Highest `message.ROWID` present in the backup.
    pub max_rowid: i64,
    pub message_count: u64,
    pub attachment_count: u64,
    pub attachments_missing: u64,
    pub files: Vec<FileRecord>,
    pub extractor_version: String,
}

impl SnapshotMeta {
    pub fn has_file(&self, name: &str) -> bool {
        self.files.iter().any(|f| f.name == name)
    }

    pub fn read(dir: &Path) -> Result<Self> {
        let path = dir.join(SNAPSHOT_META);
        let data = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_slice(&data).with_context(|| format!("parsing {}", path.display()))
    }

    /// Check that every listed file is present with the recorded size (and
    /// optionally checksum). Snapshots copied over the network may arrive
    /// partially; the server waits until they verify.
    pub fn verify(&self, dir: &Path, check_hash: bool) -> Result<bool> {
        for f in &self.files {
            let p = dir.join(&f.name);
            match fs::metadata(&p) {
                Ok(m) if m.len() == f.size => {}
                _ => return Ok(false),
            }
            if check_hash && sha256_file(&p)? != f.sha256 {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn file_record(dir: &Path, name: &str) -> Result<FileRecord> {
    let p = dir.join(name);
    Ok(FileRecord {
        name: name.to_owned(),
        size: fs::metadata(&p)?.len(),
        sha256: sha256_file(&p)?,
    })
}

/// Write JSON atomically (temp file + rename).
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(value)?)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// A discovered snapshot directory.
#[derive(Debug, Clone)]
pub struct SnapshotRef {
    pub device_dir: PathBuf,
    pub dir: PathBuf,
    pub meta: SnapshotMeta,
}

/// Find all snapshots in an archive (sorted by device, then snapshot ID).
/// Directories without a readable `snapshot.json` are skipped.
pub fn discover_snapshots(archive: &Path) -> Result<Vec<SnapshotRef>> {
    let mut out = Vec::new();
    for dev in fs::read_dir(archive).with_context(|| format!("reading {}", archive.display()))? {
        let dev = dev?.path();
        let snaps = snapshots_dir(&dev);
        if !snaps.is_dir() {
            continue;
        }
        for snap in fs::read_dir(&snaps)? {
            let dir = snap?.path();
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.starts_with('.') || !dir.is_dir() {
                continue;
            }
            if let Ok(meta) = SnapshotMeta::read(&dir) {
                out.push(SnapshotRef {
                    device_dir: dev.clone(),
                    dir,
                    meta,
                });
            }
        }
    }
    out.sort_by(|a, b| {
        (a.meta.backup_date, &a.meta.snapshot_id, &a.meta.device_udid).cmp(&(
            b.meta.backup_date,
            &b.meta.snapshot_id,
            &b.meta.device_udid,
        ))
    });
    Ok(out)
}
