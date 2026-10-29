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
    /// Re-fetch a chat's newest messages (fills gaps left while offline).
    RefreshRecent { thread: i64 },
    LoadStickerPacks,
    LoadStickerPack { pack: String },
    SendSticker { thread: i64, sticker_id: i64 },
    LoadMoreThreads,
    Mute { thread: i64, minutes: Option<i64> },
}

#[derive(Default)]
pub struct StickerState {
    /// None until the tray has been fetched.
    pub packs: Option<Vec<crate::session::StickerPack>>,
    pub pack_stickers: std::collections::HashMap<String, Vec<crate::session::Sticker>>,
    /// Requests in flight (pack ids, or "" for the tray).
    pub loading: std::collections::HashSet<String>,
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
    /// The sticker tray and loaded packs (fetched on demand).
    pub stickers: Mutex<StickerState>,
    /// Queue into the E2EE connection, once it's running.
    e2ee_tx: Mutex<Option<mpsc::UnboundedSender<crate::e2ee_listen::Outgoing>>>,
    /// Database 1 cursor of the HTTP (GraphQL) sync chain. Kept apart from the
    /// DGW streams' cursor (`Store::cursors`): the server rejects one chain's
    /// cursor on the other with `handleSyncFailure`.
    http_cursor: Mutex<Option<String>>,
    /// A mailbox poll is in flight.
    polling: std::sync::atomic::AtomicBool,
}

