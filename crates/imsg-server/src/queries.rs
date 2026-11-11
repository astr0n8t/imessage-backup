//! Read-side queries backing the JSON API.
//!
//! Every query is restricted to a [`View`]; call [`View::install`] on the
//! connection first (the web layer does this for each request).

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use imsg_core::contacts::handle_key;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde::Serialize;
use serde_json::value::RawValue;

use crate::view::View;

/// Contact names keyed by [`handle_key`].
#[derive(Default, Clone)]
pub struct Names(pub HashMap<String, String>);

impl Names {
    /// Names from the Contacts DBs of the devices this view can see. When
    /// devices disagree, the most recently backed-up Contacts DB wins.
    pub fn load(conn: &Connection, view: &View) -> Result<Self> {
        let sql = format!(
            "SELECT c.key, c.name FROM contacts c JOIN contact_sources s ON s.device = c.device
             WHERE {} OR c.device IN (SELECT DISTINCT a.device FROM conv_stats a WHERE {})
             ORDER BY s.snap_order",
            if view.all {
                "1".to_owned()
            } else {
                "c.device IN (SELECT d FROM temp.v_dev)".to_owned()
            },
            view.filter("a"),
        );
        let mut stmt = conn.prepare(&sql)?;
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

/// Devices whose copy of this conversation the view can see.
fn visible_devices_sql(view: &View) -> String {
    format!(
        "(SELECT a.device FROM conv_stats a WHERE a.conv_id = ?1 AND {})",
        view.filter("a")
    )
}

/// Build one conversation summary. `count` is the exact (de-duplicated)
/// message count when requested, otherwise `approx_count`.
fn conversation_out(
    conn: &Connection,
    view: &View,
    names: &Names,
    id: i64,
    last_ms: Option<i64>,
    approx_count: i64,
    exact: bool,
) -> Result<ConversationOut> {
    let devices = visible_devices_sql(view);
    let key: String = conn.query_row("SELECT key FROM conversations WHERE id = ?1", [id], |r| {
        r.get(0)
    })?;
    let (display_name, is_group, services): (Option<String>, bool, String) = conn
        .query_row(
            &format!(
                "SELECT (SELECT display_name FROM conv_meta WHERE conv_id = ?1 AND device IN {devices}
                            AND display_name IS NOT NULL ORDER BY snap_order DESC LIMIT 1),
                        coalesce(max(is_group), 0), coalesce(group_concat(services, ','), '')
                 FROM conv_meta WHERE conv_id = ?1 AND device IN {devices}"
            ),
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .unwrap_or((None, false, String::new()));
    let mut services: Vec<String> = services
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    services.sort();
    services.dedup();

    let mut stmt = conn.prepare_cached(&format!(
        "SELECT DISTINCT handle FROM participants WHERE conv_id = ?1 AND device IN {devices} ORDER BY handle"
    ))?;
    let handles: Vec<String> = stmt
        .query_map([id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut participants: Vec<Participant> = handles
        .into_iter()
        .map(|h| Participant {
            name: names.display(&h),
            known: names.contact(&h).is_some(),
            handle: h,
        })
        .collect();
    // The same person can appear under several handles (phone + email).
    participants.sort_by_key(|p| p.name.to_lowercase());
    participants.dedup_by(|a, b| a.known && b.known && a.name == b.name);

    let vis = view.filter("m");
    let last: Option<(String, String, String, bool, Option<String>)> = conn
        .query_row(
            &format!(
                "SELECT coalesce(m.text, ''), m.kind, m.body, m.is_from_me, m.sender FROM messages m
                 WHERE m.conv_id = ?1 AND m.kind IN ('message', 'announcement') AND {vis}
                 ORDER BY m.date_ms DESC, m.id DESC LIMIT 1"
            ),
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let (last_preview, last_from_me, last_sender) = match last {
        Some((text, kind, body, from_me, sender)) => {
            let preview = if !text.is_empty() {
                text
            } else if kind == "announcement" {
                "(group update)".into()
            } else if body.contains(r#""t":"attachment""#) {
                "📎 Attachment".into()
            } else {
                String::new()
            };
            (Some(preview), from_me, sender.map(|s| names.display(&s)))
        }
        None => (None, false, None),
    };
    let msg_count = if exact {
        conn.query_row(
            &format!(
                "SELECT count(*) FROM messages m WHERE m.conv_id = ?1 AND m.kind = 'message' AND {vis} AND {}",
                view.dedupe("m")
            ),
            [id],
            |r| r.get(0),
        )?
    } else {
        approx_count
    };
    Ok(ConversationOut {
        id,
        title: title_for(display_name, &participants, &key, names),
        is_group,
        participants,
        services,
        last_ms,
        last_preview,
        last_from_me,
        last_sender,
        msg_count,
    })
}

/// Conversations with at least one visible message, most recent first.
/// `msg_count` here is approximate (copies on several devices count twice).
pub fn conversations(
    conn: &Connection,
    view: &View,
    names: &Names,
) -> Result<Vec<ConversationOut>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT a.conv_id, max(a.last_ms), sum(a.msg_count) FROM conv_stats a WHERE {}
         GROUP BY a.conv_id ORDER BY max(a.last_ms) DESC",
        view.filter("a")
    ))?;
    let rows: Vec<(i64, Option<i64>, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    rows.into_iter()
        .map(|(id, last, count)| conversation_out(conn, view, names, id, last, count, false))
        .collect()
}

/// One conversation, if the view can see any of it.
pub fn conversation(
    conn: &Connection,
    view: &View,
    names: &Names,
    id: i64,
) -> Result<Option<ConversationOut>> {
    let last: Option<(Option<i64>, i64)> = conn
        .query_row(
            &format!(
                "SELECT max(a.last_ms), sum(a.msg_count) FROM conv_stats a WHERE a.conv_id = ?1 AND {}
                 HAVING count(*) > 0",
                view.filter("a")
            ),
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match last {
        Some((last, count)) => Ok(Some(conversation_out(
            conn, view, names, id, last, count, true,
        )?)),
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

const COLS: &str = "m.id, m.guid, m.is_from_me, m.sender, m.date_ms, m.read_ms, m.delivered_ms, m.edited_ms, \
                    m.service, m.kind, m.body, m.expressive, m.thread_guid";

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

#[allow(clippy::too_many_arguments)]
fn fetch(
    conn: &Connection,
    view: &View,
    names: &Names,
    conv: i64,
    cmp: &str,
    pivot: Option<i64>,
    desc: bool,
    limit: i64,
) -> Result<Vec<RawMsg>> {
    let order = if desc { "DESC" } else { "ASC" };
    let pivot_clause = if pivot.is_some() {
        format!("AND (m.date_ms, m.id) {cmp} (SELECT date_ms, id FROM messages WHERE id = ?3)")
    } else {
        String::new()
    };
    let sql = format!(
        "SELECT {COLS} FROM messages m WHERE m.conv_id = ?1 AND m.kind IN ('message', 'announcement')
           AND {} AND {} {pivot_clause}
         ORDER BY m.date_ms {order}, m.id {order} LIMIT ?2",
        view.filter("m"),
        view.dedupe("m"),
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

/// Is message `id` visible to `view`?
pub fn message_visible(conn: &Connection, view: &View, id: i64) -> Result<bool> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT 1 FROM messages m WHERE m.id = ?1 AND {}",
                view.filter("m")
            ),
            [id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub fn messages(
    conn: &Connection,
    view: &View,
    names: &Names,
    conv: i64,
    cursor: Cursor,
    limit: i64,
) -> Result<Page> {
    let limit = limit.clamp(1, 500);
    // A pivot message from outside the view would leak its position.
    if let Cursor::Before(p) | Cursor::After(p) | Cursor::Around(p) = cursor
        && !message_visible(conn, view, p)?
    {
        return Ok(Page {
            messages: vec![],
            has_more_before: false,
            has_more_after: false,
            names: BTreeMap::new(),
        });
    }
    let (mut before, mut after, more_before, more_after) = match cursor {
        Cursor::Latest => {
            let mut v = fetch(conn, view, names, conv, "", None, true, limit + 1)?;
            let more = v.len() as i64 > limit;
            v.truncate(limit as usize);
            (v, vec![], more, false)
        }
        Cursor::Before(id) => {
            let mut v = fetch(conn, view, names, conv, "<", Some(id), true, limit + 1)?;
            let more = v.len() as i64 > limit;
            v.truncate(limit as usize);
            (v, vec![], more, true)
        }
        Cursor::After(id) => {
            let mut v = fetch(conn, view, names, conv, ">", Some(id), false, limit + 1)?;
            let more = v.len() as i64 > limit;
            v.truncate(limit as usize);
            (vec![], v, true, more)
        }
        Cursor::Around(id) => {
            let half = (limit / 2).max(1);
            let mut b = fetch(conn, view, names, conv, "<=", Some(id), true, half + 1)?;
            let mut a = fetch(conn, view, names, conv, ">", Some(id), false, half + 1)?;
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
    let vis = view.filter("m");

    // Reactions, replies and reply counts for the page.
    if !raws.is_empty() {
        let guids: Vec<String> = raws.iter().map(|m| m.out.guid.clone()).collect();
        let placeholders = vec!["?"; guids.len()].join(",");
        let mut reactions: HashMap<String, Vec<ReactionEvent>> = HashMap::new();
        {
            let mut stmt = conn.prepare(&format!(
                "SELECT m.assoc_guid, coalesce(m.assoc_part, 0), m.is_from_me, m.sender, m.reaction, m.reaction_removed
                 FROM messages m WHERE m.kind = 'reaction' AND m.assoc_guid IN ({placeholders}) AND {vis}
                 ORDER BY m.date_ms, m.id"
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
                "SELECT m.thread_guid, count(DISTINCT m.guid) FROM messages m
                 WHERE m.thread_guid IN ({placeholders}) AND {vis} GROUP BY m.thread_guid"
            ))?;
            let mut rows = stmt.query(params_from_iter(guids.iter()))?;
            while let Some(r) = rows.next()? {
                reply_counts.insert(r.get(0)?, r.get(1)?);
            }
        }
        let mut origin = conn.prepare_cached(&format!(
            "SELECT m.id, m.is_from_me, m.sender, m.text FROM messages m WHERE m.guid = ?1 AND {vis}
             ORDER BY m.snap_order DESC LIMIT 1"
        ))?;
        for m in &mut raws {
            if let Some(list) = reactions.remove(&m.out.guid) {
                // Replay add/remove events per (sender, part, emoji). Copies from
                // several devices collapse because their keys are equal.
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
    view: &View,
    names: &Names,
    q: &str,
    conv: Option<i64>,
    limit: i64,
) -> Result<Vec<SearchHit>> {
    let Some(fq) = fts_query(q) else {
        return Ok(vec![]);
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT m.id, m.conv_id, m.date_ms, m.is_from_me, m.sender,
                snippet(messages_fts, 0, char(2), char(3), '…', 16)
         FROM messages_fts JOIN messages m ON m.id = messages_fts.rowid
         WHERE messages_fts MATCH ?1 AND (?2 IS NULL OR m.conv_id = ?2) AND {} AND {}
         ORDER BY m.date_ms DESC LIMIT ?3",
        view.filter("m"),
        view.dedupe("m"),
    ))?;
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
            && let Some(c) = conversation(conn, view, names, conv_id)?
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
    pub device_name: Option<String>,
    pub snapshot_id: String,
    pub backup_ms: Option<i64>,
    pub ingested_ms: i64,
    pub messages: i64,
}

/// Snapshots of the devices the view can see, with totals.
pub fn status(conn: &Connection, view: &View, names: &Names) -> Result<Status> {
    let mut stmt =
        conn.prepare(&format!(
        "SELECT device, device_name, snapshot_id, backup_ms, ingested_ms, messages FROM snapshots s
         WHERE {} OR s.device IN (SELECT DISTINCT a.device FROM conv_stats a WHERE {})
         ORDER BY backup_ms DESC",
        if view.all { "1".to_owned() } else { "s.device IN (SELECT d FROM temp.v_dev)".to_owned() },
        view.filter("a"),
    ))?;
    let snapshots = stmt
        .query_map([], |r| {
            Ok(SnapshotStatus {
                device: r.get(0)?,
                device_name: r.get(1)?,
                snapshot_id: r.get(2)?,
                backup_ms: r.get(3)?,
                ingested_ms: r.get(4)?,
                messages: r.get(5)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let count = |sql: String| conn.query_row(&sql, [], |r| r.get::<_, i64>(0));
    Ok(Status {
        snapshots,
        conversations: count(format!(
            "SELECT count(DISTINCT a.conv_id) FROM conv_stats a WHERE {}",
            view.filter("a")
        ))?,
        messages: count(format!(
            "SELECT count(*) FROM messages m WHERE m.kind = 'message' AND {} AND {}",
            view.filter("m"),
            view.dedupe("m")
        ))?,
        contacts: names.0.len() as i64,
    })
}

/// Where an attachment lives, restricted to copies referenced by messages the
/// view can see: `(device, mime, name)`.
pub fn attachment(
    conn: &Connection,
    view: &View,
    file_id: &str,
) -> Result<Vec<AttachmentLocation>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT DISTINCT a.device, a.mime, a.name FROM message_files f
         JOIN messages m ON m.id = f.message_id
         JOIN attachments a ON a.file_id = f.file_id AND a.device = m.device
         WHERE f.file_id = ?1 AND {}",
        view.filter("m")
    ))?;
    Ok(stmt
        .query_map([file_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?)
}

/// A device and the iMessage IDs / numbers its messages used: what an admin
/// needs to write the accounts file.
#[derive(Serialize, Debug)]
pub struct Identity {
    pub device: String,
    pub device_name: Option<String>,
    pub last_backup_ms: Option<i64>,
    /// `(normalized iMessage ID, message count)`; an empty ID means unknown.
    pub imessage_ids: Vec<(String, i64)>,
}

pub fn identities(conn: &Connection) -> Result<Vec<Identity>> {
    let mut stmt = conn.prepare(
        "SELECT device, (SELECT device_name FROM snapshots s2 WHERE s2.device = s.device AND device_name IS NOT NULL
                          ORDER BY backup_ms DESC LIMIT 1), max(backup_ms)
         FROM snapshots s GROUP BY device ORDER BY max(backup_ms) DESC",
    )?;
    let devices: Vec<(String, Option<String>, Option<i64>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let mut ids = conn.prepare(
        "SELECT owner, sum(msg_count) FROM conv_stats WHERE device = ?1 GROUP BY owner ORDER BY sum(msg_count) DESC",
    )?;
    devices
        .into_iter()
        .map(|(device, device_name, last_backup_ms)| {
            let imessage_ids = ids
                .query_map([&device], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            Ok(Identity {
                device,
                device_name,
                last_backup_ms,
                imessage_ids,
            })
        })
        .collect()
}
