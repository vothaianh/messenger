//! End-to-end encrypted (Armadillo) device: Signal keys and ICDC registration.
//!
//! Messenger E2EE runs the WhatsApp multi-device protocol. Each client is a
//! separate device with its own Signal identity, registered with Meta via
//! ICDC ("identity change detection"): we fetch the account's current device
//! identity list, append our identity key, sign the new list with it and post
//! it together with our registration id and signed prekey. See docs/E2EE.md.

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use prost::Message as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use xeddsa::Sign;

use crate::session::{Session, BASE, USER_AGENT};

const KEYCHAIN_SERVICE: &str = "messenger-rs";
const KEYCHAIN_ACCOUNT: &str = "e2ee-device";
const ICDC_FETCH: &str = "https://reg-e2ee.facebook.com/v2/fb_icdc_fetch";
const ICDC_REGISTER: &str = "https://reg-e2ee.facebook.com/v2/fb_register_v2";
/// Signal's type byte for Curve25519 ("DJB") public keys.
const DJB_TYPE: u8 = 0x05;

/// Our E2EE device. Persisted in the Keychain *before* registering, so a
/// registered device can never lose its keys.
#[derive(Serialize, Deserialize, Clone)]
pub struct Device {
    /// Random per-device UUID Meta knows us by (`device_id` in ICDC requests).
    pub device_uuid: String,
    pub registration_id: u32,
    #[serde(with = "b64")]
    pub identity_priv: [u8; 32],
    #[serde(with = "b64")]
    pub identity_pub: [u8; 32],
    pub signed_prekey_id: u32,
    #[serde(with = "b64")]
    pub signed_prekey_priv: [u8; 32],
    #[serde(with = "b64")]
    pub signed_prekey_pub: [u8; 32],
    #[serde(with = "b64_64")]
    pub signed_prekey_sig: [u8; 64],
    /// Device number assigned by ICDC registration; our JID is `<fbid>.<n>@msgr`.
    pub wa_device_id: Option<u16>,
    pub fbid: Option<i64>,
    /// Static Noise key for the chat socket handshake (separate from the
    /// Signal identity). Added after registration, hence optional.
    #[serde(default, with = "b64_opt")]
    pub noise_priv: Option<[u8; 32]>,
}

impl Device {
    pub fn generate() -> Device {
        let (identity_priv, identity_pub) = keypair();
        let (signed_prekey_priv, signed_prekey_pub) = keypair();
        // Signal signs the prekey's serialized form: type byte + 32-byte key.
        let mut to_sign = vec![DJB_TYPE];
        to_sign.extend_from_slice(&signed_prekey_pub);
        let signed_prekey_sig = sign(&identity_priv, &to_sign);
        Device {
            device_uuid: uuid::Uuid::new_v4().to_string(),
            registration_id: 1 + rand::random::<u32>() % 16380,
            identity_priv,
            identity_pub,
            signed_prekey_id: 1,
            signed_prekey_priv,
            signed_prekey_pub,
            signed_prekey_sig,
            wa_device_id: None,
            fbid: None,
            noise_priv: Some(keypair().0),
        }
    }

    pub fn load() -> Option<Device> {
        let bytes = security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn save(&self) -> Result<()> {
        let json = serde_json::to_vec(self)?;
        let _ = security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT);
        security_framework::passwords::set_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, &json)
            .context("saving E2EE device keys to Keychain")
    }

    /// The static Noise private key, creating and saving one if missing.
    pub fn ensure_noise_key(&mut self) -> Result<[u8; 32]> {
        if let Some(k) = self.noise_priv {
            return Ok(k);
        }
        let k = keypair().0;
        self.noise_priv = Some(k);
        self.save()?;
        Ok(k)
    }

    pub fn jid(&self) -> Option<String> {
        Some(format!("{}.{}@msgr", self.fbid?, self.wa_device_id?))
    }
}

/// A clamped Curve25519 key pair (private, public).
fn keypair() -> ([u8; 32], [u8; 32]) {
    // Clamp like Signal does, so the stored private key is the canonical scalar.
    let mut private: [u8; 32] = rand::random();
    private[0] &= 248;
    private[31] &= 127;
    private[31] |= 64;
    let public = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(private));
    (private, public.to_bytes())
}

