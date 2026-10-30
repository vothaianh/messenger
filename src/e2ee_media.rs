//! E2EE media (images, videos, voice, files) in Messenger's encrypted chats.
//!
//! Same scheme as WhatsApp: each file gets a random 32-byte media key;
//! HKDF-SHA256 (info per media type) expands it to an IV, an AES-256 key and an
//! HMAC key. The uploaded blob is AES-256-CBC(PKCS#7) ciphertext followed by
//! the first 10 bytes of HMAC-SHA256(iv || ciphertext). The message carries the
//! media key, the blob's direct path and the SHA-256 of the plain and the
//! encrypted file. Hosts come from `media_conn` (see `e2ee_listen.rs`).

use std::path::PathBuf;

use aes::cipher::{block_padding::Pkcs7, BlockModeDecrypt, BlockModeEncrypt, KeyIvInit};
use anyhow::{anyhow, bail, Result};
use hmac::{KeyInit, Mac};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MediaType {
    Image,
    Video,
    Audio,
    Document,
}

impl MediaType {
    fn info(self) -> &'static [u8] {
        match self {
            MediaType::Image => b"WhatsApp Image Keys",
            MediaType::Video => b"WhatsApp Video Keys",
            MediaType::Audio => b"WhatsApp Audio Keys",
            MediaType::Document => b"WhatsApp Document Keys",
        }
    }
}

struct Keys {
    iv: [u8; 16],
    cipher: [u8; 32],
    mac: [u8; 32],
}

fn expand(media_key: &[u8], kind: MediaType) -> Result<Keys> {
    let mut okm = [0u8; 112];
    hkdf::Hkdf::<Sha256>::new(None, media_key)
        .expand(kind.info(), &mut okm)
        .map_err(|_| anyhow!("HKDF expand failed"))?;
    let mut keys = Keys { iv: [0; 16], cipher: [0; 32], mac: [0; 32] };
    keys.iv.copy_from_slice(&okm[..16]);
    keys.cipher.copy_from_slice(&okm[16..48]);
    keys.mac.copy_from_slice(&okm[48..80]);
    Ok(keys)
}

fn mac10(keys: &Keys, ciphertext: &[u8]) -> [u8; 10] {
    let mut mac = hmac::Hmac::<Sha256>::new_from_slice(&keys.mac).expect("any key length");
    mac.update(&keys.iv);
    mac.update(ciphertext);
    let full = mac.finalize().into_bytes();
    let mut out = [0u8; 10];
    out.copy_from_slice(&full[..10]);
    out
}

/// Check and decrypt a downloaded blob.
pub fn decrypt(blob: &[u8], media_key: &[u8], kind: MediaType, file_enc_sha256: Option<&[u8]>, file_sha256: Option<&[u8]>) -> Result<Vec<u8>> {
    if blob.len() < 10 + 16 {
        bail!("media blob too short ({} bytes)", blob.len());
    }
    if let Some(expected) = file_enc_sha256 {
        if Sha256::digest(blob).as_slice() != expected {
            bail!("encrypted media hash mismatch");
        }
    }
    let keys = expand(media_key, kind)?;
    let (ciphertext, mac) = blob.split_at(blob.len() - 10);
    if mac10(&keys, ciphertext) != mac {
        bail!("media MAC mismatch");
    }
    let plain = cbc::Decryptor::<aes::Aes256>::new(&keys.cipher.into(), &keys.iv.into())
        .decrypt_padded_vec::<Pkcs7>(ciphertext)
        .map_err(|_| anyhow!("media padding is invalid"))?;
    if let Some(expected) = file_sha256 {
        if Sha256::digest(&plain).as_slice() != expected {
            bail!("media hash mismatch");
        }
    }
    Ok(plain)
}

/// An encrypted file ready to upload, with what the message must carry.
pub struct Encrypted {
    pub media_key: [u8; 32],
    pub blob: Vec<u8>,
    pub file_sha256: [u8; 32],
    pub file_enc_sha256: [u8; 32],
}

