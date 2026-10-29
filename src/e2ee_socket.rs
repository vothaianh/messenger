//! E2EE chat socket: Noise XX handshake to web-chat-e2ee.facebook.com with a
//! Messenger `ClientPayload`, then encrypted binary-node frames.
//!
//! Uses the MIT-licensed `wacore-*` building blocks (Noise, frame codec,
//! binary nodes, protobufs); the Messenger-specific login is ours.

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as WsMessage};
use wacore_libsignal::protocol::{KeyPair, PrivateKey};
use wacore_noise::framing::{encode_frame, FrameDecoder};
use wacore_noise::{NoiseCipher, XxHandshakeState};
use waproto::whatsapp::client_payload::{self, user_agent};
use waproto::buffa::MessageField;
use waproto::whatsapp::ClientPayload;

use crate::e2ee::Device;
use crate::session::{Session, BASE, USER_AGENT};

pub const E2EE_WS_URL: &str = "wss://web-chat-e2ee.facebook.com/ws/chat";

/// Login payload for a Messenger E2EE device (no pairing data: the device was
/// registered out of band via ICDC).
fn client_payload(session: &Session, device: &Device) -> Result<ClientPayload> {
    let fbid = device.fbid.ok_or_else(|| anyhow!("device has no fbid"))?;
    let device_no = device.wa_device_id.ok_or_else(|| anyhow!("device is not registered"))?;
    Ok(ClientPayload {
        username: Some(fbid as u64),
        device: Some(device_no as u32),
        passive: Some(false),
        pull: Some(true),
        product: Some(client_payload::Product::Messenger),
        fb_cat: Some(session.boot.e2ee_cat.as_bytes().to_vec()),
        fb_user_agent: Some(USER_AGENT.as_bytes().to_vec()),
        connect_type: Some(client_payload::ConnectType::WifiUnknown),
        connect_reason: Some(client_payload::ConnectReason::UserActivated),
        user_agent: MessageField::some(client_payload::UserAgent {
            platform: Some(user_agent::Platform::BlueWeb),
            app_version: MessageField::some(user_agent::AppVersion {
                primary: Some(301),
                secondary: Some(0),
                tertiary: Some(2),
                ..Default::default()
            }),
            mcc: Some("000".into()),
            mnc: Some("000".into()),
            os_version: Some(String::new()),
            os_build_number: Some(String::new()),
            manufacturer: Some("macOS".into()),
            device: Some("Chrome".into()),
            locale_language_iso6391: Some("en".into()),
            locale_country_iso31661_alpha2: Some("en".into()),
            ..Default::default()
        }),
        ..Default::default()
    })
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// An authenticated-encryption chat socket after the Noise handshake.
pub struct ChatSocket {
    ws: Ws,
    decoder: FrameDecoder,
    write: NoiseCipher,
    read: NoiseCipher,
    write_counter: u32,
    read_counter: u32,
}

impl ChatSocket {
    pub async fn connect(session: &Session, device: &mut Device) -> Result<ChatSocket> {
        let noise_priv = device.ensure_noise_key()?;
        let private = PrivateKey::deserialize(&noise_priv).map_err(|e| anyhow!("noise key: {e:?}"))?;
        let public = private.public_key().map_err(|e| anyhow!("noise key: {e:?}"))?;
        let static_kp = KeyPair::new(public, private);

        let payload = client_payload(session, device)?;
        let payload_bytes = waproto::codec::client_payload_to_vec(&payload);
        let mut hs = XxHandshakeState::new(static_kp, payload_bytes, &wacore_binary::consts::WA_CONN_HEADER)
            .map_err(|e| anyhow!("noise init: {e:?}"))?;

        let mut req = E2EE_WS_URL.into_client_request()?;
        req.headers_mut().insert("Origin", BASE.parse()?);
        req.headers_mut().insert("User-Agent", USER_AGENT.parse()?);
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await.context("connecting to E2EE socket")?;

        // Client hello goes out prefixed with the "WA" connection header.
        let hello = hs.build_client_hello().map_err(|e| anyhow!("client hello: {e:?}"))?;
        let framed = encode_frame(&hello, Some(&wacore_binary::consts::WA_CONN_HEADER)).map_err(|e| anyhow!("{e:?}"))?;
        ws.send(WsMessage::Binary(framed.into())).await?;

        let mut decoder = FrameDecoder::new();
        let server_hello = next_frame(&mut ws, &mut decoder).await?.ok_or_else(|| anyhow!("socket closed during handshake"))?;
        let finish = hs
            .read_server_hello_and_build_client_finish(&server_hello)
            .map_err(|e| anyhow!("server hello: {e:?}"))?;
        let framed = encode_frame(&finish, None).map_err(|e| anyhow!("{e:?}"))?;
        ws.send(WsMessage::Binary(framed.into())).await?;
        let outcome = hs.finish().map_err(|e| anyhow!("handshake finish: {e:?}"))?;

        Ok(ChatSocket { ws, decoder, write: outcome.write_cipher, read: outcome.read_cipher, write_counter: 0, read_counter: 0 })
    }

    /// Next decrypted, decompressed node payload (binary-node bytes).
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        let Some(frame) = next_frame(&mut self.ws, &mut self.decoder).await? else { return Ok(None) };
        let mut buf = frame.to_vec();
        self.read
            .decrypt_in_place_with_counter(self.read_counter, &mut buf)
            .map_err(|e| anyhow!("frame decrypt: {e:?}"))?;
        self.read_counter += 1;
        Ok(Some(wacore_binary::util::unpack(&buf).map_err(|e| anyhow!("unpack: {e:?}"))?.into_owned()))
    }

    /// Encrypt and send one marshalled binary node.
    pub async fn send(&mut self, node_bytes: &[u8]) -> Result<()> {
        let ciphertext = self
            .write
            .encrypt_with_counter(self.write_counter, node_bytes)
            .map_err(|e| anyhow!("frame encrypt: {e:?}"))?;
        self.write_counter += 1;
        let framed = encode_frame(&ciphertext, None).map_err(|e| anyhow!("{e:?}"))?;
        self.ws.send(WsMessage::Binary(framed.into())).await?;
        Ok(())
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }
}

