//! E2EE connection loop: keep one-time prekeys stocked on the server, decrypt
//! incoming `<message>` nodes with the Signal store, persist the plaintext,
//! then ack. Also sends text to 1:1 chats (phase 4, see docs/E2EE.md): one
//! Signal ciphertext per device of the peer and of our own other devices.

use std::collections::HashMap;
use std::io::Write;

use anyhow::{anyhow, bail, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use wacore_binary::builder::NodeBuilder;
use wacore_binary::node::{Node, NodeContentRef, NodeRef};
use wacore_libsignal::protocol::{
    group_decrypt, message_decrypt, message_encrypt, process_prekey_bundle, process_sender_key_distribution_message,
    CiphertextMessage, CiphertextMessageType, DeviceId, IdentityKey, PreKeyBundle, PreKeyId, PreKeySignalMessage,
    ProtocolAddress, PublicKey, SenderKeyDistributionMessage, SenderKeyName, SessionStore, SignalMessage,
    SignedPreKeyId, UsePQRatchet,
};

use crate::e2ee_socket::ChatSocket;
use crate::e2ee_store::{data_dir, SignalStore};

const SERVER: &str = "s.whatsapp.net";
/// Top up when the server holds fewer than this many of our one-time prekeys.
const MIN_PREKEYS: u32 = 50;
const UPLOAD_BATCH: u32 = 100;

/// One decrypted E2EE payload, appended to `e2ee-inbox.jsonl` before acking.
#[derive(Serialize, Deserialize)]
pub struct Received {
    pub id: String,
    pub from: String,
    pub participant: Option<String>,
    pub timestamp: i64,
    pub enc_type: String,
    /// Raw Signal plaintext (Armadillo `MessageTransport` protobuf), base64.
    pub plaintext: String,
}

pub struct Listener {
    pub sock: ChatSocket,
    pub store: SignalStore,
    /// Our fbid, to tell messages sent from our other devices apart.
    own_user: String,
    iq_counter: u64,
    /// Retry receipts sent per message id (capped at MAX_RETRIES).
    retries: HashMap<String, u32>,
    receipts: Vec<Receipt>,
}

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub enum ReceiptKind {
    Delivered,
    Read,
}

/// Someone else's device got (or read) our messages `ids`.
#[derive(Debug)]
pub struct Receipt {
    pub ids: Vec<String>,
    pub user: i64,
    pub kind: ReceiptKind,
}

const MAX_RETRIES: u32 = 5;

/// Where E2EE media lives: hostnames for `https://<host><direct path>` and
/// the upload auth token, valid for `ttl_secs`.
#[derive(Clone, Debug)]
pub struct MediaConn {
    pub auth: String,
    pub ttl_secs: u64,
    pub hosts: Vec<String>,
    pub fetched: std::time::Instant,
}

impl MediaConn {
    pub fn fresh(&self) -> bool {
        self.fetched.elapsed().as_secs() + 30 < self.ttl_secs
    }
}

/// Something to send to the 1:1 E2EE chat with `peer` (fbid); `done` gets the outcome.
pub struct Outgoing {
    pub peer: i64,
    pub payload: OutgoingPayload,
    pub done: tokio::sync::oneshot::Sender<Result<(), String>>,
}

pub enum OutgoingPayload {
    Text(String),
    /// A picture file to encrypt, upload and send.
    Image(std::path::PathBuf),
    /// A sticker picture (link) to download, encrypt, upload and send as a
    /// sticker, with its accessibility label.
    Sticker { url: String, label: String },
    /// Read receipts for the peer's messages `ids` (newest first).
    Read(Vec<String>),
}

/// Messenger message ids: milliseconds since the epoch shifted left 22 bits,
/// plus 22 random bits (same shape as the ids we receive).
fn new_message_id() -> String {
    let ms = crate::store::now_ms() as u64;
    ((ms << 22) | (rand::random::<u64>() & ((1 << 22) - 1))).to_string()
}

fn be_uint(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0, |acc, b| (acc << 8) | *b as u32)
}

/// `ADVSignedDeviceIdentity`. Messenger devices registered via ICDC have no ADV
/// account signature, so (like the reference clients) we send zero-filled
/// placeholders.
#[derive(Clone, PartialEq, prost::Message)]
struct AdvSignedDeviceIdentity {
    #[prost(bytes = "vec", optional, tag = "1")]
    details: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    account_signature_key: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "3")]
    account_signature: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "4")]
    device_signature: Option<Vec<u8>>,
}

/// JID-valued attribute: encoded as a binary JID (keeps `:device` for msgr
/// device JIDs, which a plain string attribute would lose).
fn jid_value(s: &str) -> wacore_binary::node::NodeValue {
    match s.parse::<wacore_binary::jid::Jid>() {
        Ok(jid) => jid.into(),
        Err(_) => s.to_string().into(),
    }
}

fn attr(node: &NodeRef<'_>, key: &str) -> Option<String> {
    node.get_attr(key).map(|v| v.to_string())
}

fn child<'a, 'b>(node: &'b NodeRef<'a>, tag: &str) -> Option<&'b NodeRef<'a>> {
    node.children()?.iter().find(|c| c.tag == tag)
}

fn bytes_of(node: &NodeRef<'_>) -> Option<Vec<u8>> {
    match node.content.as_ref()? {
        NodeContentRef::Bytes(b) => Some(b.to_vec()),
        NodeContentRef::String(s) => Some(s.as_bytes().to_vec()),
        NodeContentRef::Nodes(_) => None,
    }
}

