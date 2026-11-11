//! The extractor's incremental-progress file (`state.json`).
//!
//! It records, per device, how far the previous extraction got so the next
//! run only exports what is new. It lives next to the archive by default, but
//! can be kept elsewhere (e.g. on the Mac) when the output is a staging folder
//! that is later copied to the server.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::archive::{SnapshotKind, write_json_atomic};

pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct State {
    pub version: u32,
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceState>,
}

/// A `(ROWID, guid)` pair used to confirm that the database we see now is the
/// same one we extracted from last time (a phone restore or migration
/// renumbers messages; then we fall back to a full extraction).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Anchor {
    pub rowid: i64,
    pub guid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct DeviceState {
    pub device_name: Option<String>,
    /// `Last Backup Date` of the backup we last extracted.
    pub last_backup_date: Option<DateTime<Utc>>,
    pub last_run: Option<DateTime<Utc>>,
    /// Highest `message.ROWID` already exported.
    pub last_rowid: i64,
    /// Highest `date_edited`/`date_retracted` already exported (Apple epoch ns).
    #[serde(default)]
    pub last_edit_date: i64,
    #[serde(default)]
    pub anchors: Vec<Anchor>,
    /// SHA-256 of the last exported Contacts DB (raw backup bytes).
    pub contacts_sha256: Option<String>,
    /// Attachments referenced by exported messages but absent from the
    /// backup (e.g. offloaded to iCloud). Retried on every run.
    #[serde(default)]
    pub pending_attachments: BTreeSet<String>,
    #[serde(default)]
    pub snapshots: Vec<SnapshotSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnapshotSummary {
    pub snapshot_id: String,
    pub kind: SnapshotKind,
    pub backup_date: Option<DateTime<Utc>>,
    pub messages: u64,
    pub attachments: u64,
}

impl State {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(State {
                version: STATE_VERSION,
                ..Default::default()
            });
        }
        let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let state: State =
            serde_json::from_slice(&data).with_context(|| format!("parsing {}", path.display()))?;
        if state.version > STATE_VERSION {
            bail!(
                "{} was written by a newer version of imsg-extract",
                path.display()
            );
        }
        Ok(state)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_json_atomic(path, self)
    }
}
