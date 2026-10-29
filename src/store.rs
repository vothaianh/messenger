//! In-memory model of the inbox, built by applying Lightspeed procedure calls.
//!
//! Argument positions were mapped against live payloads from messenger web
//! (see README "Protocol notes"). Unknown procedures are ignored.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::lightspeed::Call;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Contact {
    pub id: i64,
    pub name: String,
    pub first_name: String,
    pub picture: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Thread {
    pub key: i64,
    pub name: String,
    /// The chat's quick emoji (sent by the button next to the composer);
    /// empty means the default 👍.
    pub emoji: String,
    pub snippet: String,
    pub picture: String,
    pub last_activity_ms: i64,
    pub last_read_ms: i64,
    pub thread_type: i64,
    pub participants: Vec<i64>,
    /// Oldest message we hold has more history behind it on the server.
    pub has_more_before: bool,
    pub muted: bool,
    /// End-to-end encrypted chat (sync group 95). Messages need the Signal
    /// transport, which this client doesn't implement yet.
    pub e2ee: bool,
}

impl Thread {
    pub fn unread(&self) -> bool {
        self.last_activity_ms > self.last_read_ms
    }
    pub fn is_group(&self) -> bool {
        // 1 = one-to-one; 2 = group; others are communities/marketplace/etc.
        // 15 = E2EE one-to-one.
        self.thread_type != 1 && self.thread_type != 15 && self.participants.len() > 2
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AttachmentKind {
    Image,
    Video,
    Audio,
    File,
    Link,
    Sticker,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub kind: AttachmentKind,
    pub preview_url: String,
    pub url: String,
    pub title: String,
    /// Shared posts: where it's from ("VTV24 · Facebook").
    pub subtitle: String,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Message {
    pub id: String,
    pub thread_key: i64,
    pub sender_id: i64,
    pub text: String,
    pub timestamp_ms: i64,
    pub otid: i64,
    pub is_admin: bool,
    pub unsent: bool,
    pub edited: bool,
    pub reply_to_id: Option<String>,
    pub reply_snippet: String,
    /// Who wrote the message this one replies to (known even when that
    /// message isn't loaded).
    pub reply_to_user: Option<i64>,
    pub sticker_id: Option<i64>,
    pub attachments: Vec<Attachment>,
    /// actor id -> emoji
    pub reactions: BTreeMap<i64, String>,
    /// Message sent locally but not yet acknowledged by the server.
    pub pending: bool,
    /// One of our messages that reached a recipient's device (E2EE receipts;
    /// normal chats use `delivered_receipts`).
    pub delivered: bool,
}

/// A decrypted, decoded E2EE message (1:1 chats).
pub struct E2eeIncoming {
    pub id: String,
    /// fbid of the sending user.
    pub sender: i64,
    pub from_me: bool,
    /// fbid of the other person in the chat.
    pub peer: i64,
    pub timestamp_ms: i64,
    pub decoded: crate::armadillo::Decoded,
}

#[derive(Default)]
pub struct Store {
    pub me: i64,
    pub contacts: HashMap<i64, Contact>,
    pub threads: HashMap<i64, Thread>,
    /// thread key -> messages sorted by timestamp
    pub messages: HashMap<i64, Vec<Message>>,
    /// thread key -> (contact id -> read watermark ms)
    pub read_receipts: HashMap<i64, HashMap<i64, i64>>,
    /// thread key -> (sender id -> when typing started)
    pub typing: HashMap<i64, HashMap<i64, Instant>>,
    /// database id -> sync cursor
    pub cursors: HashMap<i64, String>,
    /// Message ids that changed since the UI last looked (for notifications).
    pub new_incoming: Vec<(i64, String)>,
    /// Reactions for messages we don't have yet (a sync can list them before
    /// the message): message id -> (actor -> emoji, "" = removed).
    pending_reactions: HashMap<String, BTreeMap<i64, String>>,
    /// thread key -> (contact id -> delivered watermark ms)
    pub delivered_receipts: HashMap<i64, HashMap<i64, i64>>,
    /// Per-chat nicknames: thread -> (contact -> nickname).
    pub nicknames: HashMap<i64, HashMap<i64, String>>,
    pub initial_sync_done: bool,
    seen_ids: HashSet<String>,
}

/// What's kept on disk between launches so the app shows the inbox at once
/// (see `Store::save_cache`). Messages are capped per chat.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Snapshot {
    me: i64,
    contacts: Vec<Contact>,
    threads: Vec<Thread>,
    messages: Vec<Message>,
    read_receipts: HashMap<i64, HashMap<i64, i64>>,
    delivered_receipts: HashMap<i64, HashMap<i64, i64>>,
    nicknames: HashMap<i64, HashMap<i64, String>>,
}

const CACHED_MESSAGES_PER_CHAT: usize = 60;

fn cache_path() -> Option<std::path::PathBuf> {
    crate::e2ee_store::data_dir().ok().map(|d| d.join("cache.json"))
}

/// Remove the on-disk cache (on log out).
pub fn clear_cache() {
    if let Some(path) = cache_path() {
        let _ = std::fs::remove_file(path);
    }
}

impl Store {
    /// The inbox as last saved, or None (no cache, or unreadable).
    pub fn load_cache() -> Option<Store> {
        let bytes = std::fs::read(cache_path()?).ok()?;
        let snap: Snapshot = serde_json::from_slice(&bytes).ok()?;
        let mut store = Store { me: snap.me, ..Default::default() };
        store.contacts = snap.contacts.into_iter().map(|c| (c.id, c)).collect();
        store.threads = snap.threads.into_iter().map(|t| (t.key, t)).collect();
        for m in snap.messages {
            store.seen_ids.insert(m.id.clone());
            store.messages.entry(m.thread_key).or_default().push(m);
        }
        store.read_receipts = snap.read_receipts;
        store.delivered_receipts = snap.delivered_receipts;
        store.nicknames = snap.nicknames;
        Some(store)
    }

    /// Serialize what `load_cache` restores (the newest messages of each chat,
    /// no pending ones). Cheap enough to call under the store lock; the caller
    /// writes the bytes with `write_cache`, off the UI thread.
    pub fn cache_bytes(&self) -> Option<Vec<u8>> {
        if self.me == 0 || self.threads.is_empty() {
            return None;
        }
        let messages = self
            .messages
            .values()
            .flat_map(|list| {
                let kept: Vec<&Message> = list.iter().filter(|m| !m.pending).collect();
                let skip = kept.len().saturating_sub(CACHED_MESSAGES_PER_CHAT);
                kept.into_iter().skip(skip).cloned()
            })
            .collect();
        let snap = Snapshot {
            me: self.me,
            contacts: self.contacts.values().cloned().collect(),
            threads: self.threads.values().cloned().collect(),
            messages,
            read_receipts: self.read_receipts.clone(),
            delivered_receipts: self.delivered_receipts.clone(),
            nicknames: self.nicknames.clone(),
        };
        serde_json::to_vec(&snap).ok()
    }

    /// The name to show for `id` inside a chat: their nickname there, if set.
    pub fn member_name(&self, thread: i64, id: i64) -> String {
        if id != self.me {
            if let Some(nick) = self.nicknames.get(&thread).and_then(|n| n.get(&id)).filter(|n| !n.trim().is_empty()) {
                return nick.clone();
            }
        }
        self.contact_name(id)
    }

    pub fn contact_name(&self, id: i64) -> String {
        if id == self.me {
            return "You".into();
        }
        self.contacts
            .get(&id)
            .map(|c| c.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| id.to_string())
    }

    pub fn thread_title(&self, t: &Thread) -> String {
        if !t.name.is_empty() {
            return t.name.clone();
        }
        if t.thread_type == 1 {
            if let Some(c) = self.contacts.get(&t.key) {
                return c.name.clone();
            }
        }
        let names: Vec<_> = t
            .participants
            .iter()
            .filter(|p| **p != self.me)
            .take(4)
            .map(|p| {
                self.contacts
                    .get(p)
                    .map(|c| if c.first_name.is_empty() { c.name.clone() } else { c.first_name.clone() })
                    .unwrap_or_default()
            })
            .filter(|n| !n.is_empty())
            .collect();
        if names.is_empty() { t.key.to_string() } else { names.join(", ") }
    }

    pub fn thread_picture(&self, t: &Thread) -> String {
        if !t.picture.is_empty() {
            return t.picture.clone();
        }
        let other = if t.thread_type == 1 {
            Some(t.key)
        } else {
            t.participants.iter().copied().find(|p| *p != self.me)
        };
        other
            .and_then(|id| self.contacts.get(&id))
            .map(|c| c.picture.clone())
            .unwrap_or_default()
    }

    pub fn sorted_threads(&self) -> Vec<&Thread> {
        let mut v: Vec<_> = self.threads.values().collect();
        v.sort_by(|a, b| b.last_activity_ms.cmp(&a.last_activity_ms));
        v
    }

    pub fn typing_in(&self, thread: i64) -> Vec<i64> {
        self.typing
            .get(&thread)
            .map(|m| {
                m.iter()
                    .filter(|(_, t)| t.elapsed().as_secs() < 10)
                    .map(|(id, _)| *id)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn find_message_mut(&mut self, thread: Option<i64>, id: &str) -> Option<&mut Message> {
        if let Some(t) = thread {
            if let Some(pos) = self.messages.get(&t).and_then(|v| v.iter().position(|m| m.id == id)) {
                return self.messages.get_mut(&t).map(|v| &mut v[pos]);
            }
        }
        self.messages
            .values_mut()
            .flat_map(|v| v.iter_mut())
            .find(|m| m.id == id)
    }

    /// A delivery or read receipt for our E2EE messages: mark them delivered,
    /// and for reads move the reader's watermark so "seen" avatars show.
    pub fn apply_e2ee_receipt(&mut self, r: &crate::e2ee_listen::Receipt) {
        let me = self.me;
        for id in &r.ids {
            let Some(m) = self.find_message_mut(None, id) else { continue };
            if m.sender_id != me {
                continue;
            }
            m.delivered = true;
            let (thread, ts) = (m.thread_key, m.timestamp_ms);
            if r.kind == crate::e2ee_listen::ReceiptKind::Read {
                let e = self.read_receipts.entry(thread).or_default().entry(r.user).or_default();
                *e = (*e).max(ts);
            }
        }
    }

    /// Set (or, with an empty emoji, remove) `actor`'s reaction. Also used for
    /// our own reactions right away, before the server echoes them.
    pub fn set_reaction(&mut self, thread: i64, message_id: &str, actor: i64, emoji: String) {
        let thread = (thread != 0).then_some(thread);
        match self.find_message_mut(thread, message_id) {
            Some(m) if emoji.is_empty() => {
                m.reactions.remove(&actor);
            }
            Some(m) => {
                m.reactions.insert(actor, emoji);
            }
            None => {
                self.pending_reactions.entry(message_id.to_string()).or_default().insert(actor, emoji);
            }
        }
    }

    fn insert_message(&mut self, mut msg: Message) {
        if let Some(pending) = self.pending_reactions.remove(&msg.id) {
            for (actor, emoji) in pending {
                if emoji.is_empty() {
                    msg.reactions.remove(&actor);
                } else {
                    msg.reactions.insert(actor, emoji);
                }
            }
        }
        let list = self.messages.entry(msg.thread_key).or_default();
        // Replace an optimistic copy (same otid) or an earlier version (same id).
        if let Some(existing) = list
            .iter_mut()
            .find(|m| m.id == msg.id || (msg.otid != 0 && m.otid == msg.otid))
        {
            let attachments = std::mem::take(&mut existing.attachments);
            let reactions = std::mem::take(&mut existing.reactions);
            *existing = msg;
            if existing.attachments.is_empty() {
                existing.attachments = attachments;
            }
            if existing.reactions.is_empty() {
                existing.reactions = reactions;
            }
        } else {
            let pos = list.partition_point(|m| m.timestamp_ms <= msg.timestamp_ms);
            list.insert(pos, msg);
        }
    }

    /// Add a locally-sent message before the server echoes it back.
    pub fn add_pending(&mut self, thread_key: i64, otid: i64, text: String, reply_to: Option<String>) {
        let now = now_ms();
        self.insert_message(Message {
            id: format!("pending.{otid}"),
            thread_key,
            sender_id: self.me,
            text,
            timestamp_ms: now,
            otid,
            reply_to_id: reply_to,
            pending: true,
            ..Default::default()
        });
    }

    pub fn remove_pending(&mut self, thread_key: i64, otid: i64) {
        let id = format!("pending.{otid}");
        if let Some(list) = self.messages.get_mut(&thread_key) {
            list.retain(|m| m.id != id);
        }
    }

/// A decrypted E2EE message, ready to apply (see `client.rs`).
    pub fn apply_e2ee(&mut self, ev: &E2eeIncoming) -> bool {
        use crate::armadillo::{Content2, MediaKind};
        // 1:1 chats: the thread is the E2EE thread whose participants include the
        // other person (or, for older data, whose key is their id).
        let Some(thread_key) = self
            .threads
            .values()
            .find(|t| t.e2ee && (t.participants.contains(&ev.peer) || t.key == ev.peer))
            .map(|t| t.key)
        else {
            return false;
        };
        let snippet_prefix = if ev.from_me { "You: " } else { "" };
        // Our own messages (from here or another device) are read by definition.
        if ev.from_me {
            if let Some(t) = self.threads.get_mut(&thread_key) {
                t.last_read_ms = t.last_read_ms.max(ev.timestamp_ms);
            }
        }
        match &ev.decoded.content {
            Content2::Text { text } | Content2::Link { text, .. } => {
                self.add_e2ee_message(thread_key, ev, text.clone());
                self.touch_thread(thread_key, ev.timestamp_ms, format!("{snippet_prefix}{text}"));
            }
            Content2::Media { kind, caption, file_name, file } => {
                let label = match kind {
                    MediaKind::Image => "📷 Photo",
                    MediaKind::Video => "🎬 Video",
                    MediaKind::Audio => "🎤 Voice message",
                    MediaKind::Sticker => "Sticker",
                    MediaKind::File => "📄 File",
                };
                let mut text = label.to_string();
                if !file_name.is_empty() {
                    text.push_str(&format!(": {file_name}"));
                }
                if !caption.is_empty() {
                    text.push_str(&format!("\n{caption}"));
                }
                // A decrypted copy in the cache (downloaded by the E2EE listener)
                // shows as the real image/video; otherwise the label stays.
                let cached = file.as_ref().and_then(|f| crate::e2ee_media::cached_path(f).filter(|p| p.exists()).map(|p| (f, p)));
                let shown = match (kind, cached) {
                    (MediaKind::Image | MediaKind::Video | MediaKind::Sticker, Some((f, path))) => {
                        let uri = format!("file://{}", path.display());
                        self.add_e2ee_message(thread_key, ev, caption.clone());
                        self.attach(thread_key, &ev.id, Attachment {
                            kind: if *kind == MediaKind::Video { AttachmentKind::Video } else { AttachmentKind::Image },
                            preview_url: uri.clone(),
                            url: uri,
                            title: String::new(),
                            subtitle: String::new(),
                            width: f.width as f32,
                            height: f.height as f32,
                        });
                        true
                    }
                    _ => false,
                };
                if !shown {
                    self.add_e2ee_message(thread_key, ev, text.clone());
                }
                self.touch_thread(thread_key, ev.timestamp_ms, format!("{snippet_prefix}{label}"));
            }
            Content2::Reaction { target_id, emoji } => {
                if let Some(m) = self.find_message_mut(Some(thread_key), target_id) {
                    if emoji.is_empty() {
                        m.reactions.remove(&ev.sender);
                    } else {
                        m.reactions.insert(ev.sender, emoji.clone());
                    }
                }
            }
            Content2::Edit { target_id, text } => {
                if let Some(m) = self.find_message_mut(Some(thread_key), target_id) {
                    m.text = text.clone();
                    m.edited = true;
                }
            }
            Content2::Revoke { target_id } => {
                if let Some(m) = self.find_message_mut(Some(thread_key), target_id) {
                    m.unsent = true;
                    m.text.clear();
                }
            }
            Content2::None | Content2::Unsupported => {}
        }
        if !ev.from_me && self.initial_sync_done && matches!(ev.decoded.content, Content2::Text { .. } | Content2::Link { .. } | Content2::Media { .. }) {
            self.new_incoming.push((thread_key, ev.id.clone()));
        }
        true
    }

    fn add_e2ee_message(&mut self, thread_key: i64, ev: &E2eeIncoming, text: String) {
        self.insert_message(Message {
            id: ev.id.clone(),
            thread_key,
            sender_id: ev.sender,
            text,
            timestamp_ms: ev.timestamp_ms,
            reply_to_id: ev.decoded.reply_to.clone(),
            ..Default::default()
        });
    }

    fn touch_thread(&mut self, thread_key: i64, ts: i64, snippet: String) {
        if let Some(t) = self.threads.get_mut(&thread_key) {
            if ts >= t.last_activity_ms {
                t.last_activity_ms = ts;
                t.snippet = snippet;
            }
        }
    }

    pub fn apply(&mut self, calls: &[Call]) {
        for c in calls {
            self.apply_one(c);
        }
    }

    fn apply_one(&mut self, c: &Call) {
        match c.name.as_str() {
            "updateOrInsertThread" => {
                // Same leading layout as deleteThenInsertThread; arg 83 is the
                // sync group (95 = end-to-end encrypted). Name/picture/snippet are
                // usually null here, so titles come from participants/contacts.
                let key = c.arg(7).int();
                let t = self.threads.entry(key).or_default();
                t.key = key;
                t.last_activity_ms = t.last_activity_ms.max(c.arg(0).int());
                t.last_read_ms = t.last_read_ms.max(c.arg(1).int());
                if let Some(s) = c.arg(2).str() {
                    t.snippet = s.to_string();
                }
                if let Some(n) = c.arg(3).str() {
                    t.name = n.to_string();
                }
                if let Some(p) = c.arg(4).str() {
                    t.picture = p.to_string();
                }
                t.thread_type = c.arg(9).int();
                t.e2ee = c.arg(83).int() == 95 || t.thread_type == 15;
            }
            "deleteThenInsertThread" => {
                let key = c.arg(7).int();
                let t = self.threads.entry(key).or_default();
                t.key = key;
                t.last_activity_ms = c.arg(0).int();
                t.last_read_ms = c.arg(1).int();
                t.snippet = c.arg(2).string();
                t.name = c.arg(3).string();
                t.picture = c.arg(4).string();
                t.thread_type = c.arg(9).int();
                // (..., 31 customEmoji, ...)
                t.emoji = c.arg(31).string();
                t.has_more_before = true;
            }
            "updateThreadSnippet" => {
                if let Some(t) = self.threads.get_mut(&c.arg(0).int()) {
                    t.snippet = c.arg(1).string();
                }
            }
            "updateLastActivityTimestamp" | "updateThreadLastActivityTimestamp" => {
                if let Some(t) = self.threads.get_mut(&c.arg(0).int()) {
                    t.last_activity_ms = t.last_activity_ms.max(c.arg(1).int());
                }
            }
            "syncUpdateThreadName" | "updateThreadName" => {
                if let Some(t) = self.threads.get_mut(&c.arg(1).int()) {
                    t.name = c.arg(0).string();
                }
            }
            "updateThreadMuteSetting" => {
                if let Some(t) = self.threads.get_mut(&c.arg(0).int()) {
                    t.muted = c.arg(1).int() != 0;
                }
            }
            "deleteThread" => {
                let key = c.arg(0).int();
                self.threads.remove(&key);
                self.messages.remove(&key);
            }
            "verifyContactRowExists" => {
                let id = c.arg(0).int();
                let ct = self.contacts.entry(id).or_default();
                ct.id = id;
                ct.picture = c.arg(2).string();
                ct.name = c.arg(3).string();
                if let Some(f) = c.arg(8).str() {
                    ct.first_name = f.to_string();
                }
            }
            "addParticipantIdToGroupThread" => {
                // (threadKey, contactId, readWatermarkTimestampMs, readActionTimestampMs,
                //  deliveredWatermarkTimestampMs, nickname, ...)
                let (key, p) = (c.arg(0).int(), c.arg(1).int());
                let t = self.threads.entry(key).or_default();
                if !t.participants.contains(&p) {
                    t.participants.push(p);
                }
                let nickname = c.arg(5).string();
                if !nickname.trim().is_empty() {
                    self.nicknames.entry(key).or_default().insert(p, nickname);
                }
                let delivered = c.arg(4).int();
                if delivered > 0 {
                    let e = self.delivered_receipts.entry(key).or_default().entry(p).or_default();
                    *e = (*e).max(delivered);
                }
                let read = c.arg(2).int();
                if read > 0 {
                    let e = self.read_receipts.entry(key).or_default().entry(p).or_default();
                    *e = (*e).max(read);
                }
            }
            "removeParticipantFromThread" => {
                if let Some(t) = self.threads.get_mut(&c.arg(0).int()) {
                    let p = c.arg(1).int();
                    t.participants.retain(|x| *x != p);
                }
            }
            "upsertMessage" | "insertMessage" => {
                let msg = Message {
                    text: c.arg(0).string(),
                    thread_key: c.arg(3).int(),
                    timestamp_ms: c.arg(5).int(),
                    id: c.arg(8).string(),
                    otid: c.arg(9).int(),
                    sender_id: c.arg(10).int(),
                    sticker_id: c.arg(11).opt_int().filter(|s| *s != 0),
                    is_admin: c.arg(12).bool(),
                    unsent: c.arg(17).bool(),
                    reply_to_id: c.arg(23).str().map(str::to_string),
                    reply_snippet: c.arg(28).string(),
                    // (..., 27 replySnippet, 28 replyMessageText, 29 replyToUserId)
                    reply_to_user: c.arg(29).opt_int().filter(|id| *id != 0),
                    ..Default::default()
                };
                if msg.id.is_empty() {
                    return;
                }
                // Notify only for messages newer than the chat's last activity:
                // fetched history arrives the same way and mustn't ping.
                let newest = self.threads.get(&msg.thread_key).is_none_or(|t| msg.timestamp_ms > t.last_activity_ms);
                if self.initial_sync_done && newest && msg.sender_id != self.me && self.seen_ids.insert(msg.id.clone()) {
                    self.new_incoming.push((msg.thread_key, msg.id.clone()));
                } else {
                    self.seen_ids.insert(msg.id.clone());
                }
                if let Some(t) = self.threads.get_mut(&msg.thread_key) {
                    if msg.timestamp_ms > t.last_activity_ms {
                        t.last_activity_ms = msg.timestamp_ms;
                        t.snippet = msg.text.clone();
                    }
                    if msg.sender_id == self.me {
                        t.last_read_ms = t.last_read_ms.max(msg.timestamp_ms);
                    }
                }
                self.typing.entry(msg.thread_key).or_default().remove(&msg.sender_id);
                self.insert_message(msg);
            }
            "replaceOptimsiticMessage" | "replaceOptimisticMessage" => {
                let otid = c.arg(0).int();
                let id = c.arg(1).string();
                if let Some(m) = self
                    .messages
                    .values_mut()
                    .flat_map(|v| v.iter_mut())
                    .find(|m| m.otid == otid)
                {
                    m.id = id;
                    m.pending = false;
                }
            }
            "deleteMessage" => {
                let thread = c.arg(0).int();
                let id = c.arg(1).string();
                if let Some(v) = self.messages.get_mut(&thread) {
                    v.retain(|m| m.id != id);
                }
            }
            "updateUnsentMessageCollapsedStatus" | "setMessageUnsent" => {
                let id = c.arg(1).string();
                if let Some(m) = self.find_message_mut(Some(c.arg(0).int()), &id) {
                    m.unsent = true;
                    m.text.clear();
                    m.attachments.clear();
                }
            }
            "editMessage" => {
                let id = c.arg(0).string();
                let text = c.arg(2).string();
                if let Some(m) = self.find_message_mut(None, &id) {
                    m.text = text;
                    m.edited = true;
                }
            }
            "upsertReaction" => {
                // (threadKey, timestampMs, messageId, actorId, reaction, ...)
                self.set_reaction(c.arg(0).int(), &c.arg(2).string(), c.arg(3).int(), c.arg(4).string());
            }
            "deleteReaction" => {
                // (threadKey, messageId, actorId)
                self.set_reaction(c.arg(0).int(), &c.arg(1).string(), c.arg(2).int(), String::new());
            }
            "insertBlobAttachment" => {
                let filename = c.arg(0).string();
                let playable = c.arg(3).string();
                let preview = c.arg(8).str().map(str::to_string).unwrap_or_else(|| playable.clone());
                let kind = attachment_kind(&filename, c.arg(29).int());
                let att = Attachment {
                    kind,
                    url: if playable.is_empty() { preview.clone() } else { playable },
                    preview_url: preview,
                    title: filename,
                    subtitle: String::new(),
                    width: c.arg(14).int() as f32,
                    height: c.arg(15).int() as f32,
                };
                self.attach(c.arg(27).int(), &c.arg(32).string(), att);
            }
            "insertXmaAttachment" => {
                // Shares (posts, reels, photos from pages). Observed layout:
                // 8 preview image, 11 its MIME type, 13/14 its size, 25 thread,
                // 30 message id, 58 title / caption, 59 source ("Facebook"),
                // 102 the page's name, 104 its picture. There's no link to the
                // original post, only a media fallback path.
                let str_at = |i: usize| c.arg(i).str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
                let http_at = |i: usize| str_at(i).filter(|s| s.starts_with("http"));
                let preview = http_at(8).or_else(|| http_at(104)).or_else(|| http_at(65)).unwrap_or_default();
                let mime = str_at(11).unwrap_or_default();
                let title = str_at(58).unwrap_or_default();
                let source = [str_at(102), str_at(59)].into_iter().flatten().collect::<Vec<_>>().join(" · ");
                // A bare shared photo/video (no caption, no page) shows as media.
                let kind = if title.is_empty() && str_at(102).is_none() && !preview.is_empty() {
                    if mime.starts_with("video") { AttachmentKind::Video } else { AttachmentKind::Image }
                } else {
                    AttachmentKind::Link
                };
                let att = Attachment {
                    kind,
                    url: String::new(),
                    preview_url: preview,
                    title,
                    subtitle: source,
                    width: c.arg(13).int() as f32,
                    height: c.arg(14).int() as f32,
                };
                self.attach(c.arg(25).int(), &c.arg(30).string(), att);
            }
            "insertStickerAttachment" => {
                let url = c.args.iter().filter_map(|a| a.str()).find(|s| s.starts_with("http")).unwrap_or("").to_string();
                let mid = c.args.iter().filter_map(|a| a.str()).find(|s| s.starts_with("mid.")).unwrap_or("").to_string();
                let att = Attachment {
                    kind: AttachmentKind::Sticker,
                    url: url.clone(),
                    preview_url: url,
                    title: String::new(),
                    subtitle: String::new(),
                    width: 120.0,
                    height: 120.0,
                };
                self.attach(0, &mid, att);
            }
            "updateReadReceipt" => {
                let ts = c.arg(0).int();
                let thread = c.arg(1).int();
                let contact = c.arg(2).int();
                let e = self.read_receipts.entry(thread).or_default().entry(contact).or_default();
                *e = (*e).max(ts);
                if contact == self.me {
                    if let Some(t) = self.threads.get_mut(&thread) {
                        t.last_read_ms = t.last_read_ms.max(ts);
                    }
                }
            }
            "markThreadRead" | "markThreadReadV2" => {
                if let Some(t) = self.threads.get_mut(&c.arg(1).int()) {
                    t.last_read_ms = t.last_read_ms.max(c.arg(0).int());
                }
            }
            "updateTypingIndicator" => {
                let thread = c.arg(0).int();
                let sender = c.arg(1).int();
                let typing = c.typing_flag();
                let m = self.typing.entry(thread).or_default();
                if typing {
                    m.insert(sender, Instant::now());
                } else {
                    m.remove(&sender);
                }
            }
            "insertNewMessageRange" | "updateExistingMessageRange" => {
                // (threadKey, minTimestamp, maxTimestamp, minMessageId, maxMessageId, ..., hasMoreBefore, hasMoreAfter)
                if let Some(t) = self.threads.get_mut(&c.arg(0).int()) {
                    if let Some(more) = c.args.iter().skip(5).find_map(|a| match a { crate::lightspeed::Arg::Bool(b) => Some(*b), _ => None }) {
                        t.has_more_before = more;
                    }
                }
            }
            "executeFirstBlockForSyncTransaction" | "executeFirstBlockForSyncTransactionV4" => {
                if let Some(cursor) = c.arg(3).str() {
                    self.cursors.insert(c.arg(0).int(), cursor.to_string());
                }
            }
            _ => {}
        }
    }

    fn attach(&mut self, thread: i64, message_id: &str, att: Attachment) {
        if message_id.is_empty() {
            return;
        }
        let thread = if thread == 0 { None } else { Some(thread) };
        if let Some(m) = self.find_message_mut(thread, message_id) {
            // The same share can arrive twice with differently signed CDN URLs
            // (other host, other query): compare the file path only.
            fn key(a: &Attachment) -> (&str, &str) {
                (media_path(&a.url), media_path(&a.preview_url))
            }
            if !m.attachments.iter().any(|a| key(a) == key(&att)) {
                m.attachments.push(att);
            }
        }
    }
}

impl Call {
    /// `updateTypingIndicator(threadKey, senderId, isTyping)` sends a bool on
    /// some versions and 0/1 ints on others.
    fn typing_flag(&self) -> bool {
        self.arg(2).bool()
    }
}

/// A media URL without host and query (`/v/t15.../photo.jpg`).
fn media_path(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let path = rest.find('/').map_or("", |i| &rest[i..]);
    path.split('?').next().unwrap_or(path)
}

fn attachment_kind(filename: &str, type_code: i64) -> AttachmentKind {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".mp4") || lower.ends_with(".mov") || lower.starts_with("video") || type_code == 3 {
        AttachmentKind::Video
    } else if lower.ends_with(".mp3") || lower.ends_with(".m4a") || lower.ends_with(".aac") || lower.starts_with("audio") || type_code == 4 {
        AttachmentKind::Audio
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") || lower.ends_with(".png") || lower.ends_with(".gif") || lower.ends_with(".webp") || lower.starts_with("image") || type_code == 2 {
        AttachmentKind::Image
    } else {
        AttachmentKind::File
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    #[test]
    fn cache_keeps_the_newest_messages_and_skips_pending() {
        let mut st = Store { me: 1, ..Default::default() };
        st.threads.insert(7, Thread { key: 7, name: "Group".into(), ..Default::default() });
        for i in 0..100 {
            st.insert_message(Message { id: format!("m{i}"), thread_key: 7, timestamp_ms: i, text: format!("t{i}"), ..Default::default() });
        }
        st.add_pending(7, 42, "unsent".into(), None);
        let snap: Snapshot = serde_json::from_slice(&st.cache_bytes().unwrap()).unwrap();
        assert_eq!(snap.me, 1);
        assert_eq!(snap.threads.len(), 1);
        assert_eq!(snap.messages.len(), CACHED_MESSAGES_PER_CHAT);
        assert!(snap.messages.iter().all(|m| !m.pending));
        assert_eq!(snap.messages.last().unwrap().id, "m99", "the newest ones are kept");
    }

    use super::*;
    use crate::lightspeed::decode_payload;

    #[test]
    fn applies_thread_contact_and_message() {
        let mut s = Store { me: 1, ..Default::default() };
        let p = r#"{"step":[1,
            [5,"deleteThenInsertThread",[19,"200"],[19,"100"],"hey","",[9],false,[19,"80"],[19,"42"],[19,"0"],[19,"1"]],
            [5,"verifyContactRowExists",[19,"42"],[19,"1"],"http://pic","Alice Smith",[19,"1"],"x",[19,"0"],[19,"0"],"Alice"],
            [5,"upsertMessage","hey",[9],[19,"80"],[19,"42"],[19,"0"],[19,"200"],[19,"200"],[9],"mid.$a","7",[19,"42"]],
            [5,"upsertReaction",[19,"42"],[19,"300"],"mid.$a",[19,"1"],"❤",[19,"80"],[19,"300"]]
        ]}"#;
        s.apply(&decode_payload(p).unwrap());
        let t = &s.threads[&42];
        assert!(t.unread());
        assert_eq!(s.thread_title(t), "Alice Smith");
        let m = &s.messages[&42][0];
        assert_eq!(m.text, "hey");
        assert_eq!(m.sender_id, 42);
        assert_eq!(m.reactions[&1], "❤");
    }
}


/// Write cache bytes atomically, readable only by us (it holds messages).
pub fn write_cache(bytes: &[u8]) {
    if let Some(path) = cache_path() {
        let _ = crate::e2ee_media::write_private(&path, bytes);
    }
}