/// Split a JID like `123.4@msgr` / `123:4@s.whatsapp.net` into a Signal address
/// (user, device). Messenger device JIDs use `.`; WhatsApp-style ones use `:`.
pub fn signal_address(jid: &str) -> Result<ProtocolAddress> {
    let (user_part, _server) = jid.split_once('@').ok_or_else(|| anyhow!("bad jid {jid}"))?;
    let (user, device) = match user_part.rsplit_once(['.', ':']) {
        Some((u, d)) if d.chars().all(|c| c.is_ascii_digit()) => (u, d.parse::<u32>()?),
        _ => (user_part, 0),
    };
    Ok(ProtocolAddress::new(user, DeviceId::from(device)))
}

impl Listener {
    pub fn new(sock: ChatSocket, store: SignalStore, own_fbid: i64) -> Self {
        Listener { sock, store, own_user: own_fbid.to_string(), iq_counter: 0, retries: HashMap::new(), receipts: Vec::new() }
    }

    fn next_iq_id(&mut self) -> String {
        self.iq_counter += 1;
        format!("mdesk.{}", self.iq_counter)
    }

    async fn send_node(&mut self, node: &Node) -> Result<()> {
        let bytes = wacore_binary::marshal::marshal(node).map_err(|e| anyhow!("marshal: {e:?}"))?;
        if std::env::var_os("MESSENGER_DEBUG").is_some() {
            let shown = crate::e2ee_socket::describe(&bytes[1..]);
            eprintln!("  → {}", shown.chars().take(200).collect::<String>());
        }
        self.sock.send(&bytes).await
    }

    /// Send an IQ and wait for its result, handling anything else that arrives
    /// in between (messages are queued for the caller to process).
    async fn iq(&mut self, xmlns: &'static str, kind: &'static str, children: Vec<Node>, backlog: &mut Vec<Vec<u8>>) -> Result<Vec<u8>> {
        let id = self.next_iq_id();
        let node = NodeBuilder::new("iq")
            .attr("id", id.clone())
            .attr("xmlns", xmlns)
            .attr("type", kind)
            .attr("to", SERVER)
            .children(children)
            .build();
        self.send_node(&node).await?;
        loop {
            let Some(bytes) = self.sock.recv().await? else { bail!("socket closed waiting for iq {id}") };
            let node = wacore_binary::marshal::unmarshal_ref(&bytes).map_err(|e| anyhow!("{e:?}"))?;
            if node.tag == "iq" && attr(&node, "id").as_deref() == Some(id.as_str()) {
                if attr(&node, "type").as_deref() == Some("error") {
                    bail!("iq {xmlns}/{kind} failed: {}", crate::e2ee_socket::describe(&bytes));
                }
                return Ok(bytes);
            }
            backlog.push(bytes);
        }
    }

    /// Tell the server this client is active (starts normal delivery).
    pub async fn set_active(&mut self, backlog: &mut Vec<Vec<u8>>) -> Result<()> {
        self.iq("passive", "set", vec![NodeBuilder::new("active").build()], backlog).await.map(|_| ())
    }

    /// Keepalive ping; the reply is handled (ignored) by the receive loop.
    pub async fn ping(&mut self) -> Result<()> {
        let id = self.next_iq_id();
        let node = NodeBuilder::new("iq")
            .attr("id", id)
            .attr("xmlns", "w:p")
            .attr("type", "get")
            .attr("to", SERVER)
            .children([NodeBuilder::new("ping").build()])
            .build();
        self.send_node(&node).await
    }

    /// Read-only: the server's Messenger device list for the given user JIDs
    /// (`<iq xmlns="fbid:devices"><users><user jid/></users>`).
    pub async fn query_devices(&mut self, users: &[&str], backlog: &mut Vec<Vec<u8>>) -> Result<Vec<u8>> {
        let list: Vec<Node> = users.iter().map(|u| NodeBuilder::new("user").attr("jid", jid_value(u)).build()).collect();
        self.iq("fbid:devices", "get", vec![NodeBuilder::new("users").children(list).build()], backlog).await
    }

    /// Device JIDs (`user:device@msgr`) of the given users, from `fbid:devices`.
    async fn devices_of(&mut self, users: &[&str], backlog: &mut Vec<Vec<u8>>) -> Result<Vec<String>> {
        let resp = self.query_devices(users, backlog).await?;
        let node = wacore_binary::marshal::unmarshal_ref(&resp).map_err(|e| anyhow!("{e:?}"))?;
        let mut out = Vec::new();
        for user in child(&node, "users").and_then(|u| u.children()).into_iter().flatten() {
            let Some(jid) = attr(user, "jid") else { continue };
            let Some((name, _)) = jid.split_once('@') else { continue };
            for device in child(user, "devices").and_then(|d| d.children()).into_iter().flatten() {
                if let Some(id) = attr(device, "id") {
                    out.push(format!("{name}:{id}@msgr"));
                }
            }
        }
        Ok(out)
    }

    /// Start Signal sessions with the devices we have none with yet, from
    /// prekey bundles (`<iq xmlns="encrypt" type="get"><key><user jid/>`).
    /// Returns the devices we can encrypt for.
    async fn ensure_sessions(&mut self, devices: &[String], backlog: &mut Vec<Vec<u8>>) -> Result<Vec<String>> {
        let mut missing = Vec::new();
        for d in devices {
            if self.store.load_session(&signal_address(d)?).await?.is_none() {
                missing.push(d.clone());
            }
        }
        if !missing.is_empty() {
            let users: Vec<Node> = missing.iter().map(|d| NodeBuilder::new("user").attr("jid", jid_value(d)).build()).collect();
            let resp = self.iq("encrypt", "get", vec![NodeBuilder::new("key").children(users).build()], backlog).await?;
            let node = wacore_binary::marshal::unmarshal_ref(&resp).map_err(|e| anyhow!("{e:?}"))?;
            for user in child(&node, "list").and_then(|l| l.children()).into_iter().flatten() {
                let jid = attr(user, "jid").unwrap_or_default();
                if let Err(e) = self.process_bundle(user).await {
                    eprintln!("[e2ee] no session for {jid}: {e:#}");
                }
            }
        }
        let mut ready = Vec::new();
        for d in devices {
            if self.store.load_session(&signal_address(d)?).await?.is_some() {
                ready.push(d.clone());
            }
        }
        Ok(ready)
    }

