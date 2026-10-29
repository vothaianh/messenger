//! Meta's Dynamic Gateway (DGW) WebSocket transport, which the web client now
//! uses for Lightspeed (`wss://gateway.facebook.com/ws/lightspeed`). The MQTT
//! endpoint still answers requests but no longer pushes anything (new
//! messages, fetched history), so the app speaks DGW instead.
//!
//! Wire format, from the web client (`DGWConstants`, live frames):
//! - frame = type (u8) + stream id (u16 LE) + payload length (u24 LE) + payload;
//!   ping (9) and pong (10) are a single byte with no header. One WebSocket
//!   message can carry several frames, and a frame can span messages.
//! - Establish stream (15): payload `{}`; the server answers `{"code":200}`.
//! - Data (13): u16 LE ack id (high bit = "please ack"; ids count per stream
//!   from 0) followed by the body. Bodies are the same JSON as MQTT `/ls_req`
//!   and `/ls_resp`.
//! - Ack (12): the u16 LE ack id being acknowledged.
//! - End of data (14): empty; closes a stream (the web client opens one stream
//!   per database sync and one per task request).
//!
//! The socket negotiates `permessage-deflate` like a browser does (via
//! `yawc`; tungstenite can't).
//!
//! Right after a FetchMessages task (228) sent here, the server drops the
//! connection instead of pushing the history, so `client.rs` fetches history
//! over HTTP (task, then a type-2 delta sync).
//!
//! The web client gets live changes pushed on its database 1 stream (opened
//! with a type-2 delta sync from its cursor, socket URL with
//! `x-dgw-regionhint`). We do the same but get no pushes yet, so `client.rs`
//! also polls a delta sync over HTTP every few seconds.

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};

pub const ACK: u8 = 12;
pub const DATA: u8 = 13;
pub const END: u8 = 14;
pub const ESTABLISH: u8 = 15;
pub const PING: u8 = 9;
pub const PONG: u8 = 10;

const NEEDS_ACK: u16 = 0x8000;

#[derive(Debug, PartialEq)]
pub enum Frame {
    Establish { stream: u16, payload: Vec<u8> },
    Data { stream: u16, ack_id: Option<u16>, body: Vec<u8> },
    Ack { stream: u16, ack_id: u16 },
    End { stream: u16 },
    Ping,
    Pong,
    Other { kind: u8, stream: u16 },
}

fn frame(kind: u8, stream: u16, payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut out = Vec::with_capacity(6 + payload.len());
    out.push(kind);
    out.extend_from_slice(&stream.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes()[..3]);
    out.extend_from_slice(payload);
    out
}

pub fn establish(stream: u16) -> Vec<u8> {
    frame(ESTABLISH, stream, b"{}")
}

/// A data frame that asks the server to ack it.
pub fn data(stream: u16, ack_id: u16, body: &[u8]) -> Vec<u8> {
    let mut payload = (ack_id | NEEDS_ACK).to_le_bytes().to_vec();
    payload.extend_from_slice(body);
    frame(DATA, stream, &payload)
}

pub fn ack(stream: u16, ack_id: u16) -> Vec<u8> {
    frame(ACK, stream, &ack_id.to_le_bytes())
}

pub fn end(stream: u16) -> Vec<u8> {
    frame(END, stream, &[])
}

/// Take every complete frame out of `buf` (WebSocket message bytes appended
/// as they arrive), leaving a trailing partial frame for the next message.
pub fn parse(buf: &mut Vec<u8>) -> Vec<Frame> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(&kind) = buf.get(pos) {
        if kind == PING || kind == PONG {
            out.push(if kind == PING { Frame::Ping } else { Frame::Pong });
            pos += 1;
            continue;
        }
        let Some(header) = buf.get(pos..pos + 6) else { break };
        let stream = u16::from_le_bytes([header[1], header[2]]);
        let len = u32::from_le_bytes([header[3], header[4], header[5], 0]) as usize;
        let Some(payload) = buf.get(pos + 6..pos + 6 + len) else { break };
        out.push(match kind {
            ESTABLISH => Frame::Establish { stream, payload: payload.to_vec() },
            DATA if payload.len() >= 2 => {
                let id = u16::from_le_bytes([payload[0], payload[1]]);
                let ack_id = (id & NEEDS_ACK != 0).then_some(id & !NEEDS_ACK);
                Frame::Data { stream, ack_id, body: payload[2..].to_vec() }
            }
            ACK if payload.len() >= 2 => Frame::Ack { stream, ack_id: u16::from_le_bytes([payload[0], payload[1]]) },
            END => Frame::End { stream },
            kind => Frame::Other { kind, stream },
        });
        pos += 6 + len;
    }
    buf.drain(..pos);
    out
}

