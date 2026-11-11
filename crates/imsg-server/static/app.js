"use strict";
// Messages Archive web UI. All message content is inserted as text nodes
// (never as HTML) so archived content cannot inject markup or script.

const $ = (sel) => document.querySelector(sel);
const state = {
  me: null,
  viewAs: null, // admins: another account's name, or null for their own
  conversations: [],
  current: null,
  oldestId: null,
  newestId: null,
  hasBefore: false,
  hasAfter: false,
  loading: false,
  names: {},
  lastRendered: null, // {date, sender, kind} of the last appended message
  firstRendered: null,
};

function el(tag, attrs = {}, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k === "class") e.className = v;
    else if (k === "text") e.textContent = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children.flat()) {
    if (c == null || c === false) continue;
    e.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return e;
}

// Admins looking at another account pass ?as=<name> on every data request.
function withView(url) {
  if (!state.viewAs) return url;
  return `${url}${url.includes("?") ? "&" : "?"}as=${encodeURIComponent(state.viewAs)}`;
}

async function api(path, opts) {
  const r = await fetch(path === "/api/me" ? path : withView(path), { credentials: "same-origin", ...opts });
  if (r.status === 401) {
    location.href = "/login";
    throw new Error("signed out");
  }
  if (!r.ok) throw new Error(`${r.status} ${await r.text()}`);
  return r.json();
}

// ---- Formatting ---------------------------------------------------------
const dayFmt = new Intl.DateTimeFormat(undefined, { weekday: "long", year: "numeric", month: "long", day: "numeric" });
const timeFmt = new Intl.DateTimeFormat(undefined, { hour: "numeric", minute: "2-digit" });
const fullFmt = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });
const shortDate = new Intl.DateTimeFormat(undefined, { month: "numeric", day: "numeric", year: "2-digit" });