    async fn process_bundle(&mut self, user: &NodeRef<'_>) -> Result<()> {
        if child(user, "error").is_some() {
            bail!("server returned an error instead of keys");
        }
        let jid = attr(user, "jid").ok_or_else(|| anyhow!("bundle without jid"))?;
        let field = |n: &NodeRef<'_>, tag: &str| child(n, tag).and_then(bytes_of).ok_or_else(|| anyhow!("bundle missing {tag}"));
        let registration = be_uint(&field(user, "registration")?);
        let identity = IdentityKey::new(PublicKey::from_djb_public_key_bytes(&field(user, "identity")?)?);
        let skey = child(user, "skey").ok_or_else(|| anyhow!("bundle missing skey"))?;
        let prekey = match child(user, "key") {
            Some(k) => Some((PreKeyId::from(be_uint(&field(k, "id")?)), PublicKey::from_djb_public_key_bytes(&field(k, "value")?)?)),
            None => None,
        };
        let address = signal_address(&jid)?;
        let bundle = PreKeyBundle::new(
            registration,
            address.device_id(),
            prekey,
            SignedPreKeyId::from(be_uint(&field(skey, "id")?)),
            PublicKey::from_djb_public_key_bytes(&field(skey, "value")?)?,
            field(skey, "signature")?,
            identity,
        )?;
        let (mut sessions, mut identities) = (self.store.clone(), self.store.clone());
        let mut rng = <rand::rngs::StdRng as rand::SeedableRng>::from_rng(&mut rand::rng());
        process_prekey_bundle(&address, &mut sessions, &mut identities, &bundle, &mut rng, UsePQRatchet::No).await?;
        Ok(())
    }

    /// Tell the peer we read their messages `ids`: one `receipt type="read"`
    /// for the first id, the others in a `<list>` (the WhatsApp/Armadillo
    /// shape, as the web client sends when a chat is open).
    pub async fn send_read(&mut self, peer: i64, ids: &[String]) -> Result<()> {
        let Some((first, rest)) = ids.split_first() else { return Ok(()) };
        let t = (crate::store::now_ms() / 1000).to_string();
        let mut b = NodeBuilder::new("receipt").attr("id", first.clone()).attr("to", jid_value(&format!("{peer}@msgr"))).attr("type", "read").attr("t", t);
        if !rest.is_empty() {
            let items = rest.iter().map(|id| NodeBuilder::new("item").attr("id", id.clone()).build());
            b = b.children([NodeBuilder::new("list").children(items).build()]);
        }
        self.send_node(&b.build()).await
    }

    pub async fn send_text(&mut self, peer: i64, text: &str, backlog: &mut Vec<Vec<u8>>) -> Result<Received> {
        self.send_application(peer, crate::armadillo::encode_text(text), backlog).await
    }

    /// Encrypt a picture, upload it (`e2ee_media::upload`) and send it. A plain
    /// copy goes into the media cache so our own message shows the picture.
    pub async fn send_image(
        &mut self,
        peer: i64,
        path: &std::path::Path,
        http: &reqwest::Client,
        media_conn: &mut Option<MediaConn>,
        backlog: &mut Vec<Vec<u8>>,
    ) -> Result<Received> {
        use crate::e2ee_media::{self, MediaType};
        let img = e2ee_media::prepare_image(path)?;
        let enc = e2ee_media::encrypt(&img.bytes, MediaType::Image)?;
        if !media_conn.as_ref().is_some_and(MediaConn::fresh) {
            *media_conn = Some(self.media_conn(backlog).await?);
        }
        let conn = media_conn.as_ref().expect("just fetched");
        let uploaded = e2ee_media::upload(http, &conn.hosts, &conn.auth, MediaType::Image, &enc).await?;
        let file = crate::armadillo::MediaFile {
            direct_path: uploaded.direct_path,
            media_key: enc.media_key.to_vec(),
            file_sha256: enc.file_sha256.to_vec(),
            file_enc_sha256: enc.file_enc_sha256.to_vec(),
            mimetype: img.mimetype,
            file_length: img.bytes.len() as u64,
            width: img.width,
            height: img.height,
            thumbnail: img.thumbnail,
        };
        if let Some(cached) = e2ee_media::cached_path(&file) {
            e2ee_media::write_private(&cached, &img.bytes)?;
        }
        let out = crate::armadillo::encode_image(&file, uploaded.object_id.as_deref(), crate::store::now_ms() / 1000, "");
        self.send_application(peer, out, backlog).await
    }

