//! A synthetic iOS `sms.db` for tests. The schema mirrors the columns iOS 17/18
//! use (a subset of the ~90 real ones), including the delete triggers that
//! call iOS-only SQL functions, which the extractor must cope with.

use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, params};

pub const SCHEMA: &str = r#"
CREATE TABLE _SqliteDatabaseProperties (key TEXT, value TEXT, UNIQUE(key));
CREATE TABLE handle (ROWID INTEGER PRIMARY KEY AUTOINCREMENT UNIQUE, id TEXT NOT NULL, country TEXT,
    service TEXT NOT NULL, uncanonicalized_id TEXT, person_centric_id TEXT, UNIQUE (id, service));
CREATE TABLE chat (ROWID INTEGER PRIMARY KEY AUTOINCREMENT, guid TEXT UNIQUE NOT NULL, style INTEGER,
    state INTEGER, account_id TEXT, properties BLOB, chat_identifier TEXT, service_name TEXT,
    room_name TEXT, account_login TEXT, is_archived INTEGER DEFAULT 0, last_addressed_handle TEXT,
    display_name TEXT, group_id TEXT, is_filtered INTEGER DEFAULT 0, successful_query INTEGER,
    is_blackholed INTEGER DEFAULT 0, is_pending_review INTEGER DEFAULT 0);
CREATE TABLE message (ROWID INTEGER PRIMARY KEY AUTOINCREMENT, guid TEXT UNIQUE NOT NULL, text TEXT,
    replace INTEGER DEFAULT 0, service_center TEXT, handle_id INTEGER DEFAULT 0, subject TEXT,
    country TEXT, attributedBody BLOB, version INTEGER DEFAULT 0, type INTEGER DEFAULT 0,
    service TEXT, account TEXT, account_guid TEXT, error INTEGER DEFAULT 0, date INTEGER,
    date_read INTEGER, date_delivered INTEGER, is_delivered INTEGER DEFAULT 0,
    is_finished INTEGER DEFAULT 0, is_from_me INTEGER DEFAULT 0, is_read INTEGER DEFAULT 0,
    is_sent INTEGER DEFAULT 0, cache_has_attachments INTEGER DEFAULT 0, item_type INTEGER DEFAULT 0,
    other_handle INTEGER DEFAULT 0, group_title TEXT, group_action_type INTEGER DEFAULT 0,
    share_status INTEGER DEFAULT 0, share_direction INTEGER DEFAULT 0,
    associated_message_guid TEXT, associated_message_type INTEGER DEFAULT 0,
    balloon_bundle_id TEXT, payload_data BLOB, expressive_send_style_id TEXT,
    associated_message_range_location INTEGER DEFAULT 0,
    associated_message_range_length INTEGER DEFAULT 0, message_summary_info BLOB,
    destination_caller_id TEXT, reply_to_guid TEXT, thread_originator_guid TEXT,
    thread_originator_part TEXT, date_retracted INTEGER DEFAULT 0, date_edited INTEGER DEFAULT 0,
    associated_message_emoji TEXT);
CREATE TABLE attachment (ROWID INTEGER PRIMARY KEY AUTOINCREMENT, guid TEXT UNIQUE NOT NULL,
    created_date INTEGER DEFAULT 0, start_date INTEGER DEFAULT 0, filename TEXT, uti TEXT,
    mime_type TEXT, transfer_state INTEGER DEFAULT 0, is_outgoing INTEGER DEFAULT 0, user_info BLOB,
    transfer_name TEXT, total_bytes INTEGER DEFAULT 0, is_sticker INTEGER DEFAULT 0,
    sticker_user_info BLOB, attribution_info BLOB, hide_attachment INTEGER DEFAULT 0,
    emoji_image_short_description TEXT);
CREATE TABLE chat_handle_join (chat_id INTEGER REFERENCES chat (ROWID) ON DELETE CASCADE,
    handle_id INTEGER REFERENCES handle (ROWID) ON DELETE CASCADE, UNIQUE(chat_id, handle_id));
CREATE TABLE chat_message_join (chat_id INTEGER REFERENCES chat (ROWID) ON DELETE CASCADE,
    message_id INTEGER REFERENCES message (ROWID) ON DELETE CASCADE, message_date INTEGER DEFAULT 0,
    PRIMARY KEY (chat_id, message_id));
