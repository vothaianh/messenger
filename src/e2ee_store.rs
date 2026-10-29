//! Persistent Signal protocol store for the E2EE device: our one-time prekeys,
//! signed prekey, peer identities, sessions and group sender keys.
//!
//! Stored as JSON (records base64-encoded) in
//! `~/Library/Application Support/Messenger Desktop/e2ee-store.json`, mode
//! 0600, rewritten after every change so a crash never loses ratchet state.
//! Our identity key itself stays in the Keychain (`e2ee::Device`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use wacore_libsignal::protocol::{
    Direction, GenericSignedPreKey, IdentityChange, IdentityKey, IdentityKeyPair, IdentityKeyStore, KeyPair,
    PreKeyId, PreKeyRecord, PreKeyStore, PrivateKey, ProtocolAddress, PublicKey, SenderKeyName, SenderKeyRecord,
    SenderKeyStore, SessionRecord, SessionStore, SignalProtocolError, SignedPreKeyId, SignedPreKeyRecord,
    SignedPreKeyStore, Timestamp,
};

use crate::e2ee::Device;

type SignalResult<T> = std::result::Result<T, SignalProtocolError>;

#[derive(Serialize, Deserialize, Default)]
struct Data {
    /// Next one-time prekey id to generate (ids are 24-bit on the wire).
    next_prekey_id: u32,
    /// Prekeys generated and not yet uploaded (`id <= uploaded_through` are on the server).
    uploaded_through: u32,
    prekeys: BTreeMap<u32, String>,
    signed_prekey: Option<(u32, String)>,
    sessions: BTreeMap<String, String>,
    identities: BTreeMap<String, String>,
    sender_keys: BTreeMap<String, String>,
}

struct Inner {
    path: PathBuf,
    data: Data,
}

/// Cheap-to-clone handle. libsignal's decrypt takes the session, identity,
/// prekey and signed-prekey stores as separate `&mut dyn` arguments, so we pass
/// one clone per role; they share state through the mutex, which is only held
/// inside each (non-awaiting) method body.
#[derive(Clone)]
pub struct SignalStore {
    inner: Arc<Mutex<Inner>>,
    identity: IdentityKeyPair,
    registration_id: u32,
}

fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}
fn unb64(s: &str) -> SignalResult<Vec<u8>> {
    STANDARD.decode(s).map_err(|_| SignalProtocolError::InvalidState("store", "bad base64".into()))
}

pub fn data_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?;
    let dir = PathBuf::from(home).join("Library/Application Support/Messenger Desktop");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

impl SignalStore {
    /// Open (or create) the store for `device`, seeding the signed prekey from
    /// the keys registered via ICDC.
    pub fn open(device: &Device) -> Result<SignalStore> {
        let path = data_dir()?.join("e2ee-store.json");
        let mut data: Data = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("reading e2ee-store.json")?,
            Err(_) => Data::default(),
        };
        let private = PrivateKey::deserialize(&device.identity_priv).map_err(|e| anyhow!("identity key: {e:?}"))?;
        let public = PublicKey::from_djb_public_key_bytes(&device.identity_pub).map_err(|e| anyhow!("identity key: {e:?}"))?;
        let identity = IdentityKeyPair::new(IdentityKey::new(public), private);

        if data.signed_prekey.is_none() {
            let sk_private = PrivateKey::deserialize(&device.signed_prekey_priv).map_err(|e| anyhow!("{e:?}"))?;
            let sk_public = PublicKey::from_djb_public_key_bytes(&device.signed_prekey_pub).map_err(|e| anyhow!("{e:?}"))?;
            let record = SignedPreKeyRecord::new(
                SignedPreKeyId::from(device.signed_prekey_id),
                Timestamp::from_epoch_millis(crate::store::now_ms() as u64),
                &KeyPair::new(sk_public, sk_private),
                &device.signed_prekey_sig,
            );
            let bytes = record.serialize().map_err(|e| anyhow!("{e:?}"))?;
            data.signed_prekey = Some((device.signed_prekey_id, b64(&bytes)));
        }
        if data.next_prekey_id == 0 {
            data.next_prekey_id = 1;
        }
        let inner = Inner { path, data };
        inner.persist()?;
        Ok(SignalStore { inner: Arc::new(Mutex::new(inner)), identity, registration_id: device.registration_id })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Generate `count` new one-time prekeys. Returns (id, public key) pairs to upload.
    pub fn generate_prekeys(&self, count: u32) -> Result<Vec<(u32, [u8; 32])>> {
        let mut inner = self.lock();
        let mut out = Vec::new();
        for _ in 0..count {
            let id = inner.data.next_prekey_id;
            inner.data.next_prekey_id = if id >= 0xFF_FFFF { 1 } else { id + 1 };
            let kp = KeyPair::generate(&mut rand::rng());
            let record = PreKeyRecord::new(PreKeyId::from(id), &kp);
            inner.data.prekeys.insert(id, b64(&record.serialize().map_err(|e| anyhow!("{e:?}"))?));
            let public: [u8; 32] = kp.public_key.public_key_bytes().try_into().map_err(|_| anyhow!("bad prekey length"))?;
            out.push((id, public));
        }
        inner.persist()?;
        Ok(out)
    }