    /// Send a sticker: its picture (WebP, often animated) encrypted and
    /// uploaded like an image, in an Armadillo sticker message.
    pub async fn send_sticker(
        &mut self,
        peer: i64,
        url: &str,
        label: &str,
        http: &reqwest::Client,
        media_conn: &mut Option<MediaConn>,
        backlog: &mut Vec<Vec<u8>>,
    ) -> Result<Received> {
        use crate::e2ee_media::{self, MediaType};
        let bytes = http.get(url).send().await?.error_for_status()?.bytes().await?.to_vec();
        let format = image::guess_format(&bytes).map_err(|_| anyhow!("the sticker isn't a picture"))?;
        let (width, height) = image::load_from_memory_with_format(&bytes, format).map(|i| (i.width(), i.height())).unwrap_or((512, 512));
        // Animated WebP has an ANIM chunk.
        let animated = bytes.windows(4).take(256).any(|w| w == b"ANIM");
        let enc = e2ee_media::encrypt(&bytes, MediaType::Image)?;
        if !media_conn.as_ref().is_some_and(MediaConn::fresh) {
            *media_conn = Some(self.media_conn(backlog).await?);
        }
        let conn = media_conn.as_ref().expect("just fetched");
        let uploaded = e2ee_media::upload(http, &conn.hosts, &conn.auth, MediaType::Image, &enc).await?;
        let file = crate::armadillo::MediaFile {
            direct_path: uploaded.direct_path,
            media_key: enc.media_key.to_vec(),
            file_sha256: enc.file_sha256.to_vec(),
            file_enc_sha256: enc.file_enc_sha256.to_vec(),
            mimetype: format.to_mime_type().to_string(),
            file_length: bytes.len() as u64,
            width,
            height,
            thumbnail: Vec::new(),
        };
        if let Some(cached) = e2ee_media::cached_path(&file) {
            e2ee_media::write_private(&cached, &bytes)?;
        }
        let out = crate::armadillo::encode_sticker(&file, uploaded.object_id.as_deref(), crate::store::now_ms() / 1000, animated, label);
        self.send_application(peer, out, backlog).await
    }

    /// Encrypt a message (`armadillo::Outgoing`) for every device of `peer`
    /// and of our other devices, send it, wait for the server's ack, and log
    /// it to the inbox as our own message (so it shows up again after a
    /// restart). Returns that inbox record.
    async fn send_application(&mut self, peer: i64, out: crate::armadillo::Outgoing, backlog: &mut Vec<Vec<u8>>) -> Result<Received> {
        let own_device = crate::e2ee::Device::load().and_then(|d| d.wa_device_id).ok_or_else(|| anyhow!("device missing"))?;
        let own_jid = format!("{}:{own_device}@msgr", self.own_user);
        let peer_jid = format!("{peer}@msgr");
        let own_user_jid = format!("{}@msgr", self.own_user);
        let devices: Vec<String> = self
            .devices_of(&[peer_jid.as_str(), own_user_jid.as_str()], backlog)
            .await?
            .into_iter()
            .filter(|d| d != &own_jid)
            .collect();
        let devices = self.ensure_sessions(&devices, backlog).await?;
        let peer_prefix = format!("{peer}:");
        if !devices.iter().any(|d| d.starts_with(&peer_prefix)) {
            bail!("no encryption session with any of the recipient's devices");
        }

        let id = new_message_id();
        let mut participants = Vec::new();
        for d in &devices {
            let address = signal_address(d)?;
            let mine = address.name() == self.own_user;
            let plaintext = crate::armadillo::encode_transport(&out.application, &id, mine.then_some(peer_jid.as_str()));
            let (mut sessions, mut identities) = (self.store.clone(), self.store.clone());
            let ciphertext = message_encrypt(&plaintext, &address, &mut sessions, &mut identities).await?;
            let kind = if ciphertext.message_type() == CiphertextMessageType::PreKey { "pkmsg" } else { "msg" };
            let enc = NodeBuilder::new("enc").attr("v", "3").attr("type", kind).bytes(ciphertext.serialize().to_vec()).build();
            participants.push(NodeBuilder::new("to").attr("jid", jid_value(d)).children([enc]).build());
        }
        let node = NodeBuilder::new("message")
            .attr("id", id.clone())
            .attr("to", jid_value(&peer_jid))
            .attr("type", "text")
            .children([
                NodeBuilder::new("participants").children(participants).build(),
                NodeBuilder::new("franking")
                    .children([NodeBuilder::new("franking_tag").bytes(out.franking_tag.clone()).build()])
                    .build(),
            ])
            .build();
        self.send_node(&node).await?;

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        let timestamp = loop {
            let bytes = match tokio::time::timeout_at(deadline, self.sock.recv()).await {
                Ok(r) => r?.ok_or_else(|| anyhow!("socket closed waiting for the send ack"))?,
                Err(_) => bail!("no ack from the server within 20s"),
            };
            let reply = wacore_binary::marshal::unmarshal_ref(&bytes).map_err(|e| anyhow!("{e:?}"))?;
            if reply.tag == "ack" && attr(&reply, "id").as_deref() == Some(id.as_str()) {
                if let Some(err) = attr(&reply, "error") {
                    bail!("server rejected the message (error {err})");
                }
                break attr(&reply, "t").and_then(|t| t.parse().ok()).unwrap_or(crate::store::now_ms() / 1000);
            }
            backlog.push(bytes);
        };
        eprintln!("[e2ee] sent {id} to {} device(s)", devices.len());

        let rec = Received {
            id,
            from: own_jid,
            participant: None,
            timestamp,
            enc_type: "sent".into(),
            plaintext: STANDARD.encode(crate::armadillo::encode_transport(&out.application, "", Some(&peer_jid))),
        };
        append_inbox(&rec)?;
        Ok(rec)
    }

    /// Media upload/download hosts and the upload auth token
    /// (`<iq xmlns="w:m" type="set"><media_conn/>`).
    pub async fn media_conn(&mut self, backlog: &mut Vec<Vec<u8>>) -> Result<MediaConn> {
        let resp = self.iq("w:m", "set", vec![NodeBuilder::new("media_conn").build()], backlog).await?;
        let node = wacore_binary::marshal::unmarshal_ref(&resp).map_err(|e| anyhow!("{e:?}"))?;
        let conn = child(&node, "media_conn").ok_or_else(|| anyhow!("no media_conn in reply"))?;
        let hosts = conn
            .children()
            .into_iter()
            .flatten()
            .filter(|c| c.tag == "host")
            .filter_map(|h| attr(h, "hostname"))
            .collect::<Vec<_>>();
        if hosts.is_empty() {
            bail!("media_conn without hosts");
        }
        Ok(MediaConn {
            auth: attr(conn, "auth").unwrap_or_default(),
            ttl_secs: attr(conn, "ttl").and_then(|t| t.parse().ok()).unwrap_or(300),
            hosts,
            fetched: std::time::Instant::now(),
        })
    }