CREATE TABLE message_attachment_join (message_id INTEGER REFERENCES message (ROWID) ON DELETE CASCADE,
    attachment_id INTEGER REFERENCES attachment (ROWID) ON DELETE CASCADE, UNIQUE(message_id, attachment_id));
CREATE TABLE chat_recoverable_message_join (chat_id INTEGER, message_id INTEGER, delete_date INTEGER,
    ck_sync_state INTEGER DEFAULT 0, PRIMARY KEY (chat_id, message_id));
CREATE INDEX message_idx_date ON message(date);
CREATE TRIGGER after_delete_on_attachment AFTER DELETE ON attachment BEGIN
    SELECT delete_attachment_path(OLD.filename); END;
CREATE TRIGGER after_delete_on_message AFTER DELETE ON message BEGIN
    DELETE FROM handle WHERE handle.ROWID = OLD.handle_id AND
      (SELECT 1 FROM chat_handle_join WHERE handle_id = OLD.handle_id LIMIT 1) IS NULL;
    SELECT read_receipts_changed(); END;
"#;

/// A UUID-shaped message GUID (iOS GUIDs are 36 characters and parsers rely on that).
pub fn guid(label: &str) -> String {
    format!("00000000-0000-4000-8000-{label:0>12}")
}

/// Apple epoch (2001-01-01) nanoseconds from a unix timestamp in seconds.
pub fn apple_ns(unix_secs: i64) -> i64 {
    (unix_secs - 978_307_200) * 1_000_000_000
}

pub struct SmsFixture {
    pub conn: Connection,
}

impl SmsFixture {
    pub fn create(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            conn: Connection::open(path)?,
        })
    }

    pub fn handle(&self, id: &str, service: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO handle (id, service, person_centric_id) VALUES (?1, ?2, NULL)",
            params![id, service],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn chat(
        &self,
        identifier: &str,
        service: &str,
        name: Option<&str>,
        handles: &[i64],
    ) -> Result<i64> {
        let style = if handles.len() > 1 { 43 } else { 45 };
        self.conn.execute(
            "INSERT INTO chat (guid, style, chat_identifier, service_name, display_name) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![format!("{service};-;{identifier}"), style, identifier, service, name],
        )?;
        let chat = self.conn.last_insert_rowid();
        for h in handles {
            self.conn.execute(
                "INSERT INTO chat_handle_join VALUES (?1, ?2)",
                params![chat, h],
            )?;
        }
        Ok(chat)
    }

    /// Insert a plain-text message (no `attributedBody`; the parser falls back to `text`).
    pub fn message(
        &self,
        chat: i64,
        guid: &str,
        handle: i64,
        from_me: bool,
        unix: i64,
        text: &str,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO message (guid, text, handle_id, service, date, date_read, date_delivered, is_from_me, is_read)
             VALUES (?1, ?2, ?3, 'iMessage', ?4, 0, 0, ?5, 1)",
            params![guid, text, handle, apple_ns(unix), from_me],
        )?;
        let id = self.conn.last_insert_rowid();
        self.conn.execute(
            "INSERT INTO chat_message_join VALUES (?1, ?2, ?3)",
            params![chat, id, apple_ns(unix)],
        )?;
        Ok(id)
    }

    pub fn tapback(
        &self,
        chat: i64,
        guid: &str,
        handle: i64,
        unix: i64,
        target: &str,
        kind: i32,
    ) -> Result<i64> {
        let id = self.message(chat, guid, handle, false, unix, "")?;
        self.conn.execute(
            "UPDATE message SET text = NULL, associated_message_guid = ?1, associated_message_type = ?2 WHERE ROWID = ?3",
            params![format!("p:0/{target}"), kind, id],
        )?;
        Ok(id)
    }

    pub fn attachment(
        &self,
        message: i64,
        guid: &str,
        filename: &str,
        mime: &str,
        bytes: i64,
    ) -> Result<i64> {
        let transfer = filename.rsplit('/').next().unwrap_or(filename);
        self.conn.execute(
            "INSERT INTO attachment (guid, filename, mime_type, transfer_name, total_bytes) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![guid, filename, mime, transfer, bytes],
        )?;
        let id = self.conn.last_insert_rowid();
        self.conn.execute(
            "INSERT INTO message_attachment_join VALUES (?1, ?2)",
            params![message, id],
        )?;
        self.conn.execute(
            "UPDATE message SET cache_has_attachments = 1, text = coalesce(text, '') || char(65532) WHERE ROWID = ?1",
            [message],
        )?;
        Ok(id)
    }
}