pub fn encrypt(plain: &[u8], kind: MediaType) -> Result<Encrypted> {
    let media_key: [u8; 32] = rand::random();
    let keys = expand(&media_key, kind)?;
    let mut blob = cbc::Encryptor::<aes::Aes256>::new(&keys.cipher.into(), &keys.iv.into()).encrypt_padded_vec::<Pkcs7>(plain);
    let mac = mac10(&keys, &blob);
    blob.extend_from_slice(&mac);
    Ok(Encrypted {
        media_key,
        file_sha256: Sha256::digest(plain).into(),
        file_enc_sha256: Sha256::digest(&blob).into(),
        blob,
    })
}

/// Where decrypted media is cached (named by the plain file's SHA-256).
pub fn cache_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?;
    let dir = PathBuf::from(home).join("Library/Caches/Messenger Desktop/e2ee-media");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Cache path of a decrypted file (named by its SHA-256, extension from the MIME type).
pub fn cached_path(file: &crate::armadillo::MediaFile) -> Option<PathBuf> {
    let hash = if file.file_sha256.is_empty() { &file.file_enc_sha256 } else { &file.file_sha256 };
    if hash.is_empty() {
        return None;
    }
    let name: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    let ext = file.mimetype.split('/').nth(1).and_then(|e| e.split(';').next()).unwrap_or("bin");
    Some(cache_dir().ok()?.join(format!("{name}.{ext}")))
}

/// Download `direct_path` from the first media host that answers.
pub async fn download(http: &reqwest::Client, hosts: &[String], direct_path: &str) -> Result<Vec<u8>> {
    let mut last = anyhow!("no media hosts");
    for host in hosts.iter().filter(|h| !h.starts_with("rupload")) {
        let url = format!("https://{host}{direct_path}");
        match http.get(&url).send().await.and_then(|r| r.error_for_status()) {
            Ok(resp) => return Ok(resp.bytes().await?.to_vec()),
            Err(e) => last = anyhow!("{host}: {e}"),
        }
    }
    Err(last)
}

impl From<&crate::armadillo::MediaKind> for MediaType {
    fn from(kind: &crate::armadillo::MediaKind) -> Self {
        use crate::armadillo::MediaKind;
        match kind {
            MediaKind::Image | MediaKind::Sticker => MediaType::Image,
            MediaKind::Video => MediaType::Video,
            MediaKind::Audio => MediaType::Audio,
            MediaKind::File => MediaType::Document,
        }
    }
}

/// Make sure the decrypted file is in the cache: download, check and decrypt
/// it if needed. Returns its path. Files are written 0600, like the inbox.
pub async fn fetch_to_cache(http: &reqwest::Client, hosts: &[String], file: &crate::armadillo::MediaFile, kind: MediaType) -> Result<PathBuf> {
    let path = cached_path(file).ok_or_else(|| anyhow!("media without a hash"))?;
    if path.exists() {
        return Ok(path);
    }
    let blob = download(http, hosts, &file.direct_path).await?;
    fn opt(v: &[u8]) -> Option<&[u8]> {
        (!v.is_empty()).then_some(v)
    }
    let plain = decrypt(&blob, &file.media_key, kind, opt(&file.file_enc_sha256), opt(&file.file_sha256))?;
    write_private(&path, &plain)?;
    Ok(path)
}

/// Write a cache file readable only by us (0600), atomically.
pub fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("part");
    let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// A picture to send: its bytes, MIME type, size and a small JPEG preview.
pub struct PreparedImage {
    pub bytes: Vec<u8>,
    pub mimetype: String,
    pub width: u32,
    pub height: u32,
    pub thumbnail: Vec<u8>,
}