    /// Make sure the server holds enough of our one-time prekeys.
    pub async fn ensure_prekeys(&mut self, backlog: &mut Vec<Vec<u8>>) -> Result<(u32, u32)> {
        let resp = self.iq("encrypt", "get", vec![NodeBuilder::new("count").build()], backlog).await?;
        let node = wacore_binary::marshal::unmarshal_ref(&resp).map_err(|e| anyhow!("{e:?}"))?;
        let count: u32 = child(&node, "count").and_then(|c| attr(c, "value")).and_then(|v| v.parse().ok()).unwrap_or(0);
        if count >= MIN_PREKEYS {
            return Ok((count, 0));
        }
        let keys = self.store.generate_prekeys(UPLOAD_BATCH)?;
        let list: Vec<Node> = keys
            .iter()
            .map(|(id, public)| {
                NodeBuilder::new("key")
                    .children([
                        NodeBuilder::new("id").bytes(id.to_be_bytes()[1..].to_vec()).build(),
                        NodeBuilder::new("value").bytes(public.to_vec()).build(),
                    ])
                    .build()
            })
            .collect();
        let device = crate::e2ee::Device::load().ok_or_else(|| anyhow!("device missing"))?;
        let skey = NodeBuilder::new("skey")
            .children([
                NodeBuilder::new("id").bytes(device.signed_prekey_id.to_be_bytes()[1..].to_vec()).build(),
                NodeBuilder::new("value").bytes(device.signed_prekey_pub.to_vec()).build(),
                NodeBuilder::new("signature").bytes(device.signed_prekey_sig.to_vec()).build(),
            ])
            .build();
        let children = vec![
            NodeBuilder::new("registration").bytes(self.store.registration_id().to_be_bytes().to_vec()).build(),
            NodeBuilder::new("type").bytes(vec![0x05]).build(),
            NodeBuilder::new("identity").bytes(device.identity_pub.to_vec()).build(),
            NodeBuilder::new("list").children(list).build(),
            skey,
        ];
        self.iq("encrypt", "set", children, backlog).await?;
        if let Some((last, _)) = keys.last() {
            self.store.mark_uploaded(*last)?;
        }
        Ok((count, keys.len() as u32))
    }

    /// Handle one incoming node. Returns decrypted messages (already persisted and acked).
    pub async fn handle(&mut self, bytes: &[u8]) -> Result<Vec<Received>> {
        let node = wacore_binary::marshal::unmarshal_ref(bytes).map_err(|e| anyhow!("{e:?}"))?;
        let tag = node.tag.to_string();
        match tag.as_str() {
            "message" => {
                let received = self.decrypt_message(&node).await;
                // Only after the plaintext is on disk: a delivery receipt for what we
                // decrypted (it doubles as the ack), a plain ack otherwise
                // (e.g. `<unavailable>`: the sender didn't encrypt for this device).
                let has_enc = node.children().is_some_and(|c| c.iter().any(|n| n.tag == "enc"));
                let unavailable = node.children().is_some_and(|c| c.iter().any(|n| n.tag == "unavailable"));
                match &received {
                    Ok(msgs) if !msgs.is_empty() => self.receipt(&node).await?,
                    // Not encrypted for this device, or decryption failed: ask the
                    // sender to re-encrypt for us (with our keys), then ack.
                    Ok(_) if unavailable && !has_enc => {
                        self.retry_receipt(&node).await?;
                        self.ack(&node, "message").await?;
                    }
                    Err(e) if has_enc => {
                        eprintln!("[e2ee] decrypt failed for {}: {e:#}", attr(&node, "id").unwrap_or_default());
                        self.retry_receipt(&node).await?;
                        self.ack(&node, "message").await?;
                    }
                    _ => self.ack(&node, "message").await?,
                }
                received
            }
            "notification" | "receipt" => {
                if tag == "receipt" {
                    self.note_receipt(&node);
                }
                self.ack(&node, &tag).await?;
                Ok(vec![])
            }
            "iq" if attr(&node, "type").as_deref() == Some("get") => {
                // Server ping: answer with an empty result.
                if let Some(id) = attr(&node, "id") {
                    let pong = NodeBuilder::new("iq").attr("id", id).attr("type", "result").attr("to", SERVER).build();
                    self.send_node(&pong).await?;
                }
                Ok(vec![])
            }
            _ => Ok(vec![]),
        }
    }

    async fn ack(&mut self, node: &NodeRef<'_>, class: &str) -> Result<()> {
        let (Some(id), Some(from)) = (attr(node, "id"), attr(node, "from")) else { return Ok(()) };
        let mut b = NodeBuilder::new("ack").attr("class", class.to_string()).attr("id", id).attr("to", jid_value(&from));
        if let Some(p) = attr(node, "participant") {
            b = b.attr("participant", jid_value(&p));
        }
        if let Some(r) = attr(node, "recipient") {
            b = b.attr("recipient", jid_value(&r));
        }
        if class != "message" {
            if let Some(t) = attr(node, "type") {
                b = b.attr("type", t);
            }
        }
        self.send_node(&b.build()).await
    }