/// XEdDSA signature with a Curve25519 private key (Signal's `calculateSignature`).
fn sign(private: &[u8; 32], message: &[u8]) -> [u8; 64] {
    xeddsa::xed25519::PrivateKey::from(private).sign(message, rand::rng())
}

// ICDC identity list protobufs (WAArmadilloICDC).
#[derive(Clone, PartialEq, prost::Message)]
struct IcdcIdentityList {
    #[prost(int32, optional, tag = "1")]
    seq: Option<i32>,
    #[prost(int64, optional, tag = "2")]
    timestamp: Option<i64>,
    #[prost(bytes = "vec", repeated, tag = "3")]
    devices: Vec<Vec<u8>>,
    #[prost(int32, optional, tag = "4")]
    signing_device_index: Option<i32>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct SignedIcdcIdentityList {
    #[prost(bytes = "vec", optional, tag = "1")]
    details: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    signature: Option<Vec<u8>>,
}

#[derive(Deserialize, Debug)]
pub struct IcdcFetch {
    #[serde(default)]
    pub device_identities: Vec<String>,
    #[serde(default)]
    pub icdc_seq: i32,
    #[serde(default)]
    pub status: i32,
}

impl IcdcFetch {
    pub fn identities(&self) -> Result<Vec<[u8; 32]>> {
        self.device_identities
            .iter()
            .map(|s| {
                let raw = URL_SAFE_NO_PAD.decode(s.trim_end_matches('='))?;
                raw.try_into().map_err(|_| anyhow!("device identity is not 32 bytes"))
            })
            .collect()
    }
}

#[derive(Deserialize, Debug)]
struct IcdcRegister {
    #[serde(default)]
    status: i32,
    #[serde(default)]
    wa_device_id: Option<u16>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

async fn post_form<T: for<'de> Deserialize<'de>>(session: &Session, url: &str, form: &[(&str, String)]) -> Result<T> {
    let resp = session
        .http
        .post(url)
        .header("Cookie", &session.cookies.0)
        .header("Origin", BASE)
        .header("Referer", format!("{BASE}/messages/"))
        .header("User-Agent", USER_AGENT)
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Sec-Fetch-Dest", "empty")
        .header("Sec-Fetch-Mode", "cors")
        .header("Sec-Fetch-Site", "same-site")
        .form(form)
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        bail!("{url} returned HTTP {status}: {}", body.chars().take(200).collect::<String>());
    }
    serde_json::from_str(&body).with_context(|| format!("unexpected response from {url}: {}", body.chars().take(200).collect::<String>()))
}

fn require_cat(session: &Session) -> Result<()> {
    if session.boot.e2ee_cat.is_empty() || session.boot.e2ee_app_id.is_empty() {
        bail!("the Messenger page didn't include an E2EE crypto auth token");
    }
    Ok(())
}

/// Read-only: the account's current E2EE device identities and ICDC sequence.
pub async fn fetch_icdc(session: &Session, device: &Device) -> Result<IcdcFetch> {
    require_cat(session)?;
    let form = [
        ("fbid", session.boot.fbid.to_string()),
        ("fb_cat", session.boot.e2ee_cat.clone()),
        ("app_id", session.boot.e2ee_app_id.clone()),
        ("device_id", device.device_uuid.clone()),
    ];
    let resp: IcdcFetch = post_form(session, ICDC_FETCH, &form).await?;
    if resp.status != 200 {
        bail!("ICDC fetch returned status {}", resp.status);
    }
    Ok(resp)
}

/// Register `device` as a new E2EE device on the account. Returns the assigned
/// device number. The caller must have saved `device` beforehand.
pub async fn register(session: &Session, device: &Device, current: &IcdcFetch) -> Result<u16> {
    require_cat(session)?;
    let mut identities = current.identities()?;
    let mut seq = current.icdc_seq;
    let own_index = match identities.iter().position(|k| k == &device.identity_pub) {
        Some(i) => i,
        None => {
            identities.push(device.identity_pub);
            seq += 1;
            identities.len() - 1
        }
    };
    let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
    let details = IcdcIdentityList {
        seq: Some(seq),
        timestamp: Some(timestamp),
        devices: identities.iter().map(|k| k.to_vec()).collect(),
        signing_device_index: Some(own_index as i32),
    }
    .encode_to_vec();
    let signature = sign(&device.identity_priv, &details);
    let signed = SignedIcdcIdentityList { details: Some(details), signature: Some(signature.to_vec()) }.encode_to_vec();

    // ihash: first 10 bytes of SHA-256 over the sorted, concatenated identities.
    let mut sorted = identities.clone();
    sorted.sort();
    let ihash = &Sha256::digest(sorted.concat())[..10];

    let form = [
        ("fbid", session.boot.fbid.to_string()),
        ("fb_cat", session.boot.e2ee_cat.clone()),
        ("app_id", session.boot.e2ee_app_id.clone()),
        ("device_id", device.device_uuid.clone()),
        ("e_regid", STANDARD.encode(device.registration_id.to_be_bytes())),
        ("e_keytype", STANDARD.encode([DJB_TYPE])),
        ("e_ident", STANDARD.encode(device.identity_pub)),
        ("e_skey_id", STANDARD.encode(&device.signed_prekey_id.to_be_bytes()[1..])),
        ("e_skey_val", STANDARD.encode(device.signed_prekey_pub)),
        ("e_skey_sig", STANDARD.encode(device.signed_prekey_sig)),
        ("icdc_list", STANDARD.encode(&signed)),
        ("icdc_ts", timestamp.to_string()),
        ("icdc_seq", seq.to_string()),
        ("ihash", STANDARD.encode(ihash)),
    ];
    let resp: IcdcRegister = post_form(session, ICDC_REGISTER, &form).await?;
    if resp.status != 200 {
        bail!("ICDC registration returned status {} ({:?})", resp.status, resp.kind);
    }
    resp.wa_device_id.ok_or_else(|| anyhow!("ICDC registration response had no device id"))
}

mod b64 {
    use super::*;
    pub fn serialize<S: serde::Serializer>(v: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(v))
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        STANDARD.decode(s).map_err(serde::de::Error::custom)?.try_into().map_err(|_| serde::de::Error::custom("expected 32 bytes"))
    }
}