function sameDay(a, b) {
  const x = new Date(a), y = new Date(b);
  return x.getFullYear() === y.getFullYear() && x.getMonth() === y.getMonth() && x.getDate() === y.getDate();
}
function relTime(ms) {
  if (!ms) return "";
  const now = Date.now();
  if (sameDay(ms, now)) return timeFmt.format(ms);
  if (now - ms < 6 * 86400e3) return new Intl.DateTimeFormat(undefined, { weekday: "short" }).format(ms);
  return shortDate.format(ms);
}
function size(n) {
  if (!n) return "";
  const u = ["B", "KB", "MB", "GB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i ? 1 : 0)} ${u[i]}`;
}
function initials(name) {
  const letters = name.replace(/[^\p{L}\p{N} ]/gu, "").trim().split(/\s+/).map((w) => w[0]).filter(Boolean);
  return (letters.slice(0, 2).join("") || "#").toUpperCase();
}
function avatarColor(s) {
  let h = 0;
  for (const c of s) h = (h * 31 + c.codePointAt(0)) % 360;
  return `linear-gradient(hsl(${h} 45% 62%), hsl(${h} 45% 48%))`;
}
function safeUrl(u) {
  try {
    const url = new URL(u);
    return ["http:", "https:", "mailto:", "tel:", "sms:", "facetime:"].includes(url.protocol) ? url.href : null;
  } catch { return null; }
}
// Highlight markers from the server (U+0002 … U+0003) as <mark>.
function highlighted(text) {
  const out = [];
  const re = /\u0002([^\u0003]*)\u0003/g;
  let last = 0, m;
  while ((m = re.exec(text))) {
    out.push(text.slice(last, m.index), el("mark", { text: m[1] }));
    last = re.lastIndex;
  }
  out.push(text.slice(last));
  return out;
}

// ---- Conversation list ---------------------------------------------------
function convTitle(c) { return c.title || "Unknown"; }

function renderConversations(filter = "") {
  const list = $("#conv-list");
  list.replaceChildren();
  const f = filter.trim().toLowerCase();
  const items = state.conversations.filter((c) =>
    !f || convTitle(c).toLowerCase().includes(f) ||
    c.participants.some((p) => p.handle.toLowerCase().includes(f) || p.name.toLowerCase().includes(f)));
  list.classList.toggle("filtered", !!f);
  if (f && items.length) list.append(el("div", { class: "section-label", text: "Conversations" }));
  for (const c of items) {
    const title = convTitle(c);
    const preview = (c.is_group && c.last_sender && !c.last_from_me ? `${c.last_sender.split(" ")[0]}: ` : "") +
      (c.last_preview || "");
    const av = el("div", { class: "avatar" + (c.is_group ? " group" : ""), text: c.is_group ? `${c.participants.length}` : initials(title) });
    av.style.background = avatarColor(title);
    list.append(el("a", {
      class: "conv" + (state.current && state.current.id === c.id ? " active" : ""),
      href: `#/c/${c.id}`,
      "data-id": c.id,
    }, av, el("div", { class: "conv-main" },
      el("div", { class: "conv-top" }, el("span", { class: "conv-name", text: title }), el("span", { class: "conv-time", text: relTime(c.last_ms) })),
      el("div", { class: "conv-preview", text: preview }))));
  }
  if (!items.length && !f) list.append(el("div", { class: "empty", text: state.conversations.length ? "No matches" : "No conversations yet — run imsg-extract and wait for the next scan." }));
}

async function loadConversations() {
  state.conversations = await api("/api/conversations");
  renderConversations($("#search").value);
}

// ---- Search ---------------------------------------------------------------
let searchTimer = null;
function onSearchInput() {
  const q = $("#search").value;
  renderConversations(q);
  clearTimeout(searchTimer);
  const box = $("#search-results");
  if (q.trim().length < 2) { box.hidden = true; box.replaceChildren(); return; }
  searchTimer = setTimeout(async () => {
    const hits = await api(`/api/search?q=${encodeURIComponent(q)}&limit=100`);
    if ($("#search").value !== q) return;
    box.replaceChildren(el("div", { class: "section-label", text: `Messages (${hits.length}${hits.length === 100 ? "+" : ""})` }));
    for (const h of hits) {
      box.append(el("div", { class: "hit", onclick: () => { location.hash = `#/c/${h.conv_id}/m/${h.id}`; } },
        el("div", { class: "hit-head" }, el("b", { text: h.conv_title }), el("span", { text: relTime(h.date_ms) })),
        el("div", { class: "hit-snippet" }, `${h.from_me ? "You" : (h.sender_name || "")}: `, ...highlighted(h.snippet))));
    }
    box.hidden = false;
  }, 250);
}

// ---- Message rendering ---------------------------------------------------
function textBubble(part) {
  const b = el("div", { class: "bubble" });
  for (const s of part.spans) {
    let node = document.createTextNode(s.text);
    const href = s.link && safeUrl(s.link);
    if (href) node = el("a", { href, target: "_blank", rel: "noopener noreferrer" }, node);
    if (s.mention) node = el("span", { class: "mention" }, node);
    for (const [flag, tag] of [["b", "b"], ["i", "i"], ["u", "u"], ["s", "s"]]) if (s[flag]) node = el(tag, {}, node);
    b.append(node);
  }
  const all = part.spans.map((s) => s.text).join("");
  if (/^\p{Extended_Pictographic}(‍?\p{Extended_Pictographic}|️|\p{Emoji_Modifier})*\s*$/u.test(all) && [...all].length <= 8) {
    b.classList.add("emoji-only");
  }
  return b;
}

function attachmentNode(p) {
  const mime = (p.mime || "").toLowerCase();
  const name = p.name || "attachment";
  const missing = () => el("span", { class: "file missing", text: `📎 ${name} (not in archive)` });
  if (!p.file) return missing();
  const src = withView(`/media/${p.file}`);
  if (mime.startsWith("image/") || /\.(heic|jpe?g|png|gif|webp)$/i.test(name)) {
    const img = el("img", { src, alt: name, loading: "lazy", onclick: () => window.open(src, "_blank", "noopener") });
    img.addEventListener("error", () => img.parentNode && img.parentNode.replaceWith(el("a", { class: "file", href: `${src}${src.includes("?") ? "&" : "?"}dl=1`, text: `🖼 ${name} ${size(p.size)}` })));
    return el("div", { class: "media" + (p.sticker ? " sticker" : "") }, img);
  }
  if (mime.startsWith("video/")) {
    return el("div", { class: "media" }, el("video", { src, controls: true, preload: "metadata", playsinline: true }));
  }
  if (mime.startsWith("audio/")) {
    return el("div", { class: "media" }, el("audio", { src, controls: true, preload: "none" }),
      p.transcription ? el("div", { class: "meta", text: `“${p.transcription}”` }) : null);
  }
  return el("a", { class: "file", href: `${src}${src.includes("?") ? "&" : "?"}dl=1`, text: `📎 ${name} ${size(p.size)}` });
}

function appNode(p) {
  const href = p.url && safeUrl(p.url);
  const card = el(href ? "a" : "div", { class: "link-card", href, target: href ? "_blank" : null, rel: "noopener noreferrer" },
    el("div", { class: "site", text: p.site || p.app }),
    p.title ? el("div", { class: "title", text: p.title }) : null,
    p.summary ? el("div", { class: "summary", text: p.summary }) : null,
    !p.title && p.url ? el("div", { class: "summary", text: p.url }) : null);
  return card;
}

function announcementText(m, p) {
  const who = m.from_me ? "You" : (m.sender_name || "Someone");
  const target = p.target ? (state.names[p.target] || p.target) : "someone";
  switch (p.action) {
    case "unsent": return `${who} unsent a message`;
    case "added": return `${who} added ${target} to the conversation`;
    case "removed": return `${who} removed ${target} from the conversation`;
    case "renamed": return `${who} named the conversation “${p.name}”`;
    case "left": return `${who} left the conversation`;
    case "icon_changed": return `${who} changed the group photo`;
    case "icon_removed": return `${who} removed the group photo`;
    case "background_changed": return `${who} changed the background`;
    case "background_removed": return `${who} removed the background`;
    case "number_changed": return `${target} changed their phone number`;
    case "kept_audio": return `${who} kept an audio message`;
    default: return `${who}: ${p.action}`;
  }
}

function renderMessage(m, prev) {
  const frag = document.createDocumentFragment();
  if (!prev || !sameDay(prev.date_ms, m.date_ms)) {
    frag.append(el("div", { class: "day", text: dayFmt.format(m.date_ms) }));
  }
  if (m.kind === "announcement") {
    for (const p of m.body.parts || []) frag.append(el("div", { class: "announce", "data-id": m.id, text: announcementText(m, p) }));
    return frag;
  }
  const isGroup = state.current && state.current.is_group;
  const newSpeaker = !prev || prev.kind !== "message" || prev.from_me !== m.from_me || prev.sender !== m.sender ||
    m.date_ms - prev.date_ms > 30 * 60e3;
  const sms = (m.service || "").toUpperCase() === "SMS" || (m.service || "").toUpperCase() === "RCS";
  const row = el("div", { class: `row ${m.from_me ? "me" : "them"}${sms ? " sms" : ""}${newSpeaker ? " gap" : ""}`, "data-id": m.id, id: `m${m.id}` });
  if (isGroup && !m.from_me && newSpeaker) row.append(el("div", { class: "sender", text: m.sender_name || m.sender || "" }));
  if (m.reply_to) {
    row.append(el("div", { class: "reply-quote", title: "Show original", onclick: () => jumpTo(m.reply_to.id) },
      `↪ ${m.reply_to.name}: ${m.reply_to.text || "…"}`));
  }
  const wrap = el("div", { class: "bubble-wrap", title: fullFmt.format(m.date_ms) });
  const body = m.body || {};
  if (body.subject) wrap.append(el("div", { class: "bubble" }, el("b", { text: body.subject })));
  let history = [];
  for (const p of body.parts || []) {
    if (p.t === "text") { wrap.append(textBubble(p)); if (p.history) history = history.concat(p.history); }
    else if (p.t === "attachment") wrap.append(attachmentNode(p));
    else if (p.t === "app") wrap.append(appNode(p));
    else if (p.t === "unsent") wrap.append(el("div", { class: "bubble unsent", text: "Message unsent" }));
  }
  if (!wrap.childNodes.length) wrap.append(el("div", { class: "bubble unsent", text: "(empty message)" }));
  row.append(wrap);
  if (m.reactions && m.reactions.length) {
    const r = el("div", { class: "reactions" });
    for (const x of m.reactions) {
      r.append(el("span", { class: "reaction", title: x.name, text: x.emoji === "sticker" ? "🏷" : x.emoji }));
    }
    row.append(r);
  }
  const meta = [];
  if (newSpeaker) meta.push(timeFmt.format(m.date_ms));
  if (m.expressive) meta.push(m.expressive);
  if (m.edited_ms) {
    const tip = history.length ? history.map((h) => `${fullFmt.format(h.ms)}: ${h.text}`).join("\n") : `Edited ${fullFmt.format(m.edited_ms)}`;
    meta.push(el("span", { class: "edited", title: tip, text: "Edited" }));
  }
  if (m.reply_count) meta.push(`${m.reply_count} ${m.reply_count === 1 ? "reply" : "replies"}`);
  if (meta.length) {
    const md = el("div", { class: "meta" });
    meta.forEach((x, i) => { if (i) md.append(" · "); md.append(x); });
    row.append(md);
  }
  frag.append(row);
  return frag;
}

function appendMessages(msgs) {
  const box = $("#messages");
  for (const m of msgs) {
    box.append(renderMessage(m, state.lastRendered));
    state.lastRendered = m;
    if (!state.firstRendered) state.firstRendered = m;
  }
}

function prependMessages(msgs) {
  const box = $("#messages");
  const frag = document.createDocumentFragment();
  let prev = null;
  for (const m of msgs) { frag.append(renderMessage(m, prev)); prev = m; }
  // Re-render the old first message's day header only if it now shares a day.
  const oldFirst = box.querySelector(".day");
  if (prev && state.firstRendered && oldFirst && sameDay(prev.date_ms, state.firstRendered.date_ms)) oldFirst.remove();
  const before = box.scrollHeight;
  const top = box.scrollTop;
  box.prepend(frag);
  box.scrollTop = top + (box.scrollHeight - before);
  if (msgs.length) state.firstRendered = msgs[0];
}

function mergeNames(n) { Object.assign(state.names, n || {}); }

// ---- Thread loading -----------------------------------------------------
async function openConversation(id, focusId) {
  const conv = state.conversations.find((c) => c.id === id) || await api(`/api/conversations/${id}`);
  state.current = conv;
  state.lastRendered = state.firstRendered = null;
  document.getElementById("app").classList.add("open");
  $("#thread-head").hidden = false;
  $("#thread-name").textContent = convTitle(conv);
  $("#thread-sub").textContent = [conv.participants.map((p) => p.name === p.handle ? p.handle : `${p.name}`).join(", "),
    `${conv.msg_count.toLocaleString()} messages`, conv.services.join("/")].filter(Boolean).join(" · ");
  document.title = `${convTitle(conv)} · Messages Archive`;
  renderConversations($("#search").value);
  const box = $("#messages");
  box.replaceChildren(el("div", { class: "loader", text: "Loading…" }));
  const page = await api(`/api/conversations/${id}/messages?limit=80${focusId ? `&around=${focusId}` : ""}`);
  if (!state.current || state.current.id !== id) return;
  mergeNames(page.names);
  box.replaceChildren();
  state.hasBefore = page.has_more_before;
  state.hasAfter = page.has_more_after;
  state.oldestId = page.messages.length ? page.messages[0].id : null;
  state.newestId = page.messages.length ? page.messages[page.messages.length - 1].id : null;
  if (!page.messages.length) box.append(el("div", { class: "empty", text: "No messages" }));
  appendMessages(page.messages);
  if (focusId) {
    const target = document.getElementById(`m${focusId}`);
    if (target) { target.classList.add("highlight"); target.scrollIntoView({ block: "center" }); }
  } else {
    box.scrollTop = box.scrollHeight;
    // Images load later and grow the list; keep pinned to the bottom meanwhile.
    box.querySelectorAll("img").forEach((img) => img.addEventListener("load", () => {
      if (!state.userScrolled) box.scrollTop = box.scrollHeight;
    }, { once: true }));
  }
  state.userScrolled = false;
}

async function loadOlder() {
  if (state.loading || !state.hasBefore || !state.current) return;
  state.loading = true;
  try {
    const id = state.current.id;
    const page = await api(`/api/conversations/${id}/messages?limit=80&before=${state.oldestId}`);
    if (!state.current || state.current.id !== id) return;
    mergeNames(page.names);
    state.hasBefore = page.has_more_before;
    if (page.messages.length) state.oldestId = page.messages[0].id;
    prependMessages(page.messages);
  } finally { state.loading = false; }
}

async function loadNewer() {
  if (state.loading || !state.hasAfter || !state.current) return;
  state.loading = true;
  try {
    const id = state.current.id;
    const page = await api(`/api/conversations/${id}/messages?limit=80&after=${state.newestId}`);
    if (!state.current || state.current.id !== id) return;
    mergeNames(page.names);
    state.hasAfter = page.has_more_after;
    if (page.messages.length) state.newestId = page.messages[page.messages.length - 1].id;
    appendMessages(page.messages);
  } finally { state.loading = false; }
}

function jumpTo(messageId) {
  const existing = document.getElementById(`m${messageId}`);
  if (existing) {
    document.querySelectorAll(".row.highlight").forEach((r) => r.classList.remove("highlight"));
    existing.classList.add("highlight");
    existing.scrollIntoView({ block: "center", behavior: "smooth" });
  } else if (state.current) {
    location.hash = `#/c/${state.current.id}/m/${messageId}`;
  }
}

// ---- Status --------------------------------------------------------------
async function showStatus() {
  const s = await api("/api/status");
  const body = $("#status-body");
  const rows = s.snapshots.map((x) => el("tr", {},
    el("td", { text: x.device_name ? `${x.device_name} (${x.device})` : x.device }), el("td", { text: x.snapshot_id }),
    el("td", { text: x.backup_ms ? fullFmt.format(x.backup_ms) : "" }), el("td", { text: x.messages.toLocaleString() })));
  const who = state.me ? `Signed in as ${state.me.name}${state.viewAs ? ` · viewing ${state.viewAs}` : ""} · ` +
    `two-factor ${state.me.totp ? "on" : "off"}` : "";
  let identities = null;
  if (state.me && state.me.admin) {
    const ids = await api("/api/identities");
    identities = [el("h3", { text: "Devices & iMessage IDs (for the accounts file)" }),
      el("table", {}, el("thead", {}, el("tr", {}, el("th", { text: "Device UDID" }), el("th", { text: "Name" }), el("th", { text: "iMessage IDs used" }))),
        el("tbody", {}, ids.map((d) => el("tr", {},
          el("td", {}, el("code", { text: d.device })), el("td", { text: d.device_name || "" }),
          el("td", { text: d.imessage_ids.map(([id, n]) => `${id || "(unknown)"} (${n})`).join(", ") })))))];
  }
  body.replaceChildren(
    el("p", { text: who }),
    el("p", { text: `${s.messages.toLocaleString()} messages in ${s.conversations.toLocaleString()} conversations · ${s.contacts.toLocaleString()} contact handles` }),
    el("p", { text: `Transcoding: ${s.transcoding}` }),
    s.last_scan ? el("p", { text: `Last scan ${fullFmt.format(new Date(s.last_scan.at))}: ${s.last_scan.report.snapshots} new snapshot(s), ${s.last_scan.report.waiting} waiting for files, ${s.last_scan.report.failed} failed` }) : null,
    el("h3", { text: "Snapshots" }),
    el("table", {}, el("thead", {}, el("tr", {}, el("th", { text: "Device" }), el("th", { text: "Snapshot" }), el("th", { text: "Backup" }), el("th", { text: "Messages" }))), el("tbody", {}, rows)),
    ...(identities || []));
  const d = $("#status-dialog");
  if (!d.open) d.showModal();
}

// ---- Routing / wiring ----------------------------------------------------
function route() {
  const m = location.hash.match(/^#\/c\/(\d+)(?:\/m\/(\d+))?/);
  if (m) {
    openConversation(Number(m[1]), m[2] ? Number(m[2]) : null).catch((e) => console.error(e));
  } else {
    state.current = null;
    document.getElementById("app").classList.remove("open");
    $("#thread-head").hidden = true;
    $("#messages").replaceChildren(el("div", { class: "empty", text: "Select a conversation" }));
    renderConversations($("#search").value);
  }
}

$("#search").addEventListener("input", onSearchInput);
$("#back-btn").addEventListener("click", () => { location.hash = ""; });
$("#status-btn").addEventListener("click", () => showStatus().catch(console.error));
$("#rescan-btn").addEventListener("click", async () => {
  const btn = $("#rescan-btn");
  btn.disabled = true;
  btn.textContent = "Scanning…";
  try { await api("/api/rescan", { method: "POST" }); await loadConversations(); await showStatus(); }
  finally { btn.disabled = false; btn.textContent = "Scan for new snapshots"; }
});
for (const ev of ["wheel", "touchstart", "keydown"]) {
  $("#messages").addEventListener(ev, () => { state.userScrolled = true; }, { passive: true });
}
$("#messages").addEventListener("scroll", () => {
  const box = $("#messages");
  if (box.scrollTop < 400) loadOlder();
  if (box.scrollHeight - box.scrollTop - box.clientHeight < 400) loadNewer();
});
window.addEventListener("hashchange", route);
async function loadMe() {
  state.me = await api("/api/me");
  if (!state.me.admin) return;
  let saved = null;
  try { saved = localStorage.getItem("imsg.viewAs"); } catch {}
  const names = state.me.accounts.map((a) => a.name);
  state.viewAs = saved && saved !== state.me.name && names.includes(saved) ? saved : null;
  const sel = $("#view-select");
  sel.replaceChildren(...names.map((n) => el("option", { value: n, text: n === state.me.name ? `${n} (you)` : n })));
  sel.value = state.viewAs || state.me.name;
  $("#view-wrap").hidden = names.length < 2;
  sel.addEventListener("change", async () => {
    state.viewAs = sel.value === state.me.name ? null : sel.value;
    try { localStorage.setItem("imsg.viewAs", sel.value); } catch {}
    state.names = {};
    if (location.hash) location.hash = ""; else route();
    await loadConversations();
  });
}

loadMe().then(loadConversations).then(route).catch((e) => console.error(e));