async fn next_frame(ws: &mut Ws, decoder: &mut FrameDecoder) -> Result<Option<Vec<u8>>> {
    loop {
        if let Some(frame) = decoder.decode_frame() {
            return Ok(Some(frame.to_vec()));
        }
        match ws.next().await {
            Some(Ok(WsMessage::Binary(b))) => decoder.feed(&b),
            Some(Ok(WsMessage::Close(c))) => bail!("E2EE socket closed by server: {c:?}"),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
            None => return Ok(None),
        }
    }
}

/// One-line summary of a binary node: tag, attributes and child tags.
pub fn describe(node_bytes: &[u8]) -> String {
    match wacore_binary::marshal::unmarshal_ref(node_bytes) {
        Ok(node) => {
            let attrs: Vec<String> = node.attrs_iter().map(|(k, v)| format!("{k}={v}")).collect();
            let children: Vec<String> = node.children().map(|c| c.iter().map(|n| n.tag.to_string()).collect()).unwrap_or_default();
            format!("<{} {}> children={children:?}", node.tag, attrs.join(" "))
        }
        Err(e) => format!("(unparseable node: {e:?})"),
    }
}

/// Top-level protobuf field outline (`#field:wiretype[len]`), for inspecting
/// decrypted payloads without printing their content.
pub fn proto_outline(mut buf: &[u8]) -> String {
    fn varint(buf: &mut &[u8]) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let (&b, rest) = buf.split_first()?;
            *buf = rest;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
    let mut parts = Vec::new();
    while !buf.is_empty() && parts.len() < 24 {
        let Some(key) = varint(&mut buf) else { break };
        let (field, wire) = (key >> 3, key & 7);
        let desc = match wire {
            0 => varint(&mut buf).map(|_| "varint".to_string()),
            1 if buf.len() >= 8 => { buf = &buf[8..]; Some("fixed64".into()) }
            5 if buf.len() >= 4 => { buf = &buf[4..]; Some("fixed32".into()) }
            2 => varint(&mut buf).and_then(|len| {
                let len = len as usize;
                (buf.len() >= len).then(|| { buf = &buf[len..]; format!("bytes[{len}]") })
            }),
            _ => None,
        };
        match desc {
            Some(d) => parts.push(format!("#{field}:{d}")),
            None => { parts.push("…(not protobuf)".into()); break; }
        }
    }
    parts.join(" ")
}

/// Multi-line tree of a binary node (tags and attributes; byte contents shown
/// only as lengths), for diagnostics.
pub fn describe_tree(node_bytes: &[u8], max_depth: usize) -> String {
    fn walk(node: &wacore_binary::node::NodeRef<'_>, depth: usize, max: usize, out: &mut String) {
        let attrs: Vec<String> = node.attrs_iter().map(|(k, v)| format!("{k}={v}")).collect();
        let extra = match node.content.as_ref() {
            Some(wacore_binary::node::NodeContentRef::Bytes(b)) => format!(" [{} bytes]", b.len()),
            _ => String::new(),
        };
        out.push_str(&format!("{}<{} {}>{extra}\n", "  ".repeat(depth), node.tag, attrs.join(" ")));
        if depth < max {
            for c in node.children().unwrap_or_default() {
                walk(c, depth + 1, max, out);
            }
        }
    }
    match wacore_binary::marshal::unmarshal_ref(node_bytes) {
        Ok(node) => {
            let mut out = String::new();
            walk(&node, 0, max_depth, &mut out);
            out
        }
        Err(e) => format!("(unparseable node: {e:?})"),
    }
}