/// The Lightspeed endpoint URL, with the query parameters the web client sends.
/// Auth is the session cookie (auth type "1:0" = FACEBOOK) plus `x-dgw-uuid`,
/// which must be the logged-in user's fbid: any other value gets 4003.
/// The socket URL, with the same parameters (and order) as the web client.
/// `region` is the mailbox region hint from the page.
pub fn lightspeed_url(app_id: &str, fbid: i64, device_id: &str, region: Option<&str>) -> String {
    let logging_id = uuid::Uuid::new_v4();
    let region = region.map(|r| format!("&x-dgw-regionhint={r}")).unwrap_or_default();
    format!(
        "wss://gateway.facebook.com/ws/lightspeed?x-dgw-appid={app_id}&x-dgw-appversion=0&x-dgw-authtype=1:0\
         &x-dgw-version=5&x-dgw-uuid={fbid}&x-dgw-tier=prod&x-dgw-loggingid={logging_id}{region}\
         &x-dgw-deviceid={device_id}&x-dgw-ws-compression=true"
    )
}

pub struct Socket(yawc::TcpWebSocket);

impl Socket {
    pub async fn open(url: &str, cookie: &str, user_agent: &str) -> Result<Socket> {
        let request = yawc::HttpRequestBuilder::new()
            .header("Cookie", cookie)
            .header("Origin", "https://www.facebook.com")
            .header("User-Agent", user_agent);
        let options = yawc::Options::default()
            .with_balanced_compression()
            .with_limits(64 << 20, 128 << 20);
        let ws = yawc::WebSocket::connect(url.parse()?)
            .with_request(request)
            .with_options(options)
            .await
            .map_err(|e| anyhow!("DGW connect: {e}"))?;
        Ok(Socket(ws))
    }

    pub async fn send(&mut self, bytes: Vec<u8>) -> Result<()> {
        self.0.send(yawc::Frame::binary(bytes)).await.map_err(|e| anyhow!("DGW send: {e}"))
    }

    /// Next message's bytes; None once the server closes the socket.
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        while let Some(frame) = self.0.next().await {
            match frame.opcode() {
                yawc::OpCode::Binary | yawc::OpCode::Text => return Ok(Some(frame.into_payload().to_vec())),
                yawc::OpCode::Close => {
                    if std::env::var_os("MESSENGER_DEBUG").is_some() {
                        eprintln!("[dgw] closed by server: {:?} {:?}", frame.close_code(), frame.close_reason().ok().flatten());
                    }
                    return Ok(None);
                }
                _ => continue,
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_match_the_web_client() {
        // Bytes captured from the web client: establish stream 0 with "{}".
        assert_eq!(establish(0), [0x0f, 0, 0, 2, 0, 0, b'{', b'}']);
        assert_eq!(ack(4, 0x1f), [0x0c, 4, 0, 2, 0, 0, 0x1f, 0]);
        let d = data(8, 0, b"{}");
        assert_eq!(&d[..8], &[0x0d, 8, 0, 4, 0, 0, 0x00, 0x80]);
    }

    #[test]
    fn parses_a_batch_of_frames() {
        let mut buf = establish(1);
        buf.push(PING);
        buf.extend(data(1, 3, b"{\"request_id\":null}"));
        buf.extend(end(1));
        let frames = parse(&mut buf);
        assert!(buf.is_empty());
        assert_eq!(frames.len(), 4);
        assert_eq!(frames[1], Frame::Ping);
        assert_eq!(frames[2], Frame::Data { stream: 1, ack_id: Some(3), body: b"{\"request_id\":null}".to_vec() });
        assert_eq!(frames[3], Frame::End { stream: 1 });
    }

    #[test]
    fn long_payload_uses_three_length_bytes() {
        let body = vec![b'x'; 70_000];
        let mut buf = data(0, 5, &body);
        let rest = buf.split_off(30_000);
        assert!(parse(&mut buf).is_empty(), "a partial frame waits for more bytes");
        buf.extend(rest);
        match &parse(&mut buf)[0] {
            Frame::Data { body: b, ack_id, .. } => {
                assert_eq!(b.len(), 70_000);
                assert_eq!(*ack_id, Some(5));
            }
            f => panic!("{f:?}"),
        }
    }
}