/// Unix time of the first sample message (2024-05-01T12:00:00Z).
pub const SAMPLE_T0: i64 = 1_714_564_800;

/// Build a complete synthetic backup at `root`.
///
/// * stage 1: a 1:1 chat with Jane (text, a photo, a tapback) and a group chat.
/// * stage 2: stage 1 plus two new messages (one a threaded reply) and an edit
///   of an existing message, as a later backup of the same phone would look.
pub fn build_sample_backup(
    root: &Path,
    udid: &str,
    password: Option<&str>,
    stage: u8,
    backup_date: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    use crate::backup::fixture::FixtureBackup;

    let work = tempfile::tempdir()?;
    let sms_path = work.path().join("sms.db");
    let f = SmsFixture::create(&sms_path)?;
    let jane = f.handle("+15550100000", "iMessage")?;
    let bob = f.handle("+15550100001", "iMessage")?;
    let dm = f.chat("+15550100000", "iMessage", None, &[jane])?;
    let group = f.chat("chat123456789", "iMessage", Some("Road Trip"), &[jane, bob])?;
    let t = SAMPLE_T0;
    f.message(
        dm,
        &guid("M1"),
        jane,
        false,
        t,
        "Hey there! Are we still on for Friday?",
    )?;
    f.message(dm, &guid("M2"), jane, true, t + 60, "Yes! See you at 7")?;
    let photo = f.message(dm, &guid("M3"), jane, false, t + 120, "")?;
    let photo_path = "~/Library/SMS/Attachments/0a/10/AT-GUID-1/IMG_0001.JPG";
    f.attachment(photo, "AT-GUID-1", photo_path, "image/jpeg", 4)?;
    f.tapback(dm, &guid("M4"), jane, t + 180, &guid("M2"), 2000)?;
    f.message(group, &guid("M5"), bob, false, t + 240, "Who's driving?")?;
    // An attachment that was offloaded to iCloud and is not in the backup.
    let missing = f.message(group, &guid("M5B"), bob, false, t + 250, "")?;
    f.attachment(
        missing,
        "AT-GUID-2",
        "~/Library/SMS/Attachments/0b/11/AT-GUID-2/map.png",
        "image/png",
        10,
    )?;
    if stage >= 2 {
        f.message(
            dm,
            &guid("M6"),
            jane,
            false,
            t + 86_400,
            "Thanks for dinner 🍝",
        )?;
        let reply = f.message(dm, &guid("M7"), jane, true, t + 86_460, "Anytime!")?;
        f.conn.execute(
            "UPDATE message SET thread_originator_guid = ?1, thread_originator_part = '0:0:38' WHERE ROWID = ?2",
            rusqlite::params![guid("M1"), reply],
        )?;
        f.conn.execute(
            "UPDATE message SET text = 'Yes! See you at 7:30', date_edited = ?1 WHERE guid = ?2",
            rusqlite::params![apple_ns(t + 86_500), guid("M2")],
        )?;
    }
    drop(f);

    let ab_path = work.path().join("AddressBook.sqlitedb");
    let mut people: Vec<(&str, &str, &[&str])> =
        vec![("Jane", "Appleseed", &["+1 (555) 010-0000"])];
    if stage >= 2 {
        people.push(("Bob", "Builder", &["555-010-0001"]));
    }
    crate::contacts::write_test_address_book(&ab_path, &people)?;

    let b = FixtureBackup::create(root, udid, password)?;
    b.write_info(udid, backup_date)?;
    b.add_file(
        crate::backup::SMS_DB.0,
        crate::backup::SMS_DB.1,
        &std::fs::read(&sms_path)?,
    )?;
    b.add_file(
        crate::backup::ADDRESS_BOOK_DB.0,
        crate::backup::ADDRESS_BOOK_DB.1,
        &std::fs::read(&ab_path)?,
    )?;
    // A tiny (fake) JPEG body.
    b.add_file("MediaDomain", &photo_path[2..], &[0xFF, 0xD8, 0xFF, 0xD9])?;
    b.finish()?;
    Ok(())
}
