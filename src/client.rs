//! Background engine: bootstraps the session, runs the initial GraphQL sync,
//! keeps the Lightspeed connection (DGW, see `dgw.rs`) alive and turns UI
//! commands into Lightspeed tasks.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::lightspeed::decode_payload;
use crate::dgw::{self, Frame};
use crate::session::{new_otid, Cookies, Session, USER_AGENT};
use crate::store::{now_ms, Store};

/// A tagged member inside a message's text; offset and length count UTF-16
/// code units, like the web client's JavaScript strings.
#[derive(Debug, Clone)]
pub struct Mention {
    pub id: i64,
    pub offset: usize,
    pub length: usize,
}

#[derive(Debug)]
pub enum Command {
    SendText { thread: i64, text: String, reply_to: Option<String>, mentions: Vec<Mention> },
    SendFiles { thread: i64, paths: Vec<PathBuf>, reply_to: Option<String> },
    React { thread: i64, message_id: String, emoji: String },
    Unsend { thread: i64, message_id: String },
    Edit { message_id: String, text: String },
    MarkRead { thread: i64 },
    Typing { thread: i64, is_group: bool, typing: bool },
    LoadOlder { thread: i64 },
    LoadMoreThreads,
    Mute { thread: i64, minutes: Option<i64> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Connecting,
    Syncing,
    Online,
    Reconnecting(String),
    LoggedOut(String),
}

pub struct Shared {
    pub store: Mutex<Store>,
    pub status: Mutex<Status>,
    pub error: Mutex<Option<String>>,
    /// Queue into the E2EE connection, once it's running.
    e2ee_tx: Mutex<Option<mpsc::UnboundedSender<crate::e2ee_listen::Outgoing>>>,
}

pub fn spawn(cookies: Cookies, repaint: impl Fn() + Send + Sync + 'static) -> (Arc<Shared>, mpsc::UnboundedSender<Command>) {
    let shared = Arc::new(Shared {
        // Last session's inbox, shown right away while we connect and sync.
        store: Mutex::new(Store::load_cache().unwrap_or_default()),
        status: Mutex::new(Status::Connecting),
        error: Mutex::new(None),
        e2ee_tx: Mutex::new(None),
    });
    let (tx, rx) = mpsc::unbounded_channel();
    let s = shared.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(run(cookies, s, rx, Arc::new(repaint)));
    });
    (shared, tx)
}

type Repaint = Arc<dyn Fn() + Send + Sync>;

