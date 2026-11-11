//! Read-side queries backing the JSON API.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use imsg_core::contacts::handle_key;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde::Serialize;
use serde_json::value::RawValue;

/// Contact names keyed by [`handle_key`].
#[derive(Default, Clone)]
pub struct Names(pub HashMap<String, String>);

impl Names {
    pub fn load(conn: &Connection) -> Result<Self> {
        let mut stmt = conn.prepare("SELECT key, name FROM contacts")?;
        let map = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(Self(map))
    }

    pub fn contact(&self, handle: &str) -> Option<&str> {
        handle_key(handle)
            .and_then(|k| self.0.get(&k))
            .map(String::as_str)
    }

    /// Contact name, or a nicely formatted phone number / the raw address.
    pub fn display(&self, handle: &str) -> String {
        if let Some(n) = self.contact(handle) {
            return n.to_owned();
        }
        let digits: String = handle.chars().filter(char::is_ascii_digit).collect();
        if !handle.contains('@') && digits.len() == 11 && digits.starts_with('1') {
            return format!("+1 ({}) {}-{}", &digits[1..4], &digits[4..7], &digits[7..]);
        }
        if !handle.contains('@') && digits.len() == 10 && !handle.starts_with('+') {
            return format!("({}) {}-{}", &digits[..3], &digits[3..6], &digits[6..]);
        }
        handle.to_owned()
    }
}

#[derive(Serialize)]
pub struct Participant {
    pub handle: String,
    pub name: String,
    pub known: bool,
}

#[derive(Serialize)]
pub struct ConversationOut {
    pub id: i64,
    pub title: String,
    pub is_group: bool,
    pub participants: Vec<Participant>,
    pub services: Vec<String>,
    pub last_ms: Option<i64>,
    pub last_preview: Option<String>,
    pub last_from_me: bool,
    pub last_sender: Option<String>,
    pub msg_count: i64,
}

fn title_for(
    display_name: Option<String>,
    participants: &[Participant],
    key: &str,
    names: &Names,
) -> String {
    if let Some(n) = display_name.filter(|n| !n.trim().is_empty()) {
        return n;
    }
    match participants.len() {
        0 => names.display(key),
        1 => participants[0].name.clone(),
        _ => {
            let firsts: Vec<String> = participants
                .iter()
                .map(|p| {
                    if p.known {
                        p.name.split(' ').next().unwrap_or(&p.name).to_owned()
                    } else {
                        p.name.clone()
                    }
                })
                .collect();
            if firsts.len() > 4 {
                format!("{} & {} others", firsts[..3].join(", "), firsts.len() - 3)
            } else {
                let (last, rest) = firsts.split_last().unwrap();
                format!("{} & {}", rest.join(", "), last)
            }
        }
    }
}

fn participants_of(conn: &Connection, conv: i64, names: &Names) -> Result<Vec<Participant>> {
    let mut stmt =
        conn.prepare_cached("SELECT handle FROM participants WHERE conv_id = ?1 ORDER BY handle")?;
    let handles: Vec<String> = stmt
        .query_map([conv], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut out: Vec<Participant> = handles
        .into_iter()
        .map(|h| Participant {
            name: names.display(&h),
            known: names.contact(&h).is_some(),
            handle: h,
        })
        .collect();
    // The same person can appear under several handles (phone + email).
    out.sort_by_key(|a| a.name.to_lowercase());
    out.dedup_by(|a, b| a.known && b.known && a.name == b.name);
    Ok(out)
}

fn conversation_from_row(conn: &Connection, r: &Row, names: &Names) -> Result<ConversationOut> {
    let id: i64 = r.get("id")?;
    let key: String = r.get("key")?;
    let participants = participants_of(conn, id, names)?;
    let services: Option<String> = r.get("services")?;
    Ok(ConversationOut {
        id,
        title: title_for(r.get("display_name")?, &participants, &key, names),
        is_group: r.get("is_group")?,
        services: services
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        last_ms: r.get("last_ms")?,
        last_preview: r.get("last_preview")?,
        last_from_me: r.get::<_, Option<bool>>("last_from_me")?.unwrap_or(false),
        last_sender: r
            .get::<_, Option<String>>("last_sender")?
            .map(|h| names.display(&h)),
        msg_count: r.get("msg_count")?,
        participants,
    })
}

pub fn conversations(conn: &Connection, names: &Names) -> Result<Vec<ConversationOut>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM conversations WHERE msg_count > 0 OR last_ms IS NOT NULL ORDER BY last_ms DESC NULLS LAST",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        out.push(conversation_from_row(conn, r, names)?);
    }
    Ok(out)
}

