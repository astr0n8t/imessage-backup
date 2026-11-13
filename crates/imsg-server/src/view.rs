//! What an account is allowed to see.
//!
//! Every indexed message row comes from one device's backup and carries the
//! iMessage ID / phone number it was sent or received on (its *owner*, from
//! `destination_caller_id`). An account sees a row when the row's device is
//! one of its devices, or the row's owner is one of its iMessage IDs.

use rusqlite::{Connection, params};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct View {
    /// Every device (`devices = ["*"]`).
    pub all: bool,
    /// Device UDIDs (upper-case).
    pub devices: Vec<String>,
    /// iMessage IDs, normalized with [`imsg_core::contacts::handle_key`].
    pub owners: Vec<String>,
}

impl View {
    pub fn everything() -> Self {
        Self {
            all: true,
            ..Default::default()
        }
    }

    pub fn new(devices: &[String], imessage_ids: &[String]) -> Self {
        Self {
            all: devices.iter().any(|d| d.trim() == "*"),
            devices: devices
                .iter()
                .map(|d| d.trim().to_uppercase())
                .filter(|d| d != "*")
                .collect(),
            owners: imessage_ids
                .iter()
                .filter_map(|h| imsg_core::contacts::handle_key(h))
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        !self.all && self.devices.is_empty() && self.owners.is_empty()
    }

    /// Load the view into per-connection temp tables used by [`View::filter`].
    pub fn install(&self, conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS v_dev (d TEXT PRIMARY KEY);
             CREATE TEMP TABLE IF NOT EXISTS v_own (o TEXT PRIMARY KEY);
             DELETE FROM temp.v_dev; DELETE FROM temp.v_own;",
        )?;
        for d in &self.devices {
            conn.execute("INSERT OR IGNORE INTO temp.v_dev VALUES (?1)", params![d])?;
        }
        for o in &self.owners {
            conn.execute("INSERT OR IGNORE INTO temp.v_own VALUES (?1)", params![o])?;
        }
        Ok(())
    }

    /// SQL condition selecting rows (of a table with `device` and `owner`
    /// columns, aliased `alias`) that this view may see.
    pub fn filter(&self, alias: &str) -> String {
        if self.all {
            "1".into()
        } else {
            format!(
                "({alias}.device IN (SELECT d FROM temp.v_dev) OR {alias}.owner IN (SELECT o FROM temp.v_own))"
            )
        }
    }

    /// SQL condition keeping one copy of a message that several visible
    /// devices hold (e.g. an old and a new phone): the newest snapshot's.
    /// Copies are only duplicates within the same conversation and direction;
    /// the sender's and recipient's copies of an iMessage share a GUID too.
    pub fn dedupe(&self, alias: &str) -> String {
        if !self.all && self.owners.is_empty() && self.devices.len() <= 1 {
            return "1".into();
        }
        // `INDEXED BY` pins the lookup to the (guid, conv_id, is_from_me,
        // snap_order, id) index: one seek per row, already in the wanted order.
        // Left to itself SQLite may pick the conversation index instead and
        // scan the whole conversation for every message (quadratic).
        format!(
            "{alias}.id = (SELECT d2.id FROM messages d2 INDEXED BY messages_copies
               WHERE d2.guid = {alias}.guid AND d2.conv_id = {alias}.conv_id
               AND d2.is_from_me = {alias}.is_from_me AND {} ORDER BY d2.snap_order DESC, d2.id DESC LIMIT 1)",
            self.filter("d2")
        )
    }

    /// SQL expression counting distinct messages among rows of `alias`
    /// (copies held by several devices count once). Much cheaper than
    /// filtering with [`View::dedupe`] when only a count is needed.
    pub fn distinct_count(&self, alias: &str) -> String {
        if !self.all && self.owners.is_empty() && self.devices.len() <= 1 {
            return "count(*)".into();
        }
        format!(
            "count(DISTINCT {alias}.guid || '/' || {alias}.conv_id || '/' || {alias}.is_from_me)"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds() {
        let v = View::new(
            &["abc-1".into()],
            &["+1 (555) 010-9999".into(), "Me@iCloud.com".into()],
        );
        assert!(!v.all);
        assert_eq!(v.devices, ["ABC-1"]);
        assert_eq!(v.owners, ["5550109999", "me@icloud.com"]);
        assert!(View::new(&["*".into()], &[]).all);
        assert!(View::new(&[], &[]).is_empty());
    }
}