pub fn spawn(cookies: Cookies, repaint: impl Fn() + Send + Sync + 'static) -> (Arc<Shared>, mpsc::UnboundedSender<Command>) {
    let shared = Arc::new(Shared {
        // Last session's inbox, shown right away while we connect and sync.
        store: Mutex::new(Store::load_cache().unwrap_or_default()),
        status: Mutex::new(Status::Connecting),
        error: Mutex::new(None),
        stickers: Mutex::new(StickerState::default()),
        e2ee_tx: Mutex::new(None),
        polling: Default::default(),
        http_cursor: Mutex::new(None),
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
                shared.note_http_cursor(&calls);
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
        let mut result = conn.run(&mut rx, &set_status).await;
        // After the snapshot connection, subscribe like the web client: a new
        // socket whose streams are delta syncs from the DGW cursors.
        while matches!(result, Ok(true)) {
            result = conn.run(&mut rx, &set_status).await;
        }
        match result {
            Ok(_) => return, // UI closed
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
    /// Sync streams still waiting for their snapshot (type 1) answer.
    snapshot_streams: std::collections::HashSet<u16>,
    /// The snapshots are in: connections now open with delta syncs.
    subscribed: bool,
    request_id: i64,
    epoch: i64,
    /// Lightspeed task ids must be unique: the server answers a reused id with
    /// `taskExists` and drops the task.
    task_id: i64,
}

impl Conn {
    fn new(session: Arc<Session>, shared: Arc<Shared>, repaint: Repaint) -> Self {
        Conn { session, shared, repaint, next_stream: FIRST_REQUEST_STREAM, sync_streams: HashMap::new(), snapshot_streams: Default::default(), subscribed: false, request_id: 0, epoch: now_ms() << 22, task_id: now_ms() % 1_000_000 }
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

    /// One DGW connection. The first one (`!self.subscribed`) starts every
    /// database from a snapshot and returns `Ok(true)` once all are in; the
    /// next one resumes them with delta syncs and gets the live pushes.
    /// `Ok(false)` means the UI closed.
    async fn run(&mut self, rx: &mut mpsc::UnboundedReceiver<Command>, set_status: &impl Fn(Status)) -> Result<bool> {
        let url = dgw::lightspeed_url(&self.session.boot.ls_app_id, self.session.boot.fbid, &self.session.device_id, self.session.boot.region.as_deref());
        let mut ws = dgw::Socket::open(&url, &self.session.cookies.0, USER_AGENT).await?;
        if std::env::var_os("MESSENGER_DEBUG").is_some() {
            eprintln!("[dgw] region hint {:?}", self.session.boot.region);
        }

        // One long-lived stream per synced database; pushes (new messages,
        // fetched history, receipts) arrive on these.
        let mut hello = Vec::new();
        self.sync_streams = SYNC_STREAMS.into_iter().collect();
        // Live pushes only follow a stream that opened with a delta sync
        // (type 2) from a cursor DGW itself issued. GraphQL cursors make it fail
        // the sync, and a snapshot stream (type 1) answers once and stays silent
        // (a delta sent after it on the same stream too). So the first
        // connection takes snapshots, then we reconnect with their cursors.
        if !self.subscribed {
            let mut st = self.shared.store.lock().unwrap();
            for (_, database) in SYNC_STREAMS {
                st.cursors.remove(&database);
            }
            self.snapshot_streams = SYNC_STREAMS.into_iter().map(|(stream, _)| stream).collect();
        } else {
            self.snapshot_streams.clear();
        }
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
        // DGW doesn't push live changes to us yet (the web client gets them on
        // its database 1 stream; something it does to subscribe is still
        // missing), so poll a delta sync of the mailbox over HTTP meanwhile.
        let mut poll = tokio::time::interval(Duration::from_secs(5));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        poll.tick().await;

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
                                    let has_cursor = self.shared.store.lock().unwrap().cursors.contains_key(&database);
                                    if !sync_failed && has_cursor && self.snapshot_streams.remove(&stream) && self.snapshot_streams.is_empty() {
                                        // Every snapshot is in: acknowledge, then subscribe on a
                                        // new connection.
                                        ws.send(reply).await?;
                                        self.subscribed = true;
                                        return Ok(true);
                                    } else if sync_failed {
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
                                        self.snapshot_streams.insert(fresh);
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
                    let Some(cmd) = cmd else { return Ok(false) };
                    // Commands go over HTTP or the E2EE connection, so they don't
                    // wait for the DGW socket (they used to be dropped while it
                    // reconnected).
                    match self.command(cmd).await {
                        // Every request (tasks, typing) goes over HTTP: sent on the DGW
                        // socket, the server answers and then drops the whole
                        // connection, losing live updates until we reconnect. DGW
                        // carries only the database sync streams.
                        Ok(Some(body)) => match lightspeed_request(&body) {
                            Some((kind, payload)) => self.send_request_http(kind, payload),
                            None => eprintln!("[command] malformed request body"),
                        },
                        Ok(None) => {}
                        Err(e) => {
                            eprintln!("[command] failed: {e:#}");
                            *self.shared.error.lock().unwrap() = Some(e.to_string());
                            (self.repaint)();
                        }
                    }
                }
                _ = poll.tick(), if self.subscribed => self.poll_mailbox(),
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
                if calls.len() <= 2 && ls.len() > 500 && std::env::var_os("MESSENGER_SHAPE_ONCE").is_some() {
                    // Structure only: every string literal that isn't a plain
                    // identifier (procedure names) is masked.
                    let masked = regex_lite_mask(ls);
                    eprintln!("  payload shape: {masked}");
                }
                for c in calls.iter().filter(|c| c.name == "executeFirstBlockForSyncTransactionV4") {
                    eprintln!("  sync transaction: database {:?} epoch {:?} prev cursor {} new cursor {}", c.arg(0), c.arg(1), c.arg(2).str().map_or(0, str::len), c.arg(3).str().map_or(0, str::len));
                }
                for c in calls.iter().filter(|c| c.name == "taskExists" || c.name == "removeTask" || c.name == "updateSyncGroupForThreadType") {
                    eprintln!("  {} {:?}", c.name, c.args);
                }
                for c in calls.iter().filter(|c| c.name == "upsertMessage" || c.name == "insertMessage") {
                    eprintln!("  {} thread={} age={}s", c.name, c.arg(3).int(), (now_ms() - c.arg(5).int()) / 1000);
                }
            }
            let failed = calls.iter().any(|c| c.name == "handleSyncFailure");
            // A sync can bring a chat's newest messages while we hold older ones
            // from the cache; whatever arrived in between is missing. Fill the
            // gap by fetching the messages just before the new range.
            let gaps: Vec<(i64, i64, String)> = {
                let st = self.shared.store.lock().unwrap();
                calls
                    .iter()
                    .filter(|c| c.name == "insertNewMessageRange")
                    .filter_map(|c| {
                        // (threadKey, minTimestampMs, maxTimestampMs, minMessageId, ...)
                        let (thread, min_ts, min_id) = (c.arg(0).int(), c.arg(1).int(), c.arg(3).string());
                        let newest_known = st.messages.get(&thread)?.iter().filter(|m| !m.pending).map(|m| m.timestamp_ms).max()?;
                        (newest_known < min_ts && !min_id.is_empty()).then_some((thread, min_ts, min_id))
                    })
                    .take(10)
                    .collect()
            };
            self.shared.store.lock().unwrap().apply(&calls);
            for (thread, ts, mid) in gaps {
                if std::env::var_os("MESSENGER_DEBUG").is_some() {
                    eprintln!("[gap] {thread}: fetching messages before the new range");
                }
                self.load_before(thread, ts, mid);
            }
            (self.repaint)();
            return failed;
        }
        false
    }

    /// Run a Lightspeed request over HTTP (GraphQL; type 3 = tasks, 4 =
    /// typing) and apply its answer. The changes a task makes (the sent
    /// message, its attachments) aren't in that answer and DGW doesn't always
    /// push them, so tasks are followed by a delta sync of database 1 from our
    /// cursor, as `load_older` does.
    fn send_request_http(&self, kind: i64, payload: String) {
        let (session, shared, repaint) = (self.session.clone(), self.shared.clone(), self.repaint.clone());
        tokio::spawn(async move {
            // Typing (type 4) and some tasks answer with an empty payload.
            let result = session.ls_request(kind, payload).await.and_then(|ls| if ls.trim().is_empty() { Ok(Vec::new()) } else { decode_payload(&ls) });
            if std::env::var_os("MESSENGER_DEBUG").is_some() && kind == 3 {
                match &result {
                    Ok(calls) => {
                        let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
                        eprintln!("[task] answer {names:?}");
                        // Shape of edit answers: strings as their length only.
                        for c in calls.iter().filter(|c| c.name == "editMessage" || c.name == "taskExists" || c.name == "removeTask") {
                            let shape: Vec<String> = c.args.iter().map(|a| match a {
                                crate::lightspeed::Arg::Str(s) => format!("str({})", s.chars().count()),
                                other => format!("{other:?}"),
                            }).collect();
                            eprintln!("  {} {shape:?}", c.name);
                        }
                    }
                    Err(e) => eprintln!("[task] {e:#}"),
                }
            }
            match result {
                Ok(calls) => {
                    shared.store.lock().unwrap().apply(&calls);
                    repaint();
                    if kind == 3 {
                        match http_delta(&session, &shared).await {
                            Ok(calls) => {
                                shared.store.lock().unwrap().apply(&calls);
                                repaint();
                            }
                            Err(e) => eprintln!("[command] sync after task failed: {e:#}"),
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[command] task failed: {e:#}");
                    *shared.error.lock().unwrap() = Some(format!("Couldn't complete that: {e}"));
                    repaint();
                }
            }
        });
    }

    /// Delta sync of database 1 from our cursor over HTTP; one at a time.
    fn poll_mailbox(&self) {
        if self.shared.polling.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let (session, shared, repaint) = (self.session.clone(), self.shared.clone(), self.repaint.clone());
        tokio::spawn(async move {
            match http_delta(&session, &shared).await {
                Ok(calls) => {
                    let changed = calls.iter().any(|c| !c.name.starts_with("execute") && c.name != "setRegionHint" && !c.name.starts_with("updateLastSync"));
                    shared.store.lock().unwrap().apply(&calls);
                    if changed {
                        if std::env::var_os("MESSENGER_DEBUG").is_some() {
                            let mut names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
                            names.dedup();
                            eprintln!("[poll] {names:?}");
                        }
                        repaint();
                    }
                }
                Err(e) => eprintln!("[poll] {e:#}"),
            }
            shared.polling.store(false, std::sync::atomic::Ordering::Release);
        });
    }

    /// Fetch the sticker tray (`None`) or one pack's stickers, once.
    fn load_stickers(&self, pack: Option<String>) {
        let key = pack.clone().unwrap_or_default();
        if !self.shared.stickers.lock().unwrap().loading.insert(key.clone()) {
            return;
        }
        let (session, shared, repaint) = (self.session.clone(), self.shared.clone(), self.repaint.clone());
        tokio::spawn(async move {
            match &pack {
                None => match session.sticker_packs().await {
                    Ok(packs) => shared.stickers.lock().unwrap().packs = Some(packs),
                    Err(e) => {
                        // Empty rather than None, so the UI doesn't ask again every frame.
                        eprintln!("[stickers] tray: {e:#}");
                        shared.stickers.lock().unwrap().packs = Some(Vec::new());
                    }
                },
                Some(id) => match session.sticker_pack(id).await {
                    Ok(list) => {
                        shared.stickers.lock().unwrap().pack_stickers.insert(id.clone(), list);
                    }
                    Err(e) => {
                        eprintln!("[stickers] pack {id}: {e:#}");
                        shared.stickers.lock().unwrap().pack_stickers.insert(id.clone(), Vec::new());
                    }
                },
            }
            shared.stickers.lock().unwrap().loading.remove(&key);
            repaint();
        });
    }

    /// Fetch the 20 messages before the oldest one we have. Runs over HTTP, as
    /// a FetchMessages task (228) followed by a delta sync of database 1 from
    /// our cursor: the delta carries the fetched range. (Over DGW the server
    /// drops the connection instead of pushing the result.)
    fn load_older(&self, thread: i64) {
        let oldest = {
            let st = self.shared.store.lock().unwrap();
            st.messages.get(&thread).and_then(|v| v.iter().find(|m| !m.pending)).map(|m| (m.timestamp_ms, m.id.clone()))
        };
        let (ts, mid) = oldest.unwrap_or((now_ms(), String::new()));
        self.load_before(thread, ts, mid);
    }

    /// Fetch the 20 messages before (`ts`, `mid`) in `thread` (see `load_older`).
    fn load_before(&self, thread: i64, ts: i64, mid: String) {
        let cursor = self.shared.http_cursor.lock().unwrap().clone();
        let task = json!({
            "thread_key": thread, "direction": 0, "reference_timestamp_ms": ts,
            "reference_message_id": mid, "sync_group": 1, "cursor": cursor,
        });
        let (session, shared, repaint) = (self.session.clone(), self.shared.clone(), self.repaint.clone());
        tokio::spawn(async move {
            let result = async {
                session.ls_tasks(&[("228", task, format!("mrq.{thread}"))]).await?;
                http_delta(&session, &shared).await
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
                let otid = new_otid();
                let shown = if paths.len() == 1 { "Sending a file…".to_string() } else { format!("Sending {} files…", paths.len()) };
                self.shared.store.lock().unwrap().add_pending(thread, otid, shown, None);
                (self.repaint)();
                let mut ids = Vec::new();
                for path in &paths {
                    match self.session.upload(path).await {
                        Ok(id) => ids.push(id),
                        Err(e) => {
                            self.shared.store.lock().unwrap().remove_pending(thread, otid);
                            (self.repaint)();
                            return Err(e);
                        }
                    }
                }
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
                // Show it right away; the server's editMessage agrees.
                if let Some(m) = self.shared.store.lock().unwrap().find_message_mut(None, &message_id) {
                    m.text = text.clone();
                    m.edited = true;
                }
                (self.repaint)();
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
            Command::LoadStickerPacks => {
                self.load_stickers(None);
                return Ok(None);
            }
            Command::LoadStickerPack { pack } => {
                self.load_stickers(Some(pack));
                return Ok(None);
            }
            Command::SendSticker { thread, sticker_id } => {
                let otid = new_otid();
                self.shared.store.lock().unwrap().add_pending(thread, otid, "Sticker".into(), None);
                (self.repaint)();
                // A sticker is a send with send_type 2 and the sticker's id.
                let p = json!({
                    "thread_id": thread, "otid": otid.to_string(), "source": 65537, "send_type": 2,
                    "sync_group": 1, "sticker_id": sticker_id, "initiating_source": 1,
                    "skip_url_preview_gen": 0, "text_has_links": 0, "multitab_env": 0,
                });
                let read = json!({"thread_id": thread, "last_read_watermark_ts": now_ms(), "sync_group": 1});
                self.tasks(vec![("46", p, thread.to_string()), ("21", read, thread.to_string())])
            }
            Command::RefreshRecent { thread } => {
                // The 20 messages before our newest one (the server needs a real
                // message as the reference).
                let newest = {
                    let st = self.shared.store.lock().unwrap();
                    st.messages.get(&thread).and_then(|v| v.iter().rev().find(|m| !m.pending && m.id.starts_with("mid."))).map(|m| (m.timestamp_ms, m.id.clone()))
                };
                if let Some((ts, mid)) = newest {
                    self.load_before(thread, ts, mid);
                }
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

impl Shared {
    /// Remember the database 1 cursor an HTTP sync answered with.
    fn note_http_cursor(&self, calls: &[crate::lightspeed::Call]) {
        let cursor = calls
            .iter()
            .find(|c| c.name == "executeFirstBlockForSyncTransactionV4" && c.arg(0).int() == 1)
            .and_then(|c| c.arg(3).str().map(str::to_string));
        if let Some(cursor) = cursor {
            *self.http_cursor.lock().unwrap() = Some(cursor);
        }
    }
}

/// Delta sync of database 1 over HTTP from the HTTP cursor. If the server
/// rejects the cursor, start over from a snapshot (which carries a new one).
async fn http_delta(session: &Session, shared: &Shared) -> Result<Vec<crate::lightspeed::Call>> {
    let cursor = shared.http_cursor.lock().unwrap().clone();
    let mut calls = decode_payload(&session.ls_sync(1, cursor.as_deref()).await?)?;
    if cursor.is_some() && calls.iter().any(|c| c.name == "handleSyncFailure") {
        eprintln!("[sync] HTTP cursor rejected; taking a snapshot");
        calls = decode_payload(&session.ls_sync(1, None).await?)?;
    }
    shared.note_http_cursor(&calls);
    Ok(calls)
}

/// Request type and payload of a Lightspeed request body (`{"payload", "type"}`).
fn lightspeed_request(body: &[u8]) -> Option<(i64, String)> {
    let v: Value = serde_json::from_slice(body).ok()?;
    Some((v["type"].as_i64()?, v["payload"].as_str()?.to_string()))
}

/// `ls` with each string literal that isn't an identifier replaced by "…".
fn regex_lite_mask(ls: &str) -> String {
    let mut out = String::new();
    let mut chars = ls.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '"' {
            out.push(c);
            continue;
        }
        let mut lit = String::new();
        let mut escaped = false;
        for d in chars.by_ref() {
            if escaped {
                lit.push(d);
                escaped = false;
            } else if d == '\\' {
                escaped = true;
            } else if d == '"' {
                break;
            } else {
                lit.push(d);
            }
        }
        if !lit.is_empty() && lit.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') && lit.chars().next().is_some_and(|ch| ch.is_ascii_alphabetic()) {
            out.push('"');
            out.push_str(&lit);
            out.push('"');
        } else {
            out.push_str("\"…\"");
        }
    }
    out
}
