//! Reading names from the iOS Contacts database (`AddressBook.sqlitedb`) and
//! matching them to Messages handles (phone numbers / email addresses).

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

const PROP_PHONE: i64 = 3;
const PROP_EMAIL: i64 = 4;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Contact {
    pub first: Option<String>,
    pub last: Option<String>,
    pub nickname: Option<String>,
    pub organization: Option<String>,
    pub phones: Vec<String>,
    pub emails: Vec<String>,
}

impl Contact {
    pub fn display_name(&self) -> Option<String> {
        let full = [self.first.as_deref(), self.last.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if !full.is_empty() {
            return Some(full);
        }
        [self.nickname.as_deref(), self.organization.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(str::to_owned)
    }
}

/// Load every contact from an `AddressBook.sqlitedb`.
pub fn load_contacts(path: &Path) -> Result<Vec<Contact>> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut people: HashMap<i64, Contact> = HashMap::new();
    {
        let mut stmt =
            db.prepare("SELECT ROWID, First, Last, Nickname, Organization FROM ABPerson")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                Contact {
                    first: r.get(1)?,
                    last: r.get(2)?,
                    nickname: r.get(3)?,
                    organization: r.get(4)?,
                    ..Default::default()
                },
            ))
        })?;
        for row in rows {
            let (id, c) = row?;
            people.insert(id, c);
        }
    }
    let mut stmt =
        db.prepare("SELECT record_id, property, value FROM ABMultiValue WHERE value IS NOT NULL")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (id, prop, value) = row?;
        if let Some(c) = people.get_mut(&id) {
            match prop {
                PROP_PHONE => c.phones.push(value),
                PROP_EMAIL => c.emails.push(value),
                _ => {}
            }
        }
    }
    let mut out: Vec<(i64, Contact)> = people.into_iter().collect();
    out.sort_by_key(|(id, _)| *id);
    Ok(out.into_iter().map(|(_, c)| c).collect())
}

/// Normalize a handle (phone number or email) into a lookup key.
///
/// Emails are lower-cased. Phone numbers keep their digits only; numbers with
/// at least 10 digits are keyed on their last 10 so that `+1 (555) 010-0000`,
/// `5550100000` and `+15550100000` all match. Short codes are kept as-is.
pub fn handle_key(handle: &str) -> Option<String> {
    let h = handle.trim();
    if h.is_empty() {
        return None;
    }
    if h.contains('@') {
        return Some(h.to_lowercase());
    }
    let digits: String = h.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return Some(h.to_lowercase());
    }
    if digits.len() >= 10 {
        Some(digits[digits.len() - 10..].to_owned())
    } else {
        Some(digits)
    }
}

/// Build a `handle_key -> display name` map.
pub fn name_index(contacts: &[Contact]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for c in contacts {
        let Some(name) = c.display_name() else {
            continue;
        };
        for h in c.phones.iter().chain(c.emails.iter()) {
            if let Some(k) = handle_key(h) {
                map.entry(k).or_insert_with(|| name.clone());
            }
        }
    }
    map
}

/// Create an `AddressBook.sqlitedb` with the subset of the schema we read (tests).
pub fn write_test_address_book(path: &Path, people: &[(&str, &str, &[&str])]) -> Result<()> {
    let db = Connection::open(path)?;
    db.execute_batch(
        "CREATE TABLE ABPerson (ROWID INTEGER PRIMARY KEY AUTOINCREMENT, First TEXT, Last TEXT, Middle TEXT,
            Organization TEXT, Nickname TEXT);
         CREATE TABLE ABMultiValue (UID INTEGER PRIMARY KEY, record_id INTEGER, property INTEGER,
            identifier INTEGER, label INTEGER, value TEXT);",
    )?;
    for (first, last, handles) in people {
        db.execute(
            "INSERT INTO ABPerson (First, Last) VALUES (?1, ?2)",
            [first, last],
        )?;
        let id = db.last_insert_rowid();
        for h in *handles {
            let prop = if h.contains('@') {
                PROP_EMAIL
            } else {
                PROP_PHONE
            };
            db.execute(
                "INSERT INTO ABMultiValue (record_id, property, value) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, prop, h],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(handle_key("+1 (555) 010-0000"), handle_key("5550100000"));
        assert_eq!(handle_key("+15550100000"), handle_key("555.010.0000"));
        assert_eq!(handle_key("Jane@Example.com").unwrap(), "jane@example.com");
        assert_eq!(handle_key("22395").unwrap(), "22395");
    }

    #[test]
    fn load_and_index() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ab.sqlitedb");
        write_test_address_book(
            &p,
            &[("Jane", "Doe", &["(555) 010-0000", "jane@example.com"])],
        )
        .unwrap();
        let contacts = load_contacts(&p).unwrap();
        let idx = name_index(&contacts);
        assert_eq!(
            idx.get(&handle_key("+15550100000").unwrap()).unwrap(),
            "Jane Doe"
        );
        assert_eq!(idx.get("jane@example.com").unwrap(), "Jane Doe");
    }
}
