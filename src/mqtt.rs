//! Minimal MQTT 3.1 ("MQIsdp") client over WebSocket, enough for
//! edge-chat.facebook.com: CONNECT, SUBSCRIBE, PUBLISH (QoS 0/1), PUBACK, PING.

use anyhow::{bail, Result};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as WsMessage};

#[derive(Debug)]
pub enum Packet {
    ConnAck { code: u8 },
    Publish { topic: String, payload: Vec<u8>, packet_id: Option<u16> },
    PubAck,
    SubAck,
    PingResp,
    Other(#[allow(dead_code)] u8),
}

fn encode_len(mut len: usize, out: &mut Vec<u8>) {
    loop {
        let mut b = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            b |= 0x80;
        }
        out.push(b);
        if len == 0 {
            break;
        }
    }
}

fn put_str(s: &str, out: &mut Vec<u8>) {
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn packet(header: u8, body: Vec<u8>) -> Vec<u8> {
    let mut out = vec![header];
    encode_len(body.len(), &mut out);
    out.extend(body);
    out
}

pub fn connect(client_id: &str, username: &str, keepalive: u16) -> Vec<u8> {
    let mut body = Vec::new();
    put_str("MQIsdp", &mut body);
    body.push(3); // protocol level
    body.push(0x80 | 0x02); // username + clean session
    body.extend_from_slice(&keepalive.to_be_bytes());
    put_str(client_id, &mut body);
    put_str(username, &mut body);
    packet(0x10, body)
}

pub fn subscribe(packet_id: u16, topics: &[&str]) -> Vec<u8> {
    let mut body = packet_id.to_be_bytes().to_vec();
    for t in topics {
        put_str(t, &mut body);
        body.push(0);
    }
    packet(0x82, body)
}

pub fn publish(topic: &str, payload: &[u8], qos1_packet_id: Option<u16>) -> Vec<u8> {
    let mut body = Vec::new();
    put_str(topic, &mut body);
    let header = if let Some(id) = qos1_packet_id {
        body.extend_from_slice(&id.to_be_bytes());
        0x32
    } else {
        0x30
    };
    body.extend_from_slice(payload);
    packet(header, body)
}

pub fn puback(packet_id: u16) -> Vec<u8> {
    packet(0x40, packet_id.to_be_bytes().to_vec())
}

pub fn pingreq() -> Vec<u8> {
    vec![0xC0, 0]
}

/// Parse every complete packet in `buf`, leaving any partial tail in place.
pub fn parse(buf: &mut Vec<u8>) -> Result<Vec<Packet>> {
    let mut out = Vec::new();
    loop {
        if buf.len() < 2 {
            break;
        }
        let (mut len, mut mult, mut i) = (0usize, 1usize, 1usize);
        loop {
            let Some(&b) = buf.get(i) else { return Ok(out) };
            len += (b & 0x7f) as usize * mult;
            mult *= 128;
            i += 1;
            if b & 0x80 == 0 {
                break;
            }
            if i > 5 {
                bail!("malformed MQTT length");
            }
        }
        if buf.len() < i + len {
            break;
        }
        let header = buf[0];
        let body: Vec<u8> = buf[i..i + len].to_vec();
        buf.drain(..i + len);
        out.push(match header >> 4 {
            2 => Packet::ConnAck { code: *body.get(1).unwrap_or(&0) },
            3 => {
                let qos = (header >> 1) & 3;
                let tlen = u16::from_be_bytes([body[0], body[1]]) as usize;
                let topic = String::from_utf8_lossy(&body[2..2 + tlen]).into_owned();
                let mut p = 2 + tlen;
                let packet_id = (qos > 0).then(|| {
                    let id = u16::from_be_bytes([body[p], body[p + 1]]);
                    p += 2;
                    id
                });
                Packet::Publish { topic, payload: body[p..].to_vec(), packet_id }
            }
            4 => Packet::PubAck,
            9 => Packet::SubAck,
            13 => Packet::PingResp,
            t => Packet::Other(t),
        });
    }
    Ok(out)
}

pub type WsStream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub async fn open(url: &str, cookie: &str, user_agent: &str) -> Result<WsStream> {
    let mut req = url.into_client_request()?;
    let h = req.headers_mut();
    h.insert("Cookie", cookie.parse()?);
    h.insert("Origin", "https://www.facebook.com".parse()?);
    h.insert("User-Agent", user_agent.parse()?);
    // Debug only: extra handshake headers, "Name: value|Name: value".
    if let Ok(extra) = std::env::var("MESSENGER_WS_HEADERS") {
        for pair in extra.split('|').filter_map(|p| p.split_once(": ")) {
            h.insert(tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(pair.0.as_bytes())?, pair.1.parse()?);
        }
    }
    let (ws, _) = tokio_tungstenite::connect_async(req).await?;
    Ok(ws)
}

pub async fn send(ws: &mut WsStream, bytes: Vec<u8>) -> Result<()> {
    ws.send(WsMessage::Binary(bytes.into())).await?;
    Ok(())
}

/// Read the next WebSocket frame's bytes (None when the socket closes).
pub async fn recv(ws: &mut WsStream) -> Result<Option<Vec<u8>>> {
    while let Some(msg) = ws.next().await {
        match msg? {
            WsMessage::Binary(b) => return Ok(Some(b.to_vec())),
            WsMessage::Text(t) => return Ok(Some(t.as_bytes().to_vec())),
            WsMessage::Close(frame) => {
                if let Some(f) = frame {
                    eprintln!("[ws] closed by server: {} {}", u16::from(f.code), f.reason);
                }
                return Ok(None);
            }
            _ => continue,
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_publish() {
        let mut buf = publish("/ls_resp", b"{\"a\":1}", Some(7));
        buf.extend(pingreq()); // unknown-to-us type 12 just to test framing
        let pkts = parse(&mut buf).unwrap();
        assert!(buf.is_empty());
        match &pkts[0] {
            Packet::Publish { topic, payload, packet_id } => {
                assert_eq!(topic, "/ls_resp");
                assert_eq!(payload, b"{\"a\":1}");
                assert_eq!(*packet_id, Some(7));
            }
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn long_length_encoding() {
        let payload = vec![b'x'; 300];
        let mut buf = publish("/t", &payload, None);
        let partial = buf.split_off(100);
        let mut first = buf;
        assert!(parse(&mut first).unwrap().is_empty());
        first.extend(partial);
        assert_eq!(parse(&mut first).unwrap().len(), 1);
    }
}