pub fn conversation(conn: &Connection, names: &Names, id: i64) -> Result<Option<ConversationOut>> {
    let mut stmt = conn.prepare("SELECT * FROM conversations WHERE id = ?1")?;
    let mut rows = stmt.query([id])?;
    match rows.next()? {
        Some(r) => Ok(Some(conversation_from_row(conn, r, names)?)),
        None => Ok(None),
    }
}

#[derive(Serialize)]
pub struct Reaction {
    pub emoji: String,
    pub from_me: bool,
    pub name: String,
    pub part: i64,
}

#[derive(Serialize)]
pub struct ReplyRef {
    pub id: i64,
    pub name: String,
    pub text: String,
}

#[derive(Serialize)]
pub struct MessageOut {
    pub id: i64,
    pub guid: String,
    pub from_me: bool,
    pub sender: Option<String>,
    pub sender_name: Option<String>,
    pub date_ms: i64,
    pub read_ms: Option<i64>,
    pub delivered_ms: Option<i64>,
    pub edited_ms: Option<i64>,
    pub service: Option<String>,
    pub kind: String,
    pub body: Box<RawValue>,
    pub expressive: Option<String>,
    pub reply_to: Option<ReplyRef>,
    pub reactions: Vec<Reaction>,
    pub reply_count: i64,
}

#[derive(Serialize)]
pub struct Page {
    pub messages: Vec<MessageOut>,
    pub has_more_before: bool,
    pub has_more_after: bool,
    /// Display names for every handle mentioned in the page's bodies.
    pub names: BTreeMap<String, String>,
}

pub enum Cursor {
    Latest,
    Before(i64),
    After(i64),
    Around(i64),
}

/// `(emoji, part, from_me, sender, removed)`
type ReactionEvent = (String, i64, bool, Option<String>, bool);

/// `(device, mime, name)` of an archived attachment.
pub type AttachmentLocation = (String, Option<String>, Option<String>);

const COLS: &str = "id, guid, is_from_me, sender, date_ms, read_ms, delivered_ms, edited_ms, service, kind, body, expressive, thread_guid";

struct RawMsg {
    out: MessageOut,
    thread_guid: Option<String>,
}

fn raw_from_row(r: &Row, names: &Names) -> rusqlite::Result<RawMsg> {
    let sender: Option<String> = r.get(3)?;
    let body: String = r.get(10)?;
    Ok(RawMsg {
        out: MessageOut {
            id: r.get(0)?,
            guid: r.get(1)?,
            from_me: r.get(2)?,
            sender_name: sender.as_deref().map(|s| names.display(s)),
            sender,
            date_ms: r.get(4)?,
            read_ms: r.get(5)?,
            delivered_ms: r.get(6)?,
            edited_ms: r.get(7)?,
            service: r.get(8)?,
            kind: r.get(9)?,
            body: RawValue::from_string(body)
                .unwrap_or_else(|_| RawValue::from_string("{}".into()).unwrap()),
            expressive: r.get(11)?,
            reply_to: None,
            reactions: vec![],
            reply_count: 0,
        },
        thread_guid: r.get(12)?,
    })
}

fn fetch(
    conn: &Connection,
    names: &Names,
    conv: i64,
    cmp: &str,
    pivot: Option<i64>,
    desc: bool,
    limit: i64,
) -> Result<Vec<RawMsg>> {
    let order = if desc { "DESC" } else { "ASC" };
    let pivot_clause = if pivot.is_some() {
        format!("AND (date_ms, id) {cmp} (SELECT date_ms, id FROM messages WHERE id = ?3)")
    } else {
        String::new()
    };
    let sql = format!(
        "SELECT {COLS} FROM messages WHERE conv_id = ?1 AND kind IN ('message', 'announcement') {pivot_clause}
         ORDER BY date_ms {order}, id {order} LIMIT ?2"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = match pivot {
        Some(p) => stmt
            .query_map(params![conv, limit, p], |r| raw_from_row(r, names))?
            .collect::<Result<Vec<_>, _>>()?,
        None => stmt
            .query_map(params![conv, limit], |r| raw_from_row(r, names))?
            .collect::<Result<Vec<_>, _>>()?,
    };
    Ok(rows)
}