    /// Remember a delivery/read receipt from someone else for the app (see
    /// `take_receipts`). Ours (`type="sender"`, other own devices) and retries
    /// aren't status updates.
    fn note_receipt(&mut self, node: &NodeRef<'_>) {
        let kind = match attr(node, "type").as_deref() {
            None | Some("") => ReceiptKind::Delivered,
            Some("read") => ReceiptKind::Read,
            _ => return,
        };
        let Some(from) = attr(node, "participant").or_else(|| attr(node, "from")) else { return };
        let Some(user) = jid_user(&from) else { return };
        if user.to_string() == self.own_user {
            return;
        }
        // One receipt can cover several messages: id plus <list><item id/>.
        let mut ids: Vec<String> = attr(node, "id").into_iter().collect();
        if let Some(list) = child(node, "list") {
            ids.extend(list.children().into_iter().flatten().filter_map(|i| attr(i, "id")));
        }
        self.receipts.push(Receipt { ids, user, kind });
    }

    /// Receipts noted since the last call.
    pub fn take_receipts(&mut self) -> Vec<Receipt> {
        std::mem::take(&mut self.receipts)
    }

    /// Delivery receipt for a decrypted message; `type="sender"` when it came
    /// from one of our own other devices.
    async fn receipt(&mut self, node: &NodeRef<'_>) -> Result<()> {
        let (Some(id), Some(from)) = (attr(node, "id"), attr(node, "from")) else { return Ok(()) };
        let sender = attr(node, "participant").unwrap_or_else(|| from.clone());
        let from_me = sender.split(['@', '.', ':']).next() == Some(self.own_user.as_str());
        let mut b = NodeBuilder::new("receipt").attr("id", id).attr("to", jid_value(&from));
        if let Some(r) = attr(node, "recipient") {
            b = b.attr("recipient", jid_value(&r));
        }
        if let Some(p) = attr(node, "participant") {
            b = b.attr("participant", jid_value(&p));
        }
        if from_me {
            b = b.attr("type", "sender");
        }
        self.send_node(&b.build()).await
    }

    /// `<receipt type="retry">`: asks the sending device to re-encrypt the
    /// message for this device. Always includes our keys, since senders whose
    /// device list predates our registration have no session or bundle for us.
    async fn retry_receipt(&mut self, node: &NodeRef<'_>) -> Result<()> {
        let (Some(id), Some(from)) = (attr(node, "id"), attr(node, "from")) else { return Ok(()) };
        let count = self.retries.entry(id.clone()).or_insert(0);
        *count += 1;
        let count = *count;
        if count > MAX_RETRIES {
            return Ok(());
        }
        let device = crate::e2ee::Device::load().ok_or_else(|| anyhow!("device missing"))?;
        let (prekey_id, prekey_pub) = self.store.generate_prekeys(1)?.into_iter().next().ok_or_else(|| anyhow!("no prekey"))?;
        let device_identity = prost::Message::encode_to_vec(&AdvSignedDeviceIdentity {
            details: Some(Vec::new()),
            account_signature_key: Some(vec![0; 32]),
            account_signature: Some(vec![0; 64]),
            device_signature: Some(vec![0; 64]),
        });

        let mut retry = NodeBuilder::new("retry").attr("count", count.to_string()).attr("id", id.clone()).attr("v", "1");
        if let Some(t) = attr(node, "t") {
            retry = retry.attr("t", t);
        }
        let keys = NodeBuilder::new("keys")
            .children([
                NodeBuilder::new("type").bytes(vec![0x05]).build(),
                NodeBuilder::new("identity").bytes(device.identity_pub.to_vec()).build(),
                NodeBuilder::new("key")
                    .children([
                        NodeBuilder::new("id").bytes(prekey_id.to_be_bytes()[1..].to_vec()).build(),
                        NodeBuilder::new("value").bytes(prekey_pub.to_vec()).build(),
                    ])
                    .build(),
                NodeBuilder::new("skey")
                    .children([
                        NodeBuilder::new("id").bytes(device.signed_prekey_id.to_be_bytes()[1..].to_vec()).build(),
                        NodeBuilder::new("value").bytes(device.signed_prekey_pub.to_vec()).build(),
                        NodeBuilder::new("signature").bytes(device.signed_prekey_sig.to_vec()).build(),
                    ])
                    .build(),
                NodeBuilder::new("device-identity").bytes(device_identity).build(),
            ])
            .build();

        let mut b = NodeBuilder::new("receipt").attr("id", id).attr("to", jid_value(&from)).attr("type", "retry");
        if let Some(r) = attr(node, "recipient") {
            b = b.attr("recipient", jid_value(&r));
        }
        if let Some(p) = attr(node, "participant") {
            b = b.attr("participant", jid_value(&p));
        }
        let receipt = b
            .children([
                retry.build(),
                NodeBuilder::new("registration").bytes(self.store.registration_id().to_be_bytes().to_vec()).build(),
                keys,
            ])
            .build();
        self.send_node(&receipt).await
    }