mod b64_opt {
    use super::*;
    pub fn serialize<S: serde::Serializer>(v: &Option<[u8; 32]>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(k) => s.serialize_some(&STANDARD.encode(k)),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<[u8; 32]>, D::Error> {
        let Some(s) = Option::<String>::deserialize(d)? else { return Ok(None) };
        let raw = STANDARD.decode(s).map_err(serde::de::Error::custom)?;
        raw.try_into().map(Some).map_err(|_| serde::de::Error::custom("expected 32 bytes"))
    }
}

mod b64_64 {
    use super::*;
    pub fn serialize<S: serde::Serializer>(v: &[u8; 64], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(v))
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<[u8; 64], D::Error> {
        let s = String::deserialize(d)?;
        STANDARD.decode(s).map_err(serde::de::Error::custom)?.try_into().map_err(|_| serde::de::Error::custom("expected 64 bytes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xeddsa::Verify;

    #[test]
    fn signed_prekey_signature_verifies() {
        let d = Device::generate();
        let mut msg = vec![DJB_TYPE];
        msg.extend_from_slice(&d.signed_prekey_pub);
        let pk = xeddsa::xed25519::PublicKey(d.identity_pub);
        assert!(pk.verify(&msg, &d.signed_prekey_sig).is_ok());
        assert!((1..=16380).contains(&d.registration_id));
    }

    #[test]
    fn device_roundtrips_through_json() {
        let d = Device::generate();
        let back: Device = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(back.identity_priv, d.identity_priv);
        assert_eq!(back.signed_prekey_sig, d.signed_prekey_sig);
    }

    #[test]
    fn icdc_list_encodes_like_proto2() {
        let l = IcdcIdentityList { seq: Some(3), timestamp: Some(1), devices: vec![vec![7; 32]], signing_device_index: Some(0) };
        let bytes = l.encode_to_vec();
        // field1 varint 3, field2 varint 1, field3 len 32, field4 varint 0
        assert_eq!(&bytes[..4], &[0x08, 3, 0x10, 1]);
        assert_eq!(bytes[4], 0x1a);
        assert_eq!(bytes[5], 32);
        assert_eq!(&bytes[38..], &[0x20, 0]);
    }
}