pub fn messages(
    conn: &Connection,
    names: &Names,
    conv: i64,
    cursor: Cursor,
    limit: i64,
) -> Result<Page> {
    let limit = limit.clamp(1, 500);
    let (mut before, mut after, more_before, more_after) = match cursor {
        Cursor::Latest => {
            let mut v = fetch(conn, names, conv, "", None, true, limit + 1)?;
            let more = v.len() as i64 > limit;
            v.truncate(limit as usize);
            (v, vec![], more, false)
        }
        Cursor::Before(id) => {
            let mut v = fetch(conn, names, conv, "<", Some(id), true, limit + 1)?;
            let more = v.len() as i64 > limit;
            v.truncate(limit as usize);
            (v, vec![], more, true)
        }
        Cursor::After(id) => {
            let mut v = fetch(conn, names, conv, ">", Some(id), false, limit + 1)?;
            let more = v.len() as i64 > limit;
            v.truncate(limit as usize);
            (vec![], v, true, more)
        }
        Cursor::Around(id) => {
            let half = (limit / 2).max(1);
            let mut b = fetch(conn, names, conv, "<=", Some(id), true, half + 1)?;
            let mut a = fetch(conn, names, conv, ">", Some(id), false, half + 1)?;
            let mb = b.len() as i64 > half;
            let ma = a.len() as i64 > half;
            b.truncate(half as usize);
            a.truncate(half as usize);
            (b, a, mb, ma)
        }
    };
    before.reverse();
    before.append(&mut after);
    let mut raws = before;

    // Reactions, replies and reply counts for the page.
    if !raws.is_empty() {
        let guids: Vec<String> = raws.iter().map(|m| m.out.guid.clone()).collect();
        let placeholders = vec!["?"; guids.len()].join(",");
        let mut reactions: HashMap<String, Vec<ReactionEvent>> = HashMap::new();
        {
            let mut stmt = conn.prepare(&format!(
                "SELECT assoc_guid, coalesce(assoc_part, 0), is_from_me, sender, reaction, reaction_removed
                 FROM messages WHERE kind = 'reaction' AND assoc_guid IN ({placeholders}) ORDER BY date_ms, id"
            ))?;
            let mut rows = stmt.query(params_from_iter(guids.iter()))?;
            while let Some(r) = rows.next()? {
                let target: String = r.get(0)?;
                let entry = (
                    r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(5)?,
                );
                reactions.entry(target).or_default().push(entry);
            }
        }
        let mut reply_counts: HashMap<String, i64> = HashMap::new();
        {
            let mut stmt = conn.prepare(&format!(
                "SELECT thread_guid, count(*) FROM messages WHERE thread_guid IN ({placeholders}) GROUP BY thread_guid"
            ))?;
            let mut rows = stmt.query(params_from_iter(guids.iter()))?;
            while let Some(r) = rows.next()? {
                reply_counts.insert(r.get(0)?, r.get(1)?);
            }
        }
        let mut origin = conn
            .prepare_cached("SELECT id, is_from_me, sender, text FROM messages WHERE guid = ?1")?;
        for m in &mut raws {
            if let Some(list) = reactions.remove(&m.out.guid) {
                // Replay add/remove events per (sender, part, emoji).
                let mut state: Vec<(String, i64, bool, Option<String>)> = Vec::new();
                for (emoji, part, from_me, sender, removed) in list {
                    let key = (emoji.clone(), part, from_me, sender.clone());
                    state.retain(|s| *s != key);
                    if !removed {
                        state.push(key);
                    }
                }
                m.out.reactions = state
                    .into_iter()
                    .map(|(emoji, part, from_me, sender)| Reaction {
                        emoji,
                        part,
                        from_me,
                        name: if from_me {
                            "You".into()
                        } else {
                            sender.map(|s| names.display(&s)).unwrap_or_default()
                        },
                    })
                    .collect();
            }
            m.out.reply_count = reply_counts.get(&m.out.guid).copied().unwrap_or(0);
            if let Some(tg) = &m.thread_guid {
                m.out.reply_to = origin
                    .query_row([tg], |r| {
                        let from_me: bool = r.get(1)?;
                        let sender: Option<String> = r.get(2)?;
                        Ok(ReplyRef {
                            id: r.get(0)?,
                            name: if from_me {
                                "You".into()
                            } else {
                                sender.map(|s| names.display(&s)).unwrap_or_default()
                            },
                            text: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        })
                    })
                    .optional()?;
            }
        }
    }

    let mut page_names = BTreeMap::new();
    for m in &raws {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(m.out.body.get())
            && let Some(parts) = v.get("parts").and_then(|p| p.as_array())
        {
            for p in parts {
                if let Some(t) = p.get("target").and_then(|t| t.as_str()) {
                    page_names.insert(t.to_owned(), names.display(t));
                }
            }
        }
    }
    Ok(Page {
        messages: raws.into_iter().map(|m| m.out).collect(),
        has_more_before: more_before,
        has_more_after: more_after,
        names: page_names,
    })
}