    async fn decrypt_message(&mut self, node: &NodeRef<'_>) -> Result<Vec<Received>> {
        let id = attr(node, "id").unwrap_or_default();
        let from = attr(node, "from").unwrap_or_default();
        let participant = attr(node, "participant");
        let timestamp: i64 = attr(node, "t").and_then(|t| t.parse().ok()).unwrap_or(0);
        // In groups the sender device is `participant`; in 1:1 it's `from`.
        let sender_jid = participant.clone().unwrap_or_else(|| from.clone());
        let sender = signal_address(&sender_jid)?;

        let mut out = Vec::new();
        let encs: Vec<&NodeRef<'_>> = node.children().map(|c| c.iter().filter(|n| n.tag == "enc").collect()).unwrap_or_default();
        for enc in encs {
            let enc_type = attr(enc, "type").unwrap_or_default();
            let Some(ciphertext) = bytes_of(enc) else { continue };
            let plaintext = match enc_type.as_str() {
                "pkmsg" | "msg" => {
                    let msg = if enc_type == "pkmsg" {
                        CiphertextMessage::PreKeySignalMessage(PreKeySignalMessage::try_from(ciphertext.as_slice())?)
                    } else {
                        CiphertextMessage::SignalMessage(SignalMessage::try_from(ciphertext.as_slice())?)
                    };
                    // One store clone per role (they share state; see SignalStore).
                    let (mut sessions, mut identities, mut prekeys, signed) =
                        (self.store.clone(), self.store.clone(), self.store.clone(), self.store.clone());
                    // StdRng (Send) rather than ThreadRng, so this future can run on a tokio task.
                    let mut rng = <rand::rngs::StdRng as rand::SeedableRng>::from_rng(&mut rand::rng());
                    let result = message_decrypt(&msg, &sender, &mut sessions, &mut identities, &mut prekeys, &signed, &mut rng, UsePQRatchet::No).await?;
                    // Drop the consumed one-time prekey only now that the session is persisted.
                    if let Some(prekey_id) = result.consumed_prekey_id {
                        use wacore_libsignal::protocol::PreKeyStore;
                        self.store.remove_pre_key(prekey_id).await?;
                    }
                    result.plaintext
                }
                "skmsg" => {
                    let name = SenderKeyName::new(from.clone(), sender.to_string());
                    group_decrypt(&ciphertext, &mut self.store, &name).await?
                }
                other => {
                    eprintln!("[e2ee] unsupported enc type {other} in {id}");
                    continue;
                }
            };
            // Group messages carry the sender key distribution inside the decrypted
            // pkmsg/msg; Armadillo parsing (next step) will feed it here. For now,
            // try the common case where the plaintext *is* an SKDM.
            if let Ok(skdm) = SenderKeyDistributionMessage::try_from(plaintext.as_slice()) {
                let name = SenderKeyName::new(from.clone(), sender.to_string());
                let _ = process_sender_key_distribution_message(&name, &skdm, &mut self.store).await;
            }
            let rec = Received {
                id: id.clone(),
                from: from.clone(),
                participant: participant.clone(),
                timestamp,
                enc_type,
                plaintext: STANDARD.encode(&plaintext),
            };
            append_inbox(&rec)?;
            out.push(rec);
        }
        Ok(out)
    }
}

fn append_inbox(rec: &Received) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = data_dir()?.join("e2ee-inbox.jsonl");
    let mut f = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
    writeln!(f, "{}", serde_json::to_string(rec)?)?;
    f.sync_data()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msgr_device_jid_roundtrips_through_binary() {
        let jid: wacore_binary::jid::Jid = "100000000000001:86@msgr".parse().unwrap();
        eprintln!("parsed: {jid:?} display={jid}");
        let node = NodeBuilder::new("ack").attr("to", jid_value("100000000000001:86@msgr")).build();
        let bytes = wacore_binary::marshal::marshal(&node).unwrap();
        let back = wacore_binary::marshal::unmarshal_ref(&bytes[1..]).unwrap();
        let to = attr(&back, "to").unwrap();
        eprintln!("roundtrip to={to}");
        assert!(to.contains(":86") || to.contains(".86"), "device lost: {to}");
    }

    #[test]
    fn bare_msgr_jid_uses_fb_jid_token() {
        let node = NodeBuilder::new("user").attr("jid", jid_value("100000000000001@msgr")).build();
        let bytes = wacore_binary::marshal::marshal(&node).unwrap();
        assert!(bytes.contains(&wacore_binary::token::FB_JID), "expected FB_JID token");
        let back = wacore_binary::marshal::unmarshal_ref(&bytes[1..]).unwrap();
        assert_eq!(attr(&back, "jid").unwrap(), "100000000000001@msgr");
    }

    #[test]
    fn signal_address_parses_both_separators() {
        assert_eq!(signal_address("123:86@msgr").unwrap().to_string(), signal_address("123.86@msgr").unwrap().to_string());
    }
}