    pub fn mark_uploaded(&self, through: u32) -> Result<()> {
        let mut inner = self.lock();
        inner.data.uploaded_through = through;
        inner.persist()
    }

    pub fn registration_id(&self) -> u32 {
        self.registration_id
    }
}

impl Inner {
    fn persist(&self) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = self.path.with_extension("json.tmp");
        let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(&serde_json::to_vec(&self.data)?)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn persist_signal(&self) -> SignalResult<()> {
        self.persist().map_err(|e| SignalProtocolError::InvalidState("store", e.to_string()))
    }
}

#[async_trait]
impl IdentityKeyStore for SignalStore {
    async fn get_identity_key_pair(&self) -> SignalResult<IdentityKeyPair> {
        Ok(self.identity.clone())
    }

    async fn get_local_registration_id(&self) -> SignalResult<u32> {
        Ok(self.registration_id)
    }

    async fn save_identity(&mut self, address: &ProtocolAddress, identity: &IdentityKey) -> SignalResult<IdentityChange> {
        let key = address.to_string();
        let new = b64(&identity.serialize());
        let mut inner = self.lock();
        let changed = inner.data.identities.get(&key).is_some_and(|old| old != &new);
        inner.data.identities.insert(key, new);
        inner.persist_signal()?;
        Ok(IdentityChange::from_changed(changed))
    }

    async fn is_trusted_identity(&self, _address: &ProtocolAddress, _identity: &IdentityKey, _direction: Direction) -> SignalResult<bool> {
        // Trust on first use, and accept key changes (like the Messenger clients do;
        // the UI can surface "security code changed" later).
        Ok(true)
    }

    async fn get_identity(&self, address: &ProtocolAddress) -> SignalResult<Option<IdentityKey>> {
        match self.lock().data.identities.get(&address.to_string()) {
            Some(s) => Ok(Some(IdentityKey::decode(&unb64(&s)?)?)),
            None => Ok(None),
        }
    }
}

#[async_trait]
impl PreKeyStore for SignalStore {
    async fn get_pre_key(&self, prekey_id: PreKeyId) -> SignalResult<PreKeyRecord> {
        let id: u32 = prekey_id.into();
        let s = self.lock().data.prekeys.get(&id).cloned().ok_or(SignalProtocolError::InvalidPreKeyId)?;
        PreKeyRecord::deserialize(&unb64(&s)?)
    }

    async fn save_pre_key(&mut self, prekey_id: PreKeyId, record: &PreKeyRecord) -> SignalResult<()> {
        let mut inner = self.lock();
        inner.data.prekeys.insert(prekey_id.into(), b64(&record.serialize()?));
        inner.persist_signal()
    }

    async fn remove_pre_key(&mut self, prekey_id: PreKeyId) -> SignalResult<()> {
        let mut inner = self.lock();
        inner.data.prekeys.remove(&u32::from(prekey_id));
        inner.persist_signal()
    }
}

#[async_trait]
impl SignedPreKeyStore for SignalStore {
    async fn get_signed_pre_key(&self, id: SignedPreKeyId) -> SignalResult<SignedPreKeyRecord> {
        let stored = self.lock().data.signed_prekey.clone();
        match stored {
            Some((stored, s)) if stored == u32::from(id) => SignedPreKeyRecord::deserialize(&unb64(&s)?),
            _ => Err(SignalProtocolError::InvalidSignedPreKeyId),
        }
    }

    async fn save_signed_pre_key(&mut self, id: SignedPreKeyId, record: &SignedPreKeyRecord) -> SignalResult<()> {
        let mut inner = self.lock();
        inner.data.signed_prekey = Some((id.into(), b64(&record.serialize()?)));
        inner.persist_signal()
    }
}

#[async_trait]
impl SessionStore for SignalStore {
    async fn load_session(&self, address: &ProtocolAddress) -> SignalResult<Option<SessionRecord>> {
        let stored = self.lock().data.sessions.get(&address.to_string()).cloned();
        match stored {
            Some(s) => Ok(Some(SessionRecord::deserialize(&unb64(&s)?)?)),
            None => Ok(None),
        }
    }

    async fn has_session(&self, address: &ProtocolAddress) -> SignalResult<bool> {
        Ok(self.lock().data.sessions.contains_key(&address.to_string()))
    }

    async fn store_session(&mut self, address: &ProtocolAddress, record: SessionRecord) -> SignalResult<()> {
        let mut inner = self.lock();
        inner.data.sessions.insert(address.to_string(), b64(&record.serialize()?));
        inner.persist_signal()
    }
}

#[async_trait]
impl SenderKeyStore for SignalStore {
    async fn store_sender_key(&mut self, name: &SenderKeyName, record: SenderKeyRecord) -> SignalResult<()> {
        let mut inner = self.lock();
        inner.data.sender_keys.insert(sender_key_id(name), b64(&record.serialize()?));
        inner.persist_signal()
    }

    async fn load_sender_key(&self, name: &SenderKeyName) -> SignalResult<Option<SenderKeyRecord>> {
        let stored = self.lock().data.sender_keys.get(&sender_key_id(name)).cloned();
        match stored {
            Some(s) => Ok(Some(SenderKeyRecord::deserialize(&unb64(&s)?)?)),
            None => Ok(None),
        }
    }
}

fn sender_key_id(name: &SenderKeyName) -> String {
    format!("{name:?}")
}