async fn run(cookies: Cookies, shared: Arc<Shared>, mut rx: mpsc::UnboundedReceiver<Command>, repaint: Repaint) {
    let set_status = |st: Status| {
        eprintln!("[status] {st:?}");
        *shared.status.lock().unwrap() = st;
        repaint();
    };
    let mut backoff = 1u64;
    let mut e2ee_started = false;
    loop {
        set_status(Status::Connecting);
        let session = match Session::connect(cookies.clone()).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("log in") {
                    set_status(Status::LoggedOut(msg));
                    return;
                }
                set_status(Status::Reconnecting(msg));
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
                continue;
            }
        };
        {
            let mut st = shared.store.lock().unwrap();
            if st.me != 0 && st.me != session.boot.fbid {
                *st = Store::default(); // the cache belongs to another account
            }
            st.me = session.boot.fbid;
        }

        set_status(Status::Syncing);
        let cursor = shared.store.lock().unwrap().cursors.get(&1).cloned();
        match session.ls_sync(1, cursor.as_deref()).await.and_then(|p| decode_payload(&p)) {
            Ok(calls) => {
                shared.store.lock().unwrap().apply(&calls);
                // Database 95 holds end-to-end encrypted chats: we can list them
                // (participants, activity) even though we can't decrypt messages.
                match session.ls_sync(95, None).await.and_then(|p| decode_payload(&p)) {
                    Ok(calls) => shared.store.lock().unwrap().apply(&calls),
                    Err(e) => eprintln!("[sync] e2ee thread list: {e}"),
                }
                // Replay E2EE messages decrypted in earlier runs (before notifications start).
                {
                    let me = session.boot.fbid;
                    let mut st = shared.store.lock().unwrap();
                    for rec in crate::e2ee_listen::read_inbox() {
                        if let Some(ev) = crate::e2ee_listen::to_incoming(&rec, me) {
                            st.apply_e2ee(&ev);
                        }
                    }
                }
                shared.store.lock().unwrap().initial_sync_done = true;
                if !e2ee_started && crate::e2ee::Device::load().and_then(|d| d.wa_device_id).is_some() {
                    e2ee_started = true;
                    spawn_e2ee(session.clone(), shared.clone(), repaint.clone());
                }
            }
            Err(e) => {
                set_status(Status::Reconnecting(format!("sync failed: {e}")));
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
                continue;
            }
        }
        repaint();

        let mut conn = Conn::new(session.clone(), shared.clone(), repaint.clone());
        match conn.run(&mut rx, &set_status).await {
            Ok(()) => return, // UI closed
            Err(e) => {
                set_status(Status::Reconnecting(e.to_string()));
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

/// Background E2EE connection: decrypts end-to-end encrypted chats into the
/// store and sends what the UI queues for them.
fn spawn_e2ee(session: Arc<Session>, shared: Arc<Shared>, repaint: Repaint) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    *shared.e2ee_tx.lock().unwrap() = Some(tx);
    tokio::spawn(async move {
        let me = session.boot.fbid;
        let result = crate::e2ee_listen::run(
            &session,
            None,
            Some(&mut rx),
            |rec| {
                if let Some(ev) = crate::e2ee_listen::to_incoming(rec, me) {
                    shared.store.lock().unwrap().apply_e2ee(&ev);
                    repaint();
                }
            },
            |receipt| {
                shared.store.lock().unwrap().apply_e2ee_receipt(receipt);
                repaint();
            },
            |line| eprintln!("[e2ee] {line}"),
        )
        .await;
        if let Err(e) = result {
            eprintln!("[e2ee] stopped: {e:#}");
        }
    });
}

/// DGW streams kept open for database syncs: (stream, database), the same
/// set as the web client. Database 1 is the mailbox, 95 the end-to-end
/// encrypted thread list.
const SYNC_STREAMS: [(u16, i64); 4] = [(0, 1), (1, 2), (2, 16), (3, 95)];
const FIRST_REQUEST_STREAM: u16 = 4;

struct Conn {
    session: Arc<Session>,
    shared: Arc<Shared>,
    repaint: Repaint,
    /// Next DGW stream for a request (the first few are database syncs).
    next_stream: u16,
    /// Open database sync streams: stream -> database.
    sync_streams: HashMap<u16, i64>,
    request_id: i64,
    epoch: i64,
    /// Lightspeed task ids must be unique: the server answers a reused id with
    /// `taskExists` and drops the task.
    task_id: i64,
}

impl Conn {
    fn new(session: Arc<Session>, shared: Arc<Shared>, repaint: Repaint) -> Self {
        Conn { session, shared, repaint, next_stream: FIRST_REQUEST_STREAM, sync_streams: HashMap::new(), request_id: 0, epoch: now_ms() << 22, task_id: now_ms() % 1_000_000 }
    }

    /// A Lightspeed request body (the JSON the web client sends on a DGW stream).
    fn ls_req(&mut self, kind: i64, payload: String) -> Vec<u8> {
        self.request_id += 1;
        json!({
            "app_id": self.session.boot.ls_app_id,
            "payload": payload,
            "request_id": self.request_id,
            "type": kind,
        })
        .to_string()
        .into_bytes()
    }

    /// Open a fresh stream for one request, like the web client does.
    fn request_frames(&mut self, body: &[u8]) -> Vec<u8> {
        let stream = self.next_stream;
        self.next_stream = if stream == u16::MAX { FIRST_REQUEST_STREAM } else { stream + 1 };
        let mut out = dgw::establish(stream);
        out.extend(dgw::data(stream, 0, body));
        out
    }

    /// Wrap Lightspeed tasks (label + payload + queue) in a type-3 request.
    fn tasks(&mut self, tasks: Vec<(&str, Value, String)>) -> Vec<u8> {
        let tasks: Vec<Value> = tasks
            .into_iter()
            .map(|(label, payload, queue)| {
                self.task_id += 1;
                json!({
                    "failure_count": null,
                    "label": label,
                    "payload": payload.to_string(),
                    "queue_name": queue,
                    "task_id": self.task_id,
                })
            })
            .collect();
        let payload = json!({
            "epoch_id": self.epoch,
            "tasks": tasks,
            "version_id": self.session.boot.ls_version.to_string(),
        });
        self.ls_req(3, payload.to_string())
    }

    /// Database sync: resume from our cursor (type 2, as the web client does)
    /// or start from a snapshot (type 1).
    fn sync_request(&mut self, database: i64) -> Vec<u8> {
        let cursor = self.shared.store.lock().unwrap().cursors.get(&database).cloned();
        let (kind, sync_params) = match cursor {
            Some(_) => (2, Value::Null),
            None => (1, Value::String(crate::session::ls_sync_params())),
        };
        let payload = json!({
            "database": database,
            "epoch_id": self.epoch,
            "failure_count": null,
            "last_applied_cursor": cursor,
            "sync_params": sync_params,
            "version": self.session.boot.ls_version,
        });
        self.ls_req(kind, payload.to_string())
    }

    async fn run(&mut self, rx: &mut mpsc::UnboundedReceiver<Command>, set_status: &impl Fn(Status)) -> Result<()> {
        let url = dgw::lightspeed_url(&self.session.boot.ls_app_id, self.session.boot.fbid, &self.session.device_id);
        let mut ws = dgw::Socket::open(&url, &self.session.cookies.0, USER_AGENT).await?;

        // One long-lived stream per synced database; pushes (new messages,
        // fetched history, receipts) arrive on these.
        let mut hello = Vec::new();
        self.sync_streams = SYNC_STREAMS.into_iter().collect();
        for (stream, database) in SYNC_STREAMS {
            let body = self.sync_request(database);
            hello.extend(dgw::establish(stream));
            hello.extend(dgw::data(stream, 0, &body));
        }
        ws.send(hello).await?;

        let mut connected = false;
        let mut buf = Vec::new();
        // Liveness: the server answers every ping, so a long silence means the
        // socket died without closing (seen in practice); reconnect then.
        let mut last_frame = tokio::time::Instant::now();
        let mut ping = tokio::time::interval(Duration::from_secs(15));
        ping.tick().await;

        loop {
            tokio::select! {
                msg = ws.recv() => {
                    let Some(bytes) = msg? else { anyhow::bail!("connection closed") };
                    last_frame = tokio::time::Instant::now();
                    buf.extend(bytes);
                    let mut reply = Vec::new();
                    for frame in dgw::parse(&mut buf) {
                        if std::env::var_os("MESSENGER_DEBUG").is_some() {
                            match &frame {
                                Frame::Data { stream, ack_id, body } => eprintln!("[dgw] ← data s{stream} ack={ack_id:?} {} bytes", body.len()),
                                other => eprintln!("[dgw] ← {other:?}"),
                            }
                        }
                        match frame {
                            Frame::Establish { stream, payload } => {
                                let ok = serde_json::from_slice::<Value>(&payload).ok().and_then(|v| v["code"].as_i64()) == Some(200);
                                if !ok && stream < FIRST_REQUEST_STREAM {
                                    anyhow::bail!("Lightspeed stream refused: {}", String::from_utf8_lossy(&payload));
                                }
                                if stream == 0 && !connected {
                                    connected = true;
                                    set_status(Status::Online);
                                }
                            }
                            Frame::Data { stream, ack_id, body } => {
                                if let Some(id) = ack_id {
                                    reply.extend(dgw::ack(stream, id));
                                }
                                let sync_failed = self.handle_ls_resp(&body);
                                if let Some(database) = self.sync_streams.get(&stream).copied() {
                                    if sync_failed {
                                        // The server rejected our cursor (it happens with the
                                        // one from the GraphQL sync) and stops sending deltas
                                        // on this stream: start that database over from a
                                        // snapshot on a fresh stream.
                                        eprintln!("[sync] database {database} sync failed; resyncing from a snapshot");
                                        self.shared.store.lock().unwrap().cursors.remove(&database);
                                        self.sync_streams.remove(&stream);
                                        let body = self.sync_request(database);
                                        let fresh = self.next_stream;
                                        self.next_stream += 1;
                                        self.sync_streams.insert(fresh, database);
                                        reply.extend(dgw::establish(fresh));
                                        reply.extend(dgw::data(fresh, 0, &body));
                                    }
                                } else {
                                    // A request stream carries one answer; close our side.
                                    reply.extend(dgw::end(stream));
                                }
                            }
                            Frame::End { stream } if self.sync_streams.contains_key(&stream) => {
                                anyhow::bail!("Lightspeed sync stream {stream} closed by the server");
                            }
                            Frame::Ping => reply.push(dgw::PONG),
                            _ => {}
                        }
                    }
                    if !reply.is_empty() {
                        if std::env::var_os("MESSENGER_DEBUG").is_some() {
                            let mut copy = reply.clone();
                            eprintln!("[dgw] → {:?}", dgw::parse(&mut copy));
                        }
                        ws.send(reply).await?;
                    }
                }
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { return Ok(()) };
                    // Encrypted chats go over the E2EE connection, so they don't
                    // wait for this one (they used to be dropped while it reconnected).
                    if !connected && !self.is_e2ee_send(&cmd) {
                        continue;
                    }
                    match self.command(cmd).await {
                        Ok(Some(body)) => {
                            let frames = self.request_frames(&body);
                            ws.send(frames).await?;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            eprintln!("[command] failed: {e:#}");
                            *self.shared.error.lock().unwrap() = Some(e.to_string());
                            (self.repaint)();
                        }
                    }
                }
                _ = ping.tick() => {
                    if last_frame.elapsed() > Duration::from_secs(45) {
                        anyhow::bail!("no data from the server for 45s");
                    }
                    ws.send(vec![dgw::PING]).await?;
                    (self.repaint)(); // expire typing indicators
                }
            }
        }
    }

    /// Apply a Lightspeed response; returns true if it was a sync failure
    /// (`handleSyncFailure`), after which that sync stream gets no more deltas.
    fn handle_ls_resp(&self, payload: &[u8]) -> bool {
        let Ok(v) = serde_json::from_slice::<Value>(payload) else { return false };
        let Some(ls) = v.get("payload").and_then(Value::as_str) else { return false };
        let decoded = decode_payload(ls);
        if let (Err(e), Some(_)) = (&decoded, std::env::var_os("MESSENGER_DEBUG")) {
            eprintln!("[ls_resp] request_id={} undecodable ({} bytes): {e}", v.get("request_id").unwrap_or(&Value::Null), ls.len());
        }
        if let Ok(calls) = decoded {
            if std::env::var_os("MESSENGER_DEBUG").is_some() {
                let mut names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
                names.sort();
                names.dedup();
                eprintln!("[ls_resp] request_id={} calls={} procs={names:?}", v.get("request_id").unwrap_or(&Value::Null), calls.len());
                for c in calls.iter().filter(|c| c.name == "taskExists" || c.name == "removeTask" || c.name == "updateSyncGroupForThreadType") {
                    eprintln!("  {} {:?}", c.name, c.args);
                }
                for c in calls.iter().filter(|c| c.name == "upsertMessage" || c.name == "insertMessage") {
                    eprintln!("  {} thread={} age={}s", c.name, c.arg(3).int(), (now_ms() - c.arg(5).int()) / 1000);
                }
            }
            let failed = calls.iter().any(|c| c.name == "handleSyncFailure");
            self.shared.store.lock().unwrap().apply(&calls);
            (self.repaint)();
            return failed;
        }
        false
    }

    /// Fetch the 20 messages before the oldest one we have. Runs over HTTP, as
    /// a FetchMessages task (228) followed by a delta sync of database 1 from
    /// our cursor: the delta carries the fetched range. (Over DGW the server
    /// drops the connection instead of pushing the result.)
    fn load_older(&self, thread: i64) {
        let (oldest, cursor) = {
            let st = self.shared.store.lock().unwrap();
            let oldest = st.messages.get(&thread).and_then(|v| v.iter().find(|m| !m.pending)).map(|m| (m.timestamp_ms, m.id.clone()));
            (oldest, st.cursors.get(&1).cloned())
        };
        let (ts, mid) = oldest.unwrap_or((now_ms(), String::new()));
        let task = json!({
            "thread_key": thread, "direction": 0, "reference_timestamp_ms": ts,
            "reference_message_id": mid, "sync_group": 1, "cursor": cursor,
        });
        let (session, shared, repaint) = (self.session.clone(), self.shared.clone(), self.repaint.clone());
        tokio::spawn(async move {
            let result = async {
                session.ls_tasks(&[("228", task, format!("mrq.{thread}"))]).await?;
                let cursor = shared.store.lock().unwrap().cursors.get(&1).cloned();
                decode_payload(&session.ls_sync(1, cursor.as_deref()).await?)
            }
            .await;
            match result {
                Ok(calls) => {
                    let mut st = shared.store.lock().unwrap();
                    let before = st.messages.get(&thread).map_or(0, Vec::len);
                    st.apply(&calls);
                    if std::env::var_os("MESSENGER_DEBUG").is_some() {
                        eprintln!("[load_older] {thread}: {before} -> {} messages", st.messages.get(&thread).map_or(0, Vec::len));
                    }
                }
                Err(e) => eprintln!("[load_older] {thread}: {e:#}"),
            }
            repaint();
        });
    }

    /// Encrypted 1:1 chats go through the E2EE connection, not Lightspeed.
    fn is_e2ee_send(&self, cmd: &Command) -> bool {
        let thread = match cmd {
            Command::SendText { thread, .. } | Command::SendFiles { thread, .. } => thread,
            _ => return false,
        };
        self.shared.store.lock().unwrap().threads.get(thread).is_some_and(|t| t.e2ee)
    }

    /// `shown` is what the pending bubble says until the send completes.
    fn send_e2ee(&self, thread: i64, payload: crate::e2ee_listen::OutgoingPayload, shown: String) -> Result<()> {
        let peer = {
            let st = self.shared.store.lock().unwrap();
            st.threads.get(&thread).and_then(|t| t.participants.iter().copied().find(|p| *p != st.me))
        };
        let peer = peer.ok_or_else(|| anyhow::anyhow!("can't tell who this encrypted chat is with"))?;
        let tx = self.shared.e2ee_tx.lock().unwrap().clone();
        let tx = tx.ok_or_else(|| anyhow::anyhow!("encrypted chats aren't connected yet"))?;
        let otid = new_otid();
        self.shared.store.lock().unwrap().add_pending(thread, otid, shown, None);
        (self.repaint)();
        let (done, result) = tokio::sync::oneshot::channel();
        tx.send(crate::e2ee_listen::Outgoing { peer, payload, done })
            .map_err(|_| anyhow::anyhow!("the encrypted connection stopped"))?;
        let (shared, repaint) = (self.shared.clone(), self.repaint.clone());
        tokio::spawn(async move {
            let outcome = result.await.unwrap_or_else(|_| Err("the encrypted connection stopped".into()));
            // On success the sent message is already in the store (via the inbox record).
            shared.store.lock().unwrap().remove_pending(thread, otid);
            if let Err(e) = outcome {
                *shared.error.lock().unwrap() = Some(format!("Couldn't send the encrypted message: {e}"));
            }
            repaint();
        });
        Ok(())
    }

    async fn command(&mut self, cmd: Command) -> Result<Option<Vec<u8>>> {
        let me = self.session.boot.fbid;
        use crate::e2ee_listen::OutgoingPayload;
        let e2ee = |t: &i64| self.shared.store.lock().unwrap().threads.get(t).is_some_and(|t| t.e2ee);
        match &cmd {
            Command::SendText { thread, text, .. } if e2ee(thread) => {
                self.send_e2ee(*thread, OutgoingPayload::Text(text.clone()), text.clone())?;
                return Ok(None);
            }
            // Encrypted chats: pictures only for now (videos and files aren't wired up).
            Command::SendFiles { thread, paths, .. } if e2ee(thread) => {
                let (images, others): (Vec<_>, Vec<_>) =
                    paths.iter().partition(|p| std::fs::read(p).ok().is_some_and(|b| image::guess_format(&b).is_ok()));
                for path in images {
                    self.send_e2ee(*thread, OutgoingPayload::Image(path.clone()), "📷 Sending photo…".into())?;
                }
                if !others.is_empty() {
                    anyhow::bail!("Only pictures can be sent in encrypted chats so far");
                }
                return Ok(None);
            }
            _ => {}
        }
        Ok(Some(match cmd {
            Command::SendText { thread, text, reply_to, mentions } => {
                let otid = new_otid();
                self.shared.store.lock().unwrap().add_pending(thread, otid, text.clone(), reply_to.clone());
                (self.repaint)();
                let mut p = json!({
                    "thread_id": thread, "otid": otid.to_string(), "source": 65537, "send_type": 1,
                    "sync_group": 1, "text": text, "initiating_source": 1, "skip_url_preview_gen": 0,
                    "text_has_links": text.contains("http") as i32, "multitab_env": 0,
                });
                if let Some(r) = reply_to {
                    p["reply_metadata"] = json!({"reply_source_id": r, "reply_source_type": 1, "reply_type": 0});
                }
                if !mentions.is_empty() {
                    // Comma-separated lists, "p" = person (as the web client sends them).
                    let join = |f: &dyn Fn(&Mention) -> String| mentions.iter().map(f).collect::<Vec<_>>().join(",");
                    p["mention_data"] = json!({
                        "mention_ids": join(&|m| m.id.to_string()),
                        "mention_offsets": join(&|m| m.offset.to_string()),
                        "mention_lengths": join(&|m| m.length.to_string()),
                        "mention_types": join(&|_| "p".to_string()),
                    });
                }
                let read = json!({"thread_id": thread, "last_read_watermark_ts": now_ms(), "sync_group": 1});
                self.tasks(vec![("46", p, thread.to_string()), ("21", read, thread.to_string())])
            }
            Command::SendFiles { thread, paths, reply_to } => {
                let mut ids = Vec::new();
                for path in &paths {
                    ids.push(self.session.upload(path).await?);
                }
                let otid = new_otid();
                let mut p = json!({
                    "thread_id": thread, "otid": otid.to_string(), "source": 65537, "send_type": 3,
                    "sync_group": 1, "text": null, "attachment_fbids": ids, "initiating_source": 1,
                });
                if let Some(r) = reply_to {
                    p["reply_metadata"] = json!({"reply_source_id": r, "reply_source_type": 1, "reply_type": 0});
                }
                self.tasks(vec![("46", p, thread.to_string())])
            }
            Command::React { thread, message_id, emoji } => {
                // Show it right away, like the web client; the server's echo agrees.
                self.shared.store.lock().unwrap().set_reaction(thread, &message_id, me, emoji.clone());
                (self.repaint)();
                let p = json!({
                    "thread_key": thread, "timestamp_ms": now_ms(), "message_id": message_id,
                    "actor_id": me, "reaction": emoji, "reaction_style": null, "sync_group": 1,
                    "send_attribution": 65537,
                });
                let queue = json!(["reaction", message_id]).to_string();
                self.tasks(vec![("29", p, queue)])
            }
            Command::Unsend { thread, message_id } => {
                let p = json!({"message_id": message_id, "thread_key": thread, "sync_group": 1});
                self.tasks(vec![("33", p, "unsend_message".into())])
            }
            Command::Edit { message_id, text } => {
                let p = json!({"message_id": message_id, "text": text});
                self.tasks(vec![("742", p, "edit_message".into())])
            }
            Command::MarkRead { thread } => {
                let p = json!({"thread_id": thread, "last_read_watermark_ts": now_ms(), "sync_group": 1});
                self.tasks(vec![("21", p, thread.to_string())])
            }
            Command::Typing { thread, is_group, typing } => {
                let inner = json!({
                    "thread_key": thread, "is_group_thread": is_group as i32, "is_typing": typing as i32,
                    "attribution": 0, "sync_group": 1, "thread_type": if is_group { 2 } else { 1 },
                });
                let payload = json!({
                    "label": "3", "payload": inner.to_string(),
                    "version": self.session.boot.ls_version.to_string(),
                });
                self.ls_req(4, payload.to_string())
            }
            Command::LoadOlder { thread } => {
                self.load_older(thread);
                return Ok(None);
            }
            Command::LoadMoreThreads => {
                let oldest = {
                    let st = self.shared.store.lock().unwrap();
                    st.threads.values().min_by_key(|t| t.last_activity_ms).map(|t| (t.key, t.last_activity_ms))
                };
                let Some((key, ts)) = oldest else { return Ok(None) };
                let p = json!({
                    "is_after": 0, "parent_thread_key": 0, "reference_thread_key": key,
                    "reference_activity_timestamp": ts, "additional_pages_to_fetch": 0,
                    "cursor": null, "messaging_tag": null, "sync_group": 1,
                });
                self.tasks(vec![("145", p, "trq".into())])
            }
            Command::Mute { thread, minutes } => {
                let expire = match minutes {
                    None => 0,
                    Some(-1) => -1,
                    Some(m) => now_ms() + m * 60_000,
                };
                let p = json!({"thread_key": thread, "mailbox_type": 0, "mute_expire_time_ms": expire, "sync_group": 1});
                self.tasks(vec![("144", p, thread.to_string())])
            }
        }))
    }
}