/// Keep an E2EE connection alive (prekeys, active presence, pings, reconnects)
/// and hand every decrypted message to `on_message`. Runs until `deadline` (if
/// any) passes; otherwise forever.
pub async fn run(
    session: &crate::session::Session,
    deadline: Option<tokio::time::Instant>,
    mut outgoing: Option<&mut tokio::sync::mpsc::UnboundedReceiver<Outgoing>>,
    mut on_message: impl FnMut(&Received),
    mut on_receipt: impl FnMut(&Receipt),
    mut log: impl FnMut(String),
) -> Result<usize> {
    use std::time::Duration;
    let mut device = crate::e2ee::Device::load().ok_or_else(|| anyhow!("no E2EE device registered"))?;
    let far = tokio::time::Instant::now() + Duration::from_secs(86_400 * 365);
    let deadline = deadline.unwrap_or(far);
    let mut decrypted = 0;
    let mut backoff = 2u64;
    while tokio::time::Instant::now() < deadline {
        let sock = match crate::e2ee_socket::ChatSocket::connect(session, &mut device).await {
            Ok(sock) => sock,
            Err(e) => {
                log(format!("connect failed: {e:#}; retrying in {backoff}s"));
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
                continue;
            }
        };
        backoff = 2;
        let store = SignalStore::open(&device)?;
        let mut listener = Listener::new(sock, store, session.boot.fbid);
        let mut backlog = Vec::new();
        let (on_server, uploaded) = listener.ensure_prekeys(&mut backlog).await?;
        listener.set_active(&mut backlog).await?;
        log(format!("logged in, active; prekeys on server {on_server}, uploaded {uploaded}"));
        let mut next_ping = tokio::time::Instant::now() + Duration::from_secs(20);
        let mut media_conn: Option<MediaConn> = None;
        loop {
            let bytes = if !backlog.is_empty() {
                backlog.remove(0)
            } else {
                let next_out = async {
                    match outgoing.as_deref_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                };
                let received = tokio::select! {
                    Some(out) = next_out => {
                        if let OutgoingPayload::Read(ids) = &out.payload {
                            let r = listener.send_read(out.peer, ids).await;
                            if let Err(e) = &r {
                                log(format!("read receipt failed: {e:#}"));
                            }
                            let _ = out.done.send(r.map_err(|e| format!("{e:#}")));
                            continue;
                        }
                        let sent = match &out.payload {
                            OutgoingPayload::Text(text) => listener.send_text(out.peer, text, &mut backlog).await,
                            OutgoingPayload::Image(path) => listener.send_image(out.peer, path, &session.http, &mut media_conn, &mut backlog).await,
                            OutgoingPayload::Sticker { url, label } => listener.send_sticker(out.peer, url, label, &session.http, &mut media_conn, &mut backlog).await,
                            OutgoingPayload::Read(_) => unreachable!("handled above"),
                        };
                        match sent {
                            Ok(rec) => {
                                on_message(&rec);
                                let _ = out.done.send(Ok(()));
                            }
                            Err(e) => {
                                log(format!("send failed: {e:#}"));
                                let _ = out.done.send(Err(format!("{e:#}")));
                            }
                        }
                        continue;
                    }
                    r = tokio::time::timeout_at(next_ping.min(deadline), listener.sock.recv()) => r,
                };
                match received {
                    Ok(Ok(Some(b))) => b,
                    Ok(Ok(None)) => { log("socket closed".into()); break; }
                    Ok(Err(e)) => { log(format!("disconnected: {e:#}")); break; }
                    Err(_) if tokio::time::Instant::now() >= deadline => break,
                    Err(_) => {
                        if let Err(e) = listener.ping().await {
                            log(format!("ping failed: {e:#}"));
                            break;
                        }
                        next_ping = tokio::time::Instant::now() + Duration::from_secs(20);
                        continue;
                    }
                }
            };
            let summary = crate::e2ee_socket::describe(&bytes);
            match listener.handle(&bytes).await {
                Ok(msgs) => {
                    for m in &msgs {
                        decrypted += 1;
                        // Media: fetch and decrypt the file first, so the UI can show it.
                        if let Err(e) = cache_media(&mut listener, session, &mut media_conn, m, &mut backlog).await {
                            log(format!("media for {}: {e:#}", m.id));
                        }
                        on_message(m);
                    }
                    for r in listener.take_receipts() {
                        on_receipt(&r);
                    }
                }
                Err(e) => log(format!("✗ {}: {e:#}", summary.chars().take(80).collect::<String>())),
            }
            let quiet = std::env::var_os("MESSENGER_DEBUG").is_none()
                && (summary.starts_with("<ack") || (summary.starts_with("<iq") && summary.contains("type=result")));
            if !quiet {
                log(summary.chars().take(160).collect());
            }
        }
        listener.sock.close().await;
    }
    Ok(decrypted)
}

/// If `rec` carries a media file, download and decrypt it into the cache
/// (asking for `media_conn` hosts when needed; they're valid for a few minutes).
async fn cache_media(
    listener: &mut Listener,
    session: &crate::session::Session,
    media_conn: &mut Option<MediaConn>,
    rec: &Received,
    backlog: &mut Vec<Vec<u8>>,
) -> Result<()> {
    let raw = STANDARD.decode(&rec.plaintext)?;
    let Ok(decoded) = crate::armadillo::decode(&raw) else { return Ok(()) };
    let crate::armadillo::Content2::Media { kind, file: Some(file), .. } = &decoded.content else { return Ok(()) };
    if crate::e2ee_media::cached_path(file).is_some_and(|p| p.exists()) {
        return Ok(());
    }
    if !media_conn.as_ref().is_some_and(MediaConn::fresh) {
        *media_conn = Some(listener.media_conn(backlog).await?);
    }
    let hosts = &media_conn.as_ref().expect("just fetched").hosts;
    crate::e2ee_media::fetch_to_cache(&session.http, hosts, file, kind.into()).await?;
    Ok(())
}

/// All previously decrypted messages (oldest first), for replay at startup.
pub fn read_inbox() -> Vec<Received> {
    let Ok(path) = data_dir().map(|d| d.join("e2ee-inbox.jsonl")) else { return vec![] };
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn jid_user(jid: &str) -> Option<i64> {
    jid.split(['@', '.', ':']).next()?.parse().ok()
}

/// Turn a decrypted record into something the store can apply. `None` for
/// protocol-only payloads and (for now) group chats.
pub fn to_incoming(rec: &Received, me: i64) -> Option<crate::store::E2eeIncoming> {
    let raw = STANDARD.decode(&rec.plaintext).ok()?;
    let decoded = crate::armadillo::decode(&raw).ok()?;
    if !rec.from.ends_with("@msgr") {
        return None; // group JIDs: not mapped to LS threads yet
    }
    let sender = jid_user(rec.participant.as_deref().unwrap_or(&rec.from))?;
    let from_me = sender == me;
    let peer = if from_me { jid_user(decoded.destination_jid.as_deref()?)? } else { sender };
    Some(crate::store::E2eeIncoming {
        id: rec.id.clone(),
        sender,
        from_me,
        peer,
        timestamp_ms: rec.timestamp * 1000,
        decoded,
    })
}