pub fn prepare_image(path: &std::path::Path) -> Result<PreparedImage> {
    // Like the web client (MAWImagePreProcess), send JPEG: phones show the
    // encrypted image only as "Attachment" when it's a PNG. Big pictures are
    // scaled to fit MAX_SIDE; transparency goes onto white.
    const MAX_SIDE: u32 = 2560;
    let original = std::fs::read(path)?;
    let format = image::guess_format(&original).map_err(|_| anyhow!("not a supported image"))?;
    let mut img = image::load_from_memory_with_format(&original, format)?;
    let bytes = if format == image::ImageFormat::Jpeg && img.width().max(img.height()) <= MAX_SIDE {
        original
    } else {
        if img.width().max(img.height()) > MAX_SIDE {
            img = img.resize(MAX_SIDE, MAX_SIDE, image::imageops::FilterType::Lanczos3);
        }
        let rgba = img.to_rgba8();
        let mut rgb = image::RgbImage::new(rgba.width(), rgba.height());
        for (dst, src) in rgb.pixels_mut().zip(rgba.pixels()) {
            let a = src[3] as u32;
            *dst = image::Rgb([0, 1, 2].map(|i| ((src[i] as u32 * a + 255 * (255 - a)) / 255) as u8));
        }
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90).encode_image(&rgb)?;
        img = image::DynamicImage::ImageRgb8(rgb);
        out
    };
    let mimetype = "image/jpeg".to_string();
    let thumb = img.thumbnail(100, 100).to_rgb8();
    let mut thumbnail = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut thumbnail, 70).encode_image(&thumb)?;
    Ok(PreparedImage { width: img.width(), height: img.height(), bytes, mimetype, thumbnail })
}

/// Where an uploaded file ended up.
pub struct Uploaded {
    pub direct_path: String,
    pub object_id: Option<String>,
}

/// Upload an encrypted blob like the web client: POST to
/// `https://<upload host>/wa-msgr/mms/<type>/<b64url(enc sha256)>?auth=..&token=..`.
pub async fn upload(http: &reqwest::Client, hosts: &[String], auth: &str, kind: MediaType, enc: &Encrypted) -> Result<Uploaded> {
    use base64::Engine;
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(enc.file_enc_sha256);
    let mms = match kind {
        MediaType::Image => "image",
        MediaType::Video => "video",
        MediaType::Audio => "audio",
        MediaType::Document => "document",
    };
    let host = hosts.iter().find(|h| h.starts_with("rupload")).or(hosts.first()).ok_or_else(|| anyhow!("no upload host"))?;
    let url = format!("https://{host}/wa-msgr/mms/{mms}/{token}?auth={}&token={token}", urlencoding::encode(auth));
    let resp = http
        .post(&url)
        .header("Content-Type", "text/plain")
        .header("Origin", crate::session::BASE)
        .body(enc.blob.clone())
        .send()
        .await?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    if !status.is_success() {
        bail!("upload failed: HTTP {status}");
    }
    let direct_path = body["direct_path"].as_str().ok_or_else(|| anyhow!("upload answer has no direct_path"))?.to_string();
    Ok(Uploaded { direct_path, object_id: body["object_id"].as_str().map(str::to_string) })
}

#[cfg(test)]
mod tests {

    #[test]
    fn png_is_sent_as_jpeg() {
        let path = std::env::temp_dir().join(format!("md-test-{}.png", std::process::id()));
        image::RgbaImage::from_pixel(40, 30, image::Rgba([255, 0, 0, 128])).save(&path).unwrap();
        let img = super::prepare_image(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(img.mimetype, "image/jpeg");
        assert_eq!(image::guess_format(&img.bytes).unwrap(), image::ImageFormat::Jpeg);
        assert_eq!((img.width, img.height), (40, 30));
    }

    use super::*;

    #[test]
    fn encrypt_then_decrypt_roundtrips() {
        let plain = b"not really a jpeg, but bytes all the same".repeat(10);
        let enc = encrypt(&plain, MediaType::Image).unwrap();
        let back = decrypt(&enc.blob, &enc.media_key, MediaType::Image, Some(&enc.file_enc_sha256), Some(&enc.file_sha256)).unwrap();
        assert_eq!(back, plain);
    }

    #[test]
    fn tampering_is_detected() {
        let enc = encrypt(b"hello media", MediaType::Document).unwrap();
        let mut blob = enc.blob.clone();
        blob[0] ^= 1;
        assert!(decrypt(&blob, &enc.media_key, MediaType::Document, None, None).is_err());
        assert!(decrypt(&enc.blob, &enc.media_key, MediaType::Image, None, None).is_err(), "wrong media type keys");
    }
}