#[derive(Serialize)]
pub struct SearchHit {
    pub id: i64,
    pub conv_id: i64,
    pub conv_title: String,
    pub date_ms: i64,
    pub from_me: bool,
    pub sender_name: Option<String>,
    /// Snippet with matches wrapped in U+0002 / U+0003 (the UI turns them into highlights).
    pub snippet: String,
}

/// Turn free text into a safe FTS5 query: every word must appear (prefix match).
pub fn fts_query(q: &str) -> Option<String> {
    let terms: Vec<String> = q
        .split_whitespace()
        .map(|t| t.replace('"', ""))
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\"*"))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

pub fn search(
    conn: &Connection,
    names: &Names,
    q: &str,
    conv: Option<i64>,
    limit: i64,
) -> Result<Vec<SearchHit>> {
    let Some(fq) = fts_query(q) else {
        return Ok(vec![]);
    };
    let mut stmt = conn.prepare(
        "SELECT m.id, m.conv_id, m.date_ms, m.is_from_me, m.sender,
                snippet(messages_fts, 0, char(2), char(3), '…', 16)
         FROM messages_fts JOIN messages m ON m.id = messages_fts.rowid
         WHERE messages_fts MATCH ?1 AND (?2 IS NULL OR m.conv_id = ?2)
         ORDER BY m.date_ms DESC LIMIT ?3",
    )?;
    let rows: Vec<(i64, i64, i64, bool, Option<String>, String)> = stmt
        .query_map(params![fq, conv, limit.clamp(1, 500)], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    let mut titles: HashMap<i64, String> = HashMap::new();
    let mut out = Vec::with_capacity(rows.len());
    for (id, conv_id, date_ms, from_me, sender, snippet) in rows {
        if !titles.contains_key(&conv_id)
            && let Some(c) = conversation(conn, names, conv_id)?
        {
            titles.insert(conv_id, c.title);
        }
        out.push(SearchHit {
            id,
            conv_id,
            conv_title: titles.get(&conv_id).cloned().unwrap_or_default(),
            date_ms,
            from_me,
            sender_name: sender.map(|s| names.display(&s)),
            snippet,
        });
    }
    Ok(out)
}

#[derive(Serialize)]
pub struct Status {
    pub snapshots: Vec<SnapshotStatus>,
    pub conversations: i64,
    pub messages: i64,
    pub contacts: i64,
}

#[derive(Serialize)]
pub struct SnapshotStatus {
    pub device: String,
    pub snapshot_id: String,
    pub backup_ms: Option<i64>,
    pub ingested_ms: i64,
    pub messages: i64,
}

pub fn status(conn: &Connection) -> Result<Status> {
    let mut stmt = conn.prepare(
        "SELECT device, snapshot_id, backup_ms, ingested_ms, messages FROM snapshots ORDER BY backup_ms DESC",
    )?;
    let snapshots = stmt
        .query_map([], |r| {
            Ok(SnapshotStatus {
                device: r.get(0)?,
                snapshot_id: r.get(1)?,
                backup_ms: r.get(2)?,
                ingested_ms: r.get(3)?,
                messages: r.get(4)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let count = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0));
    Ok(Status {
        snapshots,
        conversations: count("SELECT count(*) FROM conversations WHERE msg_count > 0")?,
        messages: count("SELECT count(*) FROM messages WHERE kind = 'message'")?,
        contacts: count("SELECT count(*) FROM contacts")?,
    })
}

/// Where an attachment lives: `(device, mime, name)`.
pub fn attachment(conn: &Connection, file_id: &str) -> Result<Vec<AttachmentLocation>> {
    let mut stmt =
        conn.prepare_cached("SELECT device, mime, name FROM attachments WHERE file_id = ?1")?;
    Ok(stmt
        .query_map([file_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?)
}
