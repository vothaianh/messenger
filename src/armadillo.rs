//! Armadillo payloads inside decrypted E2EE messages:
//! `MessageTransport` → `MessageApplication` → `ConsumerApplication`.
//!
//! Minimal hand-written prost structs: only the fields we use. Field numbers
//! follow Meta's wire format (see docs/E2EE.md); unknown fields are skipped.

use anyhow::{anyhow, Result};
use prost::Message as _;

// ---- WACommon ----

#[derive(Clone, PartialEq, prost::Message)]
struct SubProtocol {
    #[prost(bytes = "vec", optional, tag = "1")]
    payload: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "2")]
    version: Option<i32>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct MessageKey {
    #[prost(string, optional, tag = "1")]
    remote_jid: Option<String>,
    #[prost(bool, optional, tag = "2")]
    from_me: Option<bool>,
    #[prost(string, optional, tag = "3")]
    id: Option<String>,
    #[prost(string, optional, tag = "4")]
    participant: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct MessageText {
    #[prost(string, optional, tag = "1")]
    text: Option<String>,
}

// ---- MessageTransport ----

#[derive(Clone, PartialEq, prost::Message)]
struct MessageTransport {
    #[prost(message, optional, tag = "1")]
    payload: Option<TransportPayload>,
    #[prost(message, optional, tag = "2")]
    protocol: Option<TransportProtocol>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct TransportPayload {
    #[prost(message, optional, tag = "1")]
    application_payload: Option<SubProtocol>,
    /// `FutureProofBehavior` (0 = PLACEHOLDER).
    #[prost(int32, optional, tag = "3")]
    future_proof: Option<i32>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct TransportProtocol {
    #[prost(message, optional, tag = "1")]
    integral: Option<Integral>,
    #[prost(message, optional, tag = "2")]
    ancillary: Option<Ancillary>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct Integral {
    #[prost(bytes = "vec", optional, tag = "1")]
    padding: Option<Vec<u8>>,
    #[prost(message, optional, tag = "2")]
    dsm: Option<DeviceSentMessage>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct DeviceSentMessage {
    #[prost(string, optional, tag = "1")]
    destination_jid: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct Ancillary {
    #[prost(message, optional, tag = "2")]
    skdm: Option<Skdm>,
    #[prost(message, optional, tag = "5")]
    backup_directive: Option<BackupDirective>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct BackupDirective {
    #[prost(string, optional, tag = "1")]
    message_id: Option<String>,
    /// 1 = UPSERT.
    #[prost(int32, optional, tag = "2")]
    action_type: Option<i32>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct Skdm {
    #[prost(string, optional, tag = "1")]
    group_id: Option<String>,
    #[prost(bytes = "vec", optional, tag = "2")]
    axolotl_sender_key_distribution_message: Option<Vec<u8>>,
}

// ---- MessageApplication ----

#[derive(Clone, PartialEq, prost::Message)]
struct MessageApplication {
    #[prost(message, optional, tag = "1")]
    payload: Option<AppPayload>,
    #[prost(message, optional, tag = "2")]
    metadata: Option<AppMetadata>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct AppPayload {
    #[prost(message, optional, tag = "4")]
    sub_protocol: Option<SubProtocolPayload>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct SubProtocolPayload {
    #[prost(message, optional, tag = "2")]
    consumer_message: Option<SubProtocol>,
    #[prost(message, optional, tag = "7")]
    armadillo: Option<SubProtocol>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct AppMetadata {
    #[prost(bytes = "vec", optional, tag = "8")]
    franking_key: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "9")]
    franking_version: Option<i32>,
    #[prost(message, optional, tag = "10")]
    quoted_message: Option<QuotedMessage>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct QuotedMessage {
    #[prost(string, optional, tag = "1")]
    stanza_id: Option<String>,
}

// ---- ConsumerApplication ----

#[derive(Clone, PartialEq, prost::Message)]
struct ConsumerApplication {
    #[prost(message, optional, tag = "1")]
    payload: Option<ConsumerPayload>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ConsumerPayload {
    #[prost(message, optional, tag = "1")]
    content: Option<Content>,
    #[prost(message, optional, tag = "2")]
    application_data: Option<ApplicationData>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ApplicationData {
    #[prost(message, optional, tag = "1")]
    revoke: Option<KeyOnly>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct KeyOnly {
    #[prost(message, optional, tag = "1")]
    key: Option<MessageKey>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct Content {
    #[prost(message, optional, tag = "1")]
    message_text: Option<MessageText>,
    #[prost(message, optional, tag = "2")]
    image: Option<CaptionedMedia>,
    #[prost(message, optional, tag = "5")]
    extended_text: Option<ExtendedText>,
    #[prost(message, optional, tag = "7")]
    document: Option<DocumentMessage>,
    #[prost(message, optional, tag = "8")]
    audio: Option<CaptionedMedia>,
    #[prost(message, optional, tag = "9")]
    video: Option<CaptionedMedia>,
    #[prost(message, optional, tag = "12")]
    sticker: Option<CaptionedMedia>,
    #[prost(message, optional, tag = "16")]
    reaction: Option<ReactionMessage>,
    #[prost(message, optional, tag = "19")]
    edit: Option<EditMessage>,
}

/// Image/video/audio/sticker: media subprotocol at 1, caption (images/videos) at 2.
#[derive(Clone, PartialEq, prost::Message)]
struct CaptionedMedia {
    #[prost(message, optional, tag = "1")]
    media: Option<SubProtocol>,
    #[prost(message, optional, tag = "2")]
    caption: Option<MessageText>,
}

// ---- WAMediaTransport (inside each media sub-protocol) ----

/// `ImageTransport` / `VideoTransport` / ...: integral.transport is the
/// `WAMediaTransport`; for images, ancillary holds height (1) and width (2).
#[derive(Clone, PartialEq, prost::Message)]
struct MediaTransportWrapper {
    #[prost(message, optional, tag = "1")]
    integral: Option<TransportIntegral>,
    #[prost(message, optional, tag = "2")]
    ancillary: Option<ImageDims>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct TransportIntegral {
    #[prost(message, optional, tag = "1")]
    transport: Option<WaMediaTransport>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ImageDims {
    #[prost(uint32, optional, tag = "1")]
    height: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    width: Option<u32>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct WaMediaTransport {
    #[prost(message, optional, tag = "1")]
    integral: Option<MediaIntegral>,
    #[prost(message, optional, tag = "2")]
    ancillary: Option<MediaAncillary>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct MediaIntegral {
    #[prost(bytes = "vec", optional, tag = "1")]
    file_sha256: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    media_key: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "3")]
    file_enc_sha256: Option<Vec<u8>>,
    #[prost(string, optional, tag = "4")]
    direct_path: Option<String>,
    #[prost(int64, optional, tag = "5")]
    media_key_timestamp: Option<i64>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct MediaAncillary {
    #[prost(uint64, optional, tag = "1")]
    file_length: Option<u64>,
    #[prost(string, optional, tag = "2")]
    mimetype: Option<String>,
    #[prost(message, optional, tag = "3")]
    thumbnail: Option<MediaThumbnail>,
    #[prost(string, optional, tag = "4")]
    object_id: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct MediaThumbnail {
    #[prost(bytes = "vec", optional, tag = "1")]
    jpeg_thumbnail: Option<Vec<u8>>,
    #[prost(uint32, optional, tag = "3")]
    width: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    height: Option<u32>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ExtendedText {
    #[prost(message, optional, tag = "1")]
    text: Option<MessageText>,
    #[prost(string, optional, tag = "3")]
    canonical_url: Option<String>,
    #[prost(string, optional, tag = "5")]
    title: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct DocumentMessage {
    #[prost(message, optional, tag = "1")]
    document: Option<SubProtocol>,
    #[prost(string, optional, tag = "2")]
    file_name: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ReactionMessage {
    #[prost(message, optional, tag = "1")]
    key: Option<MessageKey>,
    #[prost(string, optional, tag = "2")]
    text: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct EditMessage {
    #[prost(message, optional, tag = "1")]
    key: Option<MessageKey>,
    #[prost(message, optional, tag = "2")]
    message: Option<MessageText>,
}

// ---- Normalised result ----

#[derive(Debug, Clone, PartialEq)]
pub enum MediaKind {
    Image,
    Video,
    Audio,
    File,
    Sticker,
}

/// Where an encrypted media file is and how to open it (see `e2ee_media.rs`).
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct MediaFile {
    pub direct_path: String,
    pub media_key: Vec<u8>,
    pub file_sha256: Vec<u8>,
    pub file_enc_sha256: Vec<u8>,
    pub mimetype: String,
    pub file_length: u64,
    pub width: u32,
    pub height: u32,
    /// Small inline JPEG preview, when the sender included one.
    pub thumbnail: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Content2 {
    Text { text: String },
    Link { text: String, url: String, title: String },
    Media { kind: MediaKind, caption: String, file_name: String, file: Option<MediaFile> },
    Reaction { target_id: String, emoji: String },
    Edit { target_id: String, text: String },
    Revoke { target_id: String },
    /// Only protocol data (e.g. a sender key distribution), nothing to show.
    None,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decoded {
    pub content: Content2,
    /// Message id this one replies to.
    pub reply_to: Option<String>,
    /// For messages sent from our own other devices: the chat they went to.
    pub destination_jid: Option<String>,
    /// Group sender key distribution (group id, serialized SKDM).
    pub skdm: Option<(String, Vec<u8>)>,
}

fn text_of(t: Option<MessageText>) -> String {
    t.and_then(|t| t.text).unwrap_or_default()
}

/// Decode a decrypted Signal plaintext (Armadillo `MessageTransport`).
pub fn decode(plaintext: &[u8]) -> Result<Decoded> {
    let transport = MessageTransport::decode(plaintext).map_err(|e| anyhow!("MessageTransport: {e}"))?;
    let protocol = transport.protocol.unwrap_or_default();
    let destination_jid = protocol.integral.and_then(|i| i.dsm).and_then(|d| d.destination_jid);
    let skdm = protocol
        .ancillary
        .and_then(|a| a.skdm)
        .and_then(|s| Some((s.group_id.unwrap_or_default(), s.axolotl_sender_key_distribution_message?)));

    let app_bytes = transport.payload.and_then(|p| p.application_payload).and_then(|s| s.payload);
    let Some(app_bytes) = app_bytes else {
        return Ok(Decoded { content: Content2::None, reply_to: None, destination_jid, skdm });
    };
    let app = MessageApplication::decode(app_bytes.as_slice()).map_err(|e| anyhow!("MessageApplication: {e}"))?;
    let reply_to = app.metadata.and_then(|m| m.quoted_message).and_then(|q| q.stanza_id);
    let consumer_bytes = app
        .payload
        .and_then(|p| p.sub_protocol)
        .and_then(|s| s.consumer_message)
        .and_then(|c| c.payload);
    let content = match consumer_bytes {
        Some(bytes) => consumer_content(&bytes)?,
        None => Content2::Unsupported,
    };
    Ok(Decoded { content, reply_to, destination_jid, skdm })
}

fn consumer_content(bytes: &[u8]) -> Result<Content2> {
    let consumer = ConsumerApplication::decode(bytes).map_err(|e| anyhow!("ConsumerApplication: {e}"))?;
    let payload = consumer.payload.unwrap_or_default();
    if let Some(key) = payload.application_data.and_then(|a| a.revoke).and_then(|r| r.key) {
        return Ok(Content2::Revoke { target_id: key.id.unwrap_or_default() });
    }
    let Some(c) = payload.content else { return Ok(Content2::Unsupported) };
    let media = |kind: MediaKind, m: CaptionedMedia| Content2::Media {
        kind,
        caption: text_of(m.caption),
        file_name: String::new(),
        file: media_file(m.media),
    };
    Ok(if let Some(t) = c.message_text {
        Content2::Text { text: t.text.unwrap_or_default() }
    } else if let Some(e) = c.extended_text {
        let text = text_of(e.text);
        match e.canonical_url {
            Some(url) if !url.is_empty() => Content2::Link { text, url, title: e.title.unwrap_or_default() },
            _ => Content2::Text { text },
        }
    } else if let Some(m) = c.image {
        media(MediaKind::Image, m)
    } else if let Some(m) = c.video {
        media(MediaKind::Video, m)
    } else if let Some(m) = c.audio {
        media(MediaKind::Audio, m)
    } else if let Some(m) = c.sticker {
        media(MediaKind::Sticker, m)
    } else if let Some(d) = c.document {
        Content2::Media { kind: MediaKind::File, caption: String::new(), file_name: d.file_name.unwrap_or_default(), file: media_file(d.document) }
    } else if let Some(r) = c.reaction {
        Content2::Reaction { target_id: r.key.and_then(|k| k.id).unwrap_or_default(), emoji: r.text.unwrap_or_default() }
    } else if let Some(e) = c.edit {
        Content2::Edit { target_id: e.key.and_then(|k| k.id).unwrap_or_default(), text: text_of(e.message) }
    } else {
        Content2::Unsupported
    })
}

// ---- Encoding (sending) ----

/// Version numbers of the sub-protocols, as the web client sends them.
const CONSUMER_VERSION: i32 = 1;
const APPLICATION_VERSION: i32 = 2;

/// An outgoing text message: the serialized `MessageApplication` (shared by
/// every recipient device) and its franking tag (HMAC-SHA256 of those bytes
/// under a fresh random franking key, which travels inside the message).
pub struct Outgoing {
    pub application: Vec<u8>,
    pub franking_tag: Vec<u8>,
}

pub fn encode_text(text: &str) -> Outgoing {
    encode_content(Content { message_text: Some(MessageText { text: Some(text.to_string()) }), ..Default::default() })
}

/// An uploaded image (see `e2ee_media.rs`), as the web client encodes it:
/// `imageMessage { image: SubProtocol(ImageTransport, version 1), caption }`.
pub fn encode_image(file: &MediaFile, object_id: Option<&str>, media_key_timestamp: i64, caption: &str) -> Outgoing {
    let transport = MediaTransportWrapper {
        integral: Some(TransportIntegral {
            transport: Some(WaMediaTransport {
                integral: Some(MediaIntegral {
                    file_sha256: Some(file.file_sha256.clone()),
                    media_key: Some(file.media_key.clone()),
                    file_enc_sha256: Some(file.file_enc_sha256.clone()),
                    direct_path: Some(file.direct_path.clone()),
                    media_key_timestamp: Some(media_key_timestamp),
                }),
                ancillary: Some(MediaAncillary {
                    file_length: Some(file.file_length),
                    mimetype: Some(file.mimetype.clone()),
                    thumbnail: (!file.thumbnail.is_empty()).then(|| MediaThumbnail {
                        jpeg_thumbnail: Some(file.thumbnail.clone()),
                        width: None,
                        height: None,
                    }),
                    object_id: object_id.map(str::to_string),
                }),
            }),
        }),
        ancillary: Some(ImageDims { height: Some(file.height), width: Some(file.width) }),
    };
    let caption = MessageText { text: (!caption.is_empty()).then(|| caption.to_string()) };
    encode_content(Content {
        image: Some(CaptionedMedia {
            media: Some(SubProtocol { payload: Some(transport.encode_to_vec()), version: Some(1) }),
            caption: Some(caption),
        }),
        ..Default::default()
    })
}

fn encode_content(content: Content) -> Outgoing {
    use hmac::{KeyInit, Mac};
    let consumer = ConsumerApplication { payload: Some(ConsumerPayload { content: Some(content), application_data: None }) };
    let franking_key = rand::random::<[u8; 32]>().to_vec();
    let app = MessageApplication {
        payload: Some(AppPayload {
            sub_protocol: Some(SubProtocolPayload {
                consumer_message: Some(SubProtocol { payload: Some(consumer.encode_to_vec()), version: Some(CONSUMER_VERSION) }),
                armadillo: None,
            }),
        }),
        metadata: Some(AppMetadata { franking_key: Some(franking_key.clone()), franking_version: Some(0), quoted_message: None }),
    };
    let application = app.encode_to_vec();
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&franking_key).expect("any key length");
    mac.update(&application);
    Outgoing { application, franking_tag: mac.finalize().into_bytes().to_vec() }
}

/// The per-device Signal plaintext: `MessageTransport` around the shared
/// application bytes, with random padding, a backup directive, and (only for
/// our own other devices) the chat it was sent to.
pub fn encode_transport(application: &[u8], message_id: &str, sent_to: Option<&str>) -> Vec<u8> {
    let pad = match rand::random::<u8>() & 0x0f {
        0 => 0x0f,
        n => n,
    };
    MessageTransport {
        payload: Some(TransportPayload {
            application_payload: Some(SubProtocol { payload: Some(application.to_vec()), version: Some(APPLICATION_VERSION) }),
            future_proof: Some(0),
        }),
        protocol: Some(TransportProtocol {
            integral: Some(Integral {
                padding: Some(vec![pad; pad as usize]),
                dsm: sent_to.map(|jid| DeviceSentMessage { destination_jid: Some(jid.to_string()) }),
            }),
            ancillary: Some(Ancillary {
                skdm: None,
                backup_directive: Some(BackupDirective { message_id: Some(message_id.to_string()), action_type: Some(1) }),
            }),
        }),
    }
    .encode_to_vec()
}

/// The file reference inside a media sub-protocol (`XTransport`), if complete.
fn media_file(sub: Option<SubProtocol>) -> Option<MediaFile> {
    let bytes = sub?.payload?;
    let wrapper = MediaTransportWrapper::decode(bytes.as_slice()).ok()?;
    let transport = wrapper.integral?.transport?;
    let integral = transport.integral?;
    let ancillary = transport.ancillary.unwrap_or_default();
    let thumb = ancillary.thumbnail.unwrap_or_default();
    let dims = wrapper.ancillary.unwrap_or_default();
    Some(MediaFile {
        direct_path: integral.direct_path.filter(|p| !p.is_empty())?,
        media_key: integral.media_key.filter(|k| k.len() == 32)?,
        file_sha256: integral.file_sha256.unwrap_or_default(),
        file_enc_sha256: integral.file_enc_sha256.unwrap_or_default(),
        mimetype: ancillary.mimetype.unwrap_or_default(),
        file_length: ancillary.file_length.unwrap_or(0),
        width: dims.width.or(thumb.width).unwrap_or(0),
        height: dims.height.or(thumb.height).unwrap_or(0),
        thumbnail: thumb.jpeg_thumbnail.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap_consumer(content: Content, quoted: Option<&str>) -> Vec<u8> {
        let consumer = ConsumerApplication { payload: Some(ConsumerPayload { content: Some(content), application_data: None }) };
        let app = MessageApplication {
            payload: Some(AppPayload {
                sub_protocol: Some(SubProtocolPayload {
                    consumer_message: Some(SubProtocol { payload: Some(consumer.encode_to_vec()), version: Some(1) }),
                    armadillo: None,
                }),
            }),
            metadata: quoted.map(|q| AppMetadata { quoted_message: Some(QuotedMessage { stanza_id: Some(q.into()) }), ..Default::default() }),
        };
        MessageTransport {
            payload: Some(TransportPayload { application_payload: Some(SubProtocol { payload: Some(app.encode_to_vec()), version: Some(1) }), future_proof: None }),
            protocol: Some(TransportProtocol {
                integral: Some(Integral { padding: None, dsm: Some(DeviceSentMessage { destination_jid: Some("42@msgr".into()) }) }),
                ancillary: None,
            }),
        }
        .encode_to_vec()
    }

    #[test]
    fn decodes_text_reply_and_dsm() {
        let bytes = wrap_consumer(Content { message_text: Some(MessageText { text: Some("xin chào".into()) }), ..Default::default() }, Some("mid.1"));
        let d = decode(&bytes).unwrap();
        assert_eq!(d.content, Content2::Text { text: "xin chào".into() });
        assert_eq!(d.reply_to.as_deref(), Some("mid.1"));
        assert_eq!(d.destination_jid.as_deref(), Some("42@msgr"));
    }

    #[test]
    fn decodes_reaction() {
        let r = ReactionMessage { key: Some(MessageKey { id: Some("abc".into()), ..Default::default() }), text: Some("❤".into()) };
        let d = decode(&wrap_consumer(Content { reaction: Some(r), ..Default::default() }, None)).unwrap();
        assert_eq!(d.content, Content2::Reaction { target_id: "abc".into(), emoji: "❤".into() });
    }

    #[test]
    fn encoded_text_decodes_back() {
        let out = encode_text("chào");
        assert_eq!(out.franking_tag.len(), 32);
        let d = decode(&encode_transport(&out.application, "123", Some("42@msgr"))).unwrap();
        assert_eq!(d.content, Content2::Text { text: "chào".into() });
        assert_eq!(d.destination_jid.as_deref(), Some("42@msgr"));
        assert_eq!(decode(&encode_transport(&out.application, "123", None)).unwrap().destination_jid, None);
    }

    #[test]
    fn encoded_image_decodes_back() {
        let file = MediaFile {
            direct_path: "/v/t62/abc".into(),
            media_key: vec![7; 32],
            file_sha256: vec![1; 32],
            file_enc_sha256: vec![2; 32],
            mimetype: "image/jpeg".into(),
            file_length: 1234,
            width: 640,
            height: 480,
            thumbnail: vec![0xff, 0xd8],
        };
        let out = encode_image(&file, Some("obj"), 1, "");
        match decode(&encode_transport(&out.application, "1", None)).unwrap().content {
            Content2::Media { kind: MediaKind::Image, file: Some(back), .. } => assert_eq!(back, file),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn transport_without_payload_is_none() {
        let t = MessageTransport { payload: None, protocol: None }.encode_to_vec();
        assert_eq!(decode(&t).unwrap().content, Content2::None);
    }
}
