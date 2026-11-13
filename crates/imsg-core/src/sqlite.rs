//! Small SQLite helpers shared by the extractor and the server.

use rusqlite::Connection;

/// Does `table` have a column named `column`?
pub fn has_column(db: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = db.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for n in names {
        if n?.eq_ignore_ascii_case(column) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Is there an index on `table` whose first column is `column`?
pub fn has_index_on(db: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let indexes: Vec<String> = db
        .prepare(&format!("PRAGMA index_list(\"{table}\")"))?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<Result<_, _>>()?;
    for index in indexes {
        let first: Option<String> = db
            .prepare(&format!(
                "PRAGMA index_info(\"{}\")",
                index.replace('"', "\"\"")
            ))?
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(2)?))
            })?
            .filter_map(Result::ok)
            .find(|(seq, _)| *seq == 0)
            .and_then(|(_, name)| name);
        if first.is_some_and(|c| c.eq_ignore_ascii_case(column)) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// imessage-database counts each message's replies with a correlated
/// subquery on `message.thread_originator_guid`. Without an index that is a
/// full table scan per message, i.e. quadratic in the number of messages.
/// Returns true if this added the index.
pub fn ensure_reply_index(db: &Connection) -> rusqlite::Result<bool> {
    if !has_column(db, "message", "thread_originator_guid")?
        || has_index_on(db, "message", "thread_originator_guid")?
    {
        return Ok(false);
    }
    db.execute_batch(
        "CREATE INDEX IF NOT EXISTS imsg_message_thread_originator_guid ON message (thread_originator_guid);",
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_index() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE message (ROWID INTEGER PRIMARY KEY, guid TEXT, thread_originator_guid TEXT);")
            .unwrap();
        assert!(!has_index_on(&db, "message", "thread_originator_guid").unwrap());
        assert!(ensure_reply_index(&db).unwrap());
        assert!(has_index_on(&db, "message", "thread_originator_guid").unwrap());
        assert!(!ensure_reply_index(&db).unwrap(), "idempotent");
        let old = Connection::open_in_memory().unwrap();
        old.execute_batch("CREATE TABLE message (ROWID INTEGER PRIMARY KEY, guid TEXT);")
            .unwrap();
        assert!(
            !ensure_reply_index(&old).unwrap(),
            "pre-iOS 14 schemas have no replies"
        );
    }
}
