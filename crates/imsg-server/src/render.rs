//! Turn imessage-database `Message`s into the compact JSON body stored in the
//! index and rendered by the web UI.

use std::collections::HashMap;

use imessage_database::message_types::edited::EditStatus;
use imessage_database::message_types::expressives::Expressive;
use imessage_database::message_types::text_effects::{style::Style, text_effect::TextEffect};
use imessage_database::message_types::url::URLMessage;
use imessage_database::message_types::variants::{
    Announcement, CustomBalloon, Tapback, TapbackAction, URLOverride, Variant,
};
use imessage_database::rusqlite::Connection;
use imessage_database::tables::attachment::Attachment;
use imessage_database::tables::messages::Message;
use imessage_database::tables::messages::models::{BubbleComponent, GroupAction};
use imessage_database::util::dates::to_utc;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Span {
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub b: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub i: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub u: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub s: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mention: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EditEvent {
    pub ms: i64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Part {
    Text {
        spans: Vec<Span>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        history: Vec<EditEvent>,
    },
    Attachment {
        file: Option<String>,
        mime: Option<String>,
        name: Option<String>,
        size: i64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        sticker: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transcription: Option<String>,
    },
    App {
        app: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site: Option<String>,
    },
    Unsent,
    Announcement {
        action: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Body {
    pub parts: Vec<Part>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

/// Everything the index stores about one message.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub kind: &'static str,
    pub body: Body,
    /// Plain text for previews and full-text search.
    pub text: String,
    pub assoc_guid: Option<String>,
    pub assoc_part: Option<i64>,
    pub reaction: Option<String>,
    pub reaction_removed: bool,
    pub expressive: Option<String>,
    pub date_ms: i64,
    pub read_ms: Option<i64>,
    pub delivered_ms: Option<i64>,
    pub edited_ms: Option<i64>,
    /// `(file_id, mime, name, size)` for every attachment with a known file.
    pub files: Vec<(String, Option<String>, Option<String>, i64)>,
}

pub fn apple_ms(stamp: i64) -> Option<i64> {
    if stamp == 0 {
        return None;
    }
    let secs = to_utc(stamp).ok()?.timestamp();
    // Keep sub-second precision for nanosecond stamps.
    let ms = if stamp >= 1_000_000_000_000 {
        (stamp / 1_000_000) % 1000
    } else {
        0
    };
    Some(secs * 1000 + ms)
}

fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '\u{FFFC}' && *c != '\u{FFFD}')
        .collect()
}

fn span_for(text: &str, effects: &[TextEffect]) -> Span {
    let mut span = Span {
        text: clean(text),
        ..Default::default()
    };
    for e in effects {
        match e {
            TextEffect::Link(url) => span.link = Some(url.clone()),
            TextEffect::Mention(who) => span.mention = Some(who.clone()),
            TextEffect::Styles(styles) => {
                for s in styles {
                    match s {
                        Style::Bold => span.b = true,
                        Style::Italic => span.i = true,
                        Style::Underline => span.u = true,
                        Style::Strikethrough => span.s = true,
                    }
                }
            }
            _ => {}
        }
    }
    span
}

fn tapback_symbol(t: &Tapback) -> String {
    match t {
        Tapback::Loved => "❤️".into(),
        Tapback::Liked => "👍".into(),
        Tapback::Disliked => "👎".into(),
        Tapback::Laughed => "😂".into(),
        Tapback::Emphasized => "‼️".into(),
        Tapback::Questioned => "❓".into(),
        Tapback::Emoji(e) => e.unwrap_or("❔").to_owned(),
        Tapback::Sticker => "sticker".into(),
    }
}

fn balloon_name(b: &CustomBalloon) -> String {
    match b {
        CustomBalloon::Application(id) => (*id).to_owned(),
        CustomBalloon::URL => "Link".into(),
        CustomBalloon::Handwriting => "Handwritten message".into(),
        CustomBalloon::DigitalTouch => "Digital Touch".into(),
        CustomBalloon::ApplePay => "Apple Cash".into(),
        CustomBalloon::Fitness => "Fitness".into(),
        CustomBalloon::Slideshow => "Photos".into(),
        CustomBalloon::CheckIn => "Check In".into(),
        CustomBalloon::FindMy => "Find My".into(),
        CustomBalloon::Polls => "Poll".into(),
        CustomBalloon::Business => "Business Chat".into(),
    }
}

fn app_part(db: &Connection, m: &Message, balloon: &CustomBalloon) -> Part {
    let mut part = Part::App {
        app: balloon_name(balloon),
        title: None,
        summary: None,
        url: None,
        site: None,
    };
    if let (CustomBalloon::URL, Some(payload)) = (balloon, m.payload_data(db)) {
        let (app, title, summary, url, site) = match URLMessage::get_url_message_override(&payload)
        {
            Ok(URLOverride::Normal(u)) => (
                "Link",
                u.title.map(str::to_owned),
                u.summary.map(str::to_owned),
                u.url.or(u.original_url).map(str::to_owned),
                u.site_name.map(str::to_owned),
            ),
            Ok(URLOverride::AppleMusic(_)) => ("Apple Music", None, None, None, None),
            Ok(URLOverride::AppStore(_)) => ("App Store", None, None, None, None),
            Ok(URLOverride::Collaboration(_)) => ("Collaboration", None, None, None, None),
            Ok(URLOverride::SharedPlacemark(_)) => ("Location", None, None, None, None),
            Err(_) => ("Link", None, None, None, None),
        };
        let url = url.or_else(|| {
            m.text
                .as_deref()
                .map(clean)
                .filter(|t| t.starts_with("http"))
        });
        part = Part::App {
            app: app.into(),
            title,
            summary,
            url,
            site,
        };
    }
    part
}

fn attachment_part(a: &Attachment, transcription: Option<String>) -> Part {
    Part::Attachment {
        file: a
            .filename
            .as_deref()
            .and_then(imsg_core::backup::attachment_file_id),
        mime: a
            .mime_type
            .clone()
            .or_else(|| a.uti.clone().map(|u| format!("uti:{u}"))),
        name: a
            .transfer_name
            .clone()
            .or_else(|| a.filename().map(str::to_owned)),
        size: a.total_bytes,
        sticker: a.is_sticker,
        transcription,
    }
}

fn announcement(m: &Message, handles: &HashMap<i32, String>) -> Option<Part> {
    let handle = |h: &i32| handles.get(h).cloned();
    let (action, target, name) = match m.get_announcement()? {
        Announcement::FullyUnsent => ("unsent", None, None),
        Announcement::AudioMessageKept => ("kept_audio", None, None),
        Announcement::GroupAction(a) => match a {
            GroupAction::ParticipantAdded(h) => ("added", handle(&h), None),
            GroupAction::ParticipantRemoved(h) => ("removed", handle(&h), None),
            GroupAction::NameChange(n) => ("renamed", None, Some(n.to_owned())),
            GroupAction::ParticipantLeft => ("left", None, None),
            GroupAction::GroupIconChanged => ("icon_changed", None, None),
            GroupAction::GroupIconRemoved => ("icon_removed", None, None),
            GroupAction::ChatBackgroundChanged => ("background_changed", None, None),
            GroupAction::ChatBackgroundRemoved => ("background_removed", None, None),
            GroupAction::PhoneNumberChanged(h) => ("number_changed", handle(&h), None),
        },
    };
    Some(Part::Announcement {
        action: action.into(),
        target,
        name,
    })
}

/// Render one message. `handles` maps this snapshot's handle ROWIDs to IDs.
pub fn render(
    db: &Connection,
    caps: &imessage_database::tables::capabilities::Capabilities,
    m: &Message,
    handles: &HashMap<i32, String>,
) -> Rendered {
    let mut out = Rendered {
        kind: "message",
        body: Body {
            parts: vec![],
            subject: m.subject.clone().filter(|s| !s.is_empty()),
        },
        text: String::new(),
        assoc_guid: None,
        assoc_part: None,
        reaction: None,
        reaction_removed: false,
        expressive: match m.get_expressive() {
            Expressive::None => None,
            e => Some(e.to_string()),
        },
        date_ms: apple_ms(m.date).unwrap_or(0),
        read_ms: apple_ms(m.date_read),
        delivered_ms: apple_ms(m.date_delivered),
        edited_ms: apple_ms(m.date_edited),
        files: vec![],
    };

    let attachments = Attachment::from_message(db, m, caps).unwrap_or_default();
    for a in &attachments {
        if let Some(id) = a
            .filename
            .as_deref()
            .and_then(imsg_core::backup::attachment_file_id)
        {
            out.files.push((
                id,
                a.mime_type.clone(),
                a.transfer_name.clone(),
                a.total_bytes,
            ));
        }
    }

    if let Some(part) = announcement(m, handles) {
        out.kind = "announcement";
        out.body.parts.push(part);
        return out;
    }

    match m.variant() {
        Variant::Tapback(_, action, tapback) => {
            out.kind = "reaction";
            if let Some((idx, guid)) = m.clean_associated_guid() {
                out.assoc_guid = Some(guid.to_owned());
                out.assoc_part = Some(idx as i64);
            }
            out.reaction = Some(tapback_symbol(&tapback));
            out.reaction_removed = action == TapbackAction::Removed;
            out.body
                .parts
                .extend(attachments.iter().map(|a| attachment_part(a, None)));
            return out;
        }
        Variant::Vote | Variant::PollUpdate => {
            out.kind = "hidden";
            return out;
        }
        _ => {}
    }

    let balloon = match m.variant() {
        Variant::App(b) => Some(b),
        _ => None,
    };
    let text = m.text.as_deref().unwrap_or_default();
    let mut used = vec![false; attachments.len()];
    let mut take_attachment = |guid: Option<&str>| -> Option<usize> {
        let by_guid = guid.and_then(|g| {
            attachments
                .iter()
                .enumerate()
                .position(|(i, a)| !used[i] && a.guid.as_deref() == Some(g))
        });
        let idx = by_guid.or_else(|| used.iter().position(|u| !u))?;
        used[idx] = true;
        Some(idx)
    };

    for (index, component) in m.components.iter().enumerate() {
        let status = m.edited_parts.as_ref().and_then(|e| e.part(index));
        match (component, status.map(|s| &s.status)) {
            (_, Some(EditStatus::Unsent)) | (BubbleComponent::Retracted, _) => {
                out.body.parts.push(Part::Unsent)
            }
            (BubbleComponent::Run(ranges), Some(EditStatus::Edited)) => {
                let history: Vec<EditEvent> = status
                    .map(|s| {
                        s.edit_history
                            .iter()
                            .map(|e| EditEvent {
                                ms: apple_ms(e.date).unwrap_or(0),
                                text: clean(&e.text),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let current = history.last().map(|e| e.text.clone()).unwrap_or_else(|| {
                    ranges
                        .iter()
                        .filter_map(|r| text.get(r.start..r.end))
                        .map(clean)
                        .collect()
                });
                out.body.parts.push(Part::Text {
                    spans: vec![Span {
                        text: current,
                        ..Default::default()
                    }],
                    history,
                });
            }
            (BubbleComponent::Run(ranges), _) => {
                let mut spans = Vec::new();
                for r in ranges {
                    if r.is_attachment() {
                        if !spans.is_empty() {
                            out.body.parts.push(Part::Text {
                                spans: std::mem::take(&mut spans),
                                history: vec![],
                            });
                        }
                        let meta = r.attachment.as_ref();
                        if let Some(i) = take_attachment(meta.and_then(|m| m.guid.as_deref())) {
                            out.body.parts.push(attachment_part(
                                &attachments[i],
                                meta.and_then(|m| m.transcription.clone()),
                            ));
                        }
                    } else if let Some(slice) = text.get(r.start..r.end) {
                        let span = span_for(slice, &r.effects);
                        if !span.text.is_empty() {
                            spans.push(span);
                        }
                    }
                }
                if !spans.is_empty() {
                    out.body.parts.push(Part::Text {
                        spans,
                        history: vec![],
                    });
                }
            }
            (BubbleComponent::App, _) => {
                if let Some(b) = &balloon {
                    out.body.parts.push(app_part(db, m, b));
                }
            }
        }
    }
    if m.components.is_empty() && !text.is_empty() {
        out.body.parts.push(Part::Text {
            spans: vec![span_for(text, &[])],
            history: vec![],
        });
    }
    if let Some(b) = &balloon
        && !out.body.parts.iter().any(|p| matches!(p, Part::App { .. }))
    {
        out.body.parts.push(app_part(db, m, b));
    }
    // Attachments the body did not reference (e.g. stickers) go at the end.
    for (i, a) in attachments.iter().enumerate() {
        if !used[i] {
            out.body.parts.push(attachment_part(a, None));
        }
    }

    let mut plain: Vec<String> = Vec::new();
    if let Some(s) = &out.body.subject {
        plain.push(s.clone());
    }
    for p in &out.body.parts {
        match p {
            Part::Text { spans, .. } => plain.push(spans.iter().map(|s| s.text.as_str()).collect()),
            Part::App {
                app, title, url, ..
            } => plain.push(
                title
                    .clone()
                    .or_else(|| url.clone())
                    .unwrap_or_else(|| app.clone()),
            ),
            _ => {}
        }
    }
    out.text = plain.join(" ").trim().to_owned();
    out
}
