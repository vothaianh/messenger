//! Messenger's PIN check for encrypted-chat backups ("Vesta"): an OPAQUE
//! login against Meta's HSMs. The client half is Meta's own Rust code,
//! shipped to the web app as a WASI WebAssembly module; we run that same
//! module (fetched at runtime, like the web) in `wasmi`, so the crypto is
//! byte-for-byte the web client's and a correct PIN can't fail because of us.
//!
//! The module has one export, `execute_command`: it reads a `CliCommand`
//! protobuf from the file `input` and writes a `CliResponse` to `output`.
//! The WASI layer here is in memory: those two files, no network, no disk.
//! See docs/E2EE.md for the protocol notes.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};
use prost::Message;
use wasmi::{Caller, Engine, Extern, Instance, Linker, Memory, Module, Store};

// ---- Commands (VestaWasm.pb) ----

#[derive(Clone, PartialEq, prost::Message)]
struct CliCommand {
    #[prost(message, optional, tag = "1")]
    validate_hsm_key: Option<ValidateHsmKeyRequest>,
    #[prost(message, optional, tag = "4")]
    begin_login: Option<BeginLoginRequest>,
    #[prost(message, optional, tag = "5")]
    finish_login: Option<FinishLoginRequest>,
    #[prost(message, optional, tag = "6")]
    unpack_login_payload: Option<UnpackLoginPayloadRequest>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct CliResponse {
    #[prost(int32, optional, tag = "1")]
    error_code: Option<i32>,
    #[prost(message, optional, tag = "2")]
    validate_hsm_key: Option<ValidateHsmKeyResponse>,
    #[prost(message, optional, tag = "5")]
    begin_login: Option<BeginLoginResponse>,
    #[prost(message, optional, tag = "6")]
    finish_login: Option<FinishLoginResponse>,
    #[prost(message, optional, tag = "7")]
    unpack_login_payload: Option<UnpackLoginPayloadResponse>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ValidateHsmKeyRequest {
    #[prost(bytes = "vec", optional, tag = "1")]
    hsm_key: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    signature: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ValidateHsmKeyResponse {
    #[prost(bool, optional, tag = "1")]
    is_valid: Option<bool>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct BeginLoginRequest {
    #[prost(bytes = "vec", optional, tag = "1")]
    password: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct BeginLoginResponse {
    #[prost(bytes = "vec", optional, tag = "1")]
    l1_message: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    client_state: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct FinishLoginRequest {
    #[prost(bytes = "vec", optional, tag = "1")]
    l2_message: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    l2_message_sig: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "3")]
    island_ed25519_pub: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "4")]
    client_state: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct FinishLoginResponse {
    #[prost(bytes = "vec", optional, tag = "1")]
    l3_message: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    session_key: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "3")]
    export_key: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct UnpackLoginPayloadRequest {
    #[prost(bytes = "vec", optional, tag = "1")]
    login_payload: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    session_key: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "3")]
    export_key: Option<Vec<u8>>,
    #[prost(bool, optional, tag = "4")]
    use_protobuf_mode: Option<bool>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct UnpackLoginPayloadResponse {
    #[prost(bytes = "vec", optional, tag = "1")]
    user_secret: Option<Vec<u8>>,
}

/// The module's error codes (`mapVestaClientError` in the web client).
fn error_name(code: i32) -> &'static str {
    match code {
        1 => "response buffer too small",
        2 => "incorrect PIN",
        3 => "malformed server response",
        4 => "malformed client input",
        5 => "encryption failure",
        6 => "decryption failure",
        7 => "serialization failure",
        8 => "deserialization failure",
        9 => "invalid HSM RSA key",
        10 => "HSM RSA signature invalid",
        11 => "invalid HSM Ed25519 key",
        12 => "HSM Ed25519 signature invalid",
        _ => "unknown error",
    }
}

/// Raised by the module for a wrong PIN, so callers can tell it apart.
#[derive(Debug)]
pub struct WrongPin;

impl std::fmt::Display for WrongPin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("incorrect PIN")
    }
}

impl std::error::Error for WrongPin {}

// ---- In-memory WASI ----

const ERRNO_SUCCESS: i32 = 0;
const ERRNO_BADF: i32 = 8;
const ERRNO_INVAL: i32 = 28;
const ERRNO_NOENT: i32 = 44;
/// The one preopened directory ("/"), holding `input` and `output`.
const ROOT_FD: i32 = 3;

#[derive(Default)]
struct Host {
    files: HashMap<String, Vec<u8>>,
    /// Open file descriptors: name and position.
    open: HashMap<i32, (String, usize)>,
    next_fd: i32,
}

fn memory(caller: &Caller<'_, Host>) -> Option<Memory> {
    caller.get_export("memory").and_then(Extern::into_memory)
}

fn read_bytes(caller: &Caller<'_, Host>, ptr: i32, len: i32) -> Option<Vec<u8>> {
    let mem = memory(caller)?;
    let data = mem.data(caller);
    data.get(ptr as usize..(ptr as usize).checked_add(len as usize)?).map(<[u8]>::to_vec)
}

fn write_bytes(caller: &mut Caller<'_, Host>, ptr: i32, bytes: &[u8]) -> bool {
    let Some(mem) = memory(caller) else { return false };
    let data = mem.data_mut(caller);
    match data.get_mut(ptr as usize..ptr as usize + bytes.len()) {
        Some(dst) => {
            dst.copy_from_slice(bytes);
            true
        }
        None => false,
    }
}

fn u32_at(caller: &Caller<'_, Host>, ptr: i32) -> Option<u32> {
    read_bytes(caller, ptr, 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// (pointer, length) pairs of an iovec array.
fn iovecs(caller: &Caller<'_, Host>, iovs: i32, count: i32) -> Option<Vec<(i32, i32)>> {
    (0..count).map(|i| Some((u32_at(caller, iovs + i * 8)? as i32, u32_at(caller, iovs + i * 8 + 4)? as i32))).collect()
}

fn link(linker: &mut Linker<Host>) -> Result<()> {
    const W: &str = "wasi_snapshot_preview1";
    linker.func_wrap(W, "environ_sizes_get", |mut c: Caller<'_, Host>, count: i32, size: i32| -> i32 {
        write_bytes(&mut c, count, &0u32.to_le_bytes());
        write_bytes(&mut c, size, &0u32.to_le_bytes());
        ERRNO_SUCCESS
    })?;
    linker.func_wrap(W, "environ_get", |_: Caller<'_, Host>, _: i32, _: i32| -> i32 { ERRNO_SUCCESS })?;
    linker.func_wrap(W, "fd_prestat_get", |mut c: Caller<'_, Host>, fd: i32, buf: i32| -> i32 {
        if fd != ROOT_FD {
            return ERRNO_BADF;
        }
        // Tag 0 (directory) and the name's length ("/").
        let mut out = [0u8; 8];
        out[4..].copy_from_slice(&1u32.to_le_bytes());
        write_bytes(&mut c, buf, &out);
        ERRNO_SUCCESS
    })?;
    linker.func_wrap(W, "fd_prestat_dir_name", |mut c: Caller<'_, Host>, fd: i32, path: i32, _len: i32| -> i32 {
        if fd != ROOT_FD {
            return ERRNO_BADF;
        }
        write_bytes(&mut c, path, b"/");
        ERRNO_SUCCESS
    })?;
    linker.func_wrap(
        W,
        "path_open",
        |mut c: Caller<'_, Host>, _dir: i32, _dirflags: i32, path: i32, len: i32, oflags: i32, _rb: i64, _ri: i64, _fdflags: i32, fd_out: i32| -> i32 {
            let Some(raw) = read_bytes(&c, path, len) else { return ERRNO_INVAL };
            let name = String::from_utf8_lossy(&raw).trim_start_matches("./").trim_start_matches('/').to_string();
            const CREAT: i32 = 1;
            const TRUNC: i32 = 8;
            let host = c.data_mut();
            if !host.files.contains_key(&name) {
                if oflags & CREAT == 0 {
                    return ERRNO_NOENT;
                }
                host.files.insert(name.clone(), Vec::new());
            }
            if oflags & TRUNC != 0 {
                host.files.insert(name.clone(), Vec::new());
            }
            let fd = host.next_fd.max(ROOT_FD + 1);
            host.next_fd = fd + 1;
            host.open.insert(fd, (name, 0));
            write_bytes(&mut c, fd_out, &(fd as u32).to_le_bytes());
            ERRNO_SUCCESS
        },
    )?;
    linker.func_wrap(W, "fd_read", |mut c: Caller<'_, Host>, fd: i32, iovs: i32, count: i32, nread: i32| -> i32 {
        let Some(vecs) = iovecs(&c, iovs, count) else { return ERRNO_INVAL };
        let Some((name, pos)) = c.data().open.get(&fd).cloned() else {
            // stdin: empty.
            write_bytes(&mut c, nread, &0u32.to_le_bytes());
            return if fd == 0 { ERRNO_SUCCESS } else { ERRNO_BADF };
        };
        let file = c.data().files.get(&name).cloned().unwrap_or_default();
        let mut at = pos.min(file.len());
        let mut total = 0usize;
        for (ptr, len) in vecs {
            let n = (len as usize).min(file.len() - at);
            if n == 0 {
                break;
            }
            write_bytes(&mut c, ptr, &file[at..at + n]);
            at += n;
            total += n;
        }
        if let Some(entry) = c.data_mut().open.get_mut(&fd) {
            entry.1 = at;
        }
        write_bytes(&mut c, nread, &(total as u32).to_le_bytes());
        ERRNO_SUCCESS
    })?;
    linker.func_wrap(W, "fd_write", |mut c: Caller<'_, Host>, fd: i32, iovs: i32, count: i32, nwritten: i32| -> i32 {
        let Some(vecs) = iovecs(&c, iovs, count) else { return ERRNO_INVAL };
        let mut data = Vec::new();
        for (ptr, len) in vecs {
            match read_bytes(&c, ptr, len) {
                Some(b) => data.extend(b),
                None => return ERRNO_INVAL,
            }
        }
        // stdout and stderr (logs) are dropped: they could mention secrets.
        if let Some((name, pos)) = c.data().open.get(&fd).cloned() {
            let host = c.data_mut();
            let file = host.files.entry(name).or_default();
            if file.len() < pos + data.len() {
                file.resize(pos + data.len(), 0);
            }
            file[pos..pos + data.len()].copy_from_slice(&data);
            if let Some(entry) = host.open.get_mut(&fd) {
                entry.1 = pos + data.len();
            }
        } else if fd > 2 {
            return ERRNO_BADF;
        }
        write_bytes(&mut c, nwritten, &(data.len() as u32).to_le_bytes());
        ERRNO_SUCCESS
    })?;
    linker.func_wrap(W, "fd_seek", |mut c: Caller<'_, Host>, fd: i32, offset: i64, whence: i32, out: i32| -> i32 {
        let Some((name, pos)) = c.data().open.get(&fd).cloned() else { return ERRNO_BADF };
        let len = c.data().files.get(&name).map_or(0, Vec::len) as i64;
        let base = match whence {
            0 => 0,
            1 => pos as i64,
            2 => len,
            _ => return ERRNO_INVAL,
        };
        let new = base + offset;
        if new < 0 {
            return ERRNO_INVAL;
        }
        if let Some(entry) = c.data_mut().open.get_mut(&fd) {
            entry.1 = new as usize;
        }
        write_bytes(&mut c, out, &(new as u64).to_le_bytes());
        ERRNO_SUCCESS
    })?;
    linker.func_wrap(W, "fd_close", |mut c: Caller<'_, Host>, fd: i32| -> i32 {
        if c.data_mut().open.remove(&fd).is_some() { ERRNO_SUCCESS } else { ERRNO_BADF }
    })?;
    linker.func_wrap(W, "fd_fdstat_get", |mut c: Caller<'_, Host>, fd: i32, buf: i32| -> i32 {
        // filetype: 2 character device (stdio), 3 directory, 4 regular file.
        let kind: u8 = match fd {
            0..=2 => 2,
            ROOT_FD => 3,
            _ if c.data().open.contains_key(&fd) => 4,
            _ => return ERRNO_BADF,
        };
        let mut out = [0u8; 24];
        out[0] = kind;
        out[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        out[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        write_bytes(&mut c, buf, &out);
        ERRNO_SUCCESS
    })?;
    linker.func_wrap(W, "fd_filestat_get", |mut c: Caller<'_, Host>, fd: i32, buf: i32| -> i32 {
        let mut out = [0u8; 64];
        match c.data().open.get(&fd).cloned() {
            Some((name, _)) => {
                out[16] = 4;
                out[24..32].copy_from_slice(&1u64.to_le_bytes());
                let size = c.data().files.get(&name).map_or(0, Vec::len) as u64;
                out[32..40].copy_from_slice(&size.to_le_bytes());
            }
            None if fd <= 2 => out[16] = 2,
            None if fd == ROOT_FD => out[16] = 3,
            None => return ERRNO_BADF,
        }
        write_bytes(&mut c, buf, &out);
        ERRNO_SUCCESS
    })?;
    linker.func_wrap(W, "proc_exit", |_: Caller<'_, Host>, code: i32| -> Result<(), wasmi::Error> { Err(wasmi::Error::i32_exit(code)) })?;
    linker.func_wrap(W, "random_get", |mut c: Caller<'_, Host>, buf: i32, len: i32| -> i32 {
        let mut bytes = vec![0u8; len.max(0) as usize];
        rand::fill(&mut bytes[..]);
        if write_bytes(&mut c, buf, &bytes) { ERRNO_SUCCESS } else { ERRNO_INVAL }
    })?;
    Ok(())
}

/// A loaded Vesta client module.
pub struct Vesta {
    store: Store<Host>,
    instance: Instance,
}

/// What a successful login gives: the message for the server and the keys
/// to open its answer.
pub struct LoginFinish {
    pub l3: Vec<u8>,
    pub session_key: Vec<u8>,
    pub export_key: Vec<u8>,
}

impl Vesta {
    pub fn load(wasm: &[u8]) -> Result<Vesta> {
        let engine = Engine::default();
        let module = Module::new(&engine, wasm).map_err(|e| anyhow!("Vesta module: {e}"))?;
        let mut store = Store::new(&engine, Host::default());
        let mut linker = Linker::new(&engine);
        link(&mut linker)?;
        let instance = linker.instantiate_and_start(&mut store, &module).map_err(|e| anyhow!("Vesta module: {e}"))?;
        // A WASI command: run its (empty) main once, like the web's reactor
        // does, then call `execute_command` per request.
        if let Ok(start) = instance.get_typed_func::<(), ()>(&store, "_start") {
            if let Err(e) = start.call(&mut store, ()) {
                if e.i32_exit_status().is_none_or(|code| code != 0) {
                    bail!("Vesta module didn't start: {e}");
                }
            }
        }
        Ok(Vesta { store, instance })
    }

    fn run(&mut self, command: CliCommand) -> Result<CliResponse> {
        let host = self.store.data_mut();
        host.files.insert("input".into(), command.encode_to_vec());
        host.files.insert("output".into(), Vec::new());
        host.open.clear();
        // Called like the web does, ignoring its return value (its type
        // isn't part of the interface).
        let exec = self.instance.get_func(&self.store, "execute_command").ok_or_else(|| anyhow!("Vesta module: no execute_command"))?;
        let ty = exec.ty(&self.store);
        let mut results: Vec<wasmi::Val> = ty.results().iter().map(|t| wasmi::Val::default(*t)).collect();
        if let Err(e) = exec.call(&mut self.store, &[], &mut results) {
            if e.i32_exit_status().is_none_or(|code| code != 0) {
                bail!("Vesta module failed: {e}");
            }
        }
        let output = self.store.data_mut().files.remove("output").unwrap_or_default();
        if output.is_empty() {
            bail!("Vesta module gave no answer");
        }
        let response = CliResponse::decode(&output[..]).map_err(|e| anyhow!("Vesta answer: {e}"))?;
        match response.error_code.unwrap_or(0) {
            0 => Ok(response),
            2 => Err(WrongPin.into()),
            code => bail!("Vesta: {} (code {code})", error_name(code)),
        }
    }

    /// Check an HSM key against Meta's signature over it.
    pub fn validate_hsm_key(&mut self, key: &[u8], signature: &[u8]) -> Result<bool> {
        let r = self.run(CliCommand {
            validate_hsm_key: Some(ValidateHsmKeyRequest { hsm_key: Some(key.to_vec()), signature: Some(signature.to_vec()) }),
            ..Default::default()
        })?;
        Ok(r.validate_hsm_key.and_then(|v| v.is_valid).unwrap_or(false))
    }

    /// Start a login with the PIN: the first message and the client state.
    pub fn begin_login(&mut self, pin: &str) -> Result<(Vec<u8>, Vec<u8>)> {
        let r = self.run(CliCommand { begin_login: Some(BeginLoginRequest { password: Some(pin.as_bytes().to_vec()) }), ..Default::default() })?;
        let b = r.begin_login.ok_or_else(|| anyhow!("Vesta: no login message"))?;
        Ok((b.l1_message.unwrap_or_default(), b.client_state.unwrap_or_default()))
    }

    /// Check the server's answer (signed by the HSM) and make the last message.
    pub fn finish_login(&mut self, l2: &[u8], l2_sig: &[u8], island_pub: &[u8], state: &[u8]) -> Result<LoginFinish> {
        let r = self.run(CliCommand {
            finish_login: Some(FinishLoginRequest {
                l2_message: Some(l2.to_vec()),
                l2_message_sig: Some(l2_sig.to_vec()),
                island_ed25519_pub: Some(island_pub.to_vec()),
                client_state: Some(state.to_vec()),
            }),
            ..Default::default()
        })?;
        let f = r.finish_login.ok_or_else(|| anyhow!("Vesta: no finish message"))?;
        Ok(LoginFinish {
            l3: f.l3_message.unwrap_or_default(),
            session_key: f.session_key.unwrap_or_default(),
            export_key: f.export_key.unwrap_or_default(),
        })
    }

    /// Open the server's login payload: the user secret (the backup's
    /// recovery code, as text).
    pub fn unpack_login_payload(&mut self, payload: &[u8], keys: &LoginFinish) -> Result<Vec<u8>> {
        let r = self.run(CliCommand {
            unpack_login_payload: Some(UnpackLoginPayloadRequest {
                login_payload: Some(payload.to_vec()),
                session_key: Some(keys.session_key.clone()),
                export_key: Some(keys.export_key.clone()),
                use_protobuf_mode: Some(true),
            }),
            ..Default::default()
        })?;
        r.unpack_login_payload.and_then(|u| u.user_secret).ok_or_else(|| anyhow!("Vesta: empty login payload"))
    }
}

// ---- The login, as the web client runs it (E2EEBackupSecretRestoreModule) ----

const PRODUCT_USE_CASE: &str = "MSGR_ENC_BACKUP";
const INIT_DOC: &str = "28587565877599806";
const BEGIN_DOC: &str = "28781838094734033";
const FINISH_DOC: &str = "28557176640587196";
/// `MWChatEncryptedBackupsSettingsDialogQuery`: the backup's virtual devices.
const SETTINGS_DOC: &str = "31918369997811363";

/// The Vesta client id: the `client_generated_id` of the backup's HSM
/// virtual device (what the web passes as `fbid` to the Vesta calls).
async fn vesta_client_id(session: &crate::session::Session) -> Result<String> {
    let v = session.graphql("MWChatEncryptedBackupsSettingsDialogQuery", SETTINGS_DOC, &serde_json::json!({})).await?;
    v.pointer("/data/xfb_backup/virtual_devices")
        .and_then(|d| d.as_array())
        .and_then(|devices| devices.iter().find(|d| d["device_type"].as_str() == Some("HSM")))
        .and_then(|d| d["client_generated_id"].as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("the backup has no PIN (HSM) device"))
}

/// The backup's PIN status, from the login init.
#[derive(Clone, Debug)]
pub struct PinStatus {
    pub registered: bool,
    pub attempts_remaining: Option<i64>,
}

/// Why a PIN login didn't produce the secret.
#[derive(Debug)]
pub enum PinError {
    /// The PIN is wrong; `attempts_remaining` as the server counts them.
    WrongPin { attempts_remaining: Option<i64> },
    /// The account has no PIN backup.
    NotRegistered,
    Other(anyhow::Error),
}

impl std::fmt::Display for PinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PinError::WrongPin { attempts_remaining: Some(n) } => write!(f, "Incorrect PIN ({n} attempts left)"),
            PinError::WrongPin { attempts_remaining: None } => f.write_str("Incorrect PIN"),
            PinError::NotRegistered => f.write_str("This account has no Messenger PIN backup"),
            PinError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl From<anyhow::Error> for PinError {
    fn from(e: anyhow::Error) -> Self {
        PinError::Other(e)
    }
}

fn b64url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn unb64url(s: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    let s = s.trim_end_matches('=');
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).map_err(|e| anyhow!("bad base64 from the server: {e}"))
}

/// Download (once per run) the Vesta module the page points at.
async fn fetch_module(session: &crate::session::Session) -> Result<Vec<u8>> {
    static CACHE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    if let Some(bytes) = CACHE.get() {
        return Ok(bytes.clone());
    }
    let url = session.boot.vesta_wasm_url.as_deref().ok_or_else(|| anyhow!("the page has no Vesta module link"))?;
    let bytes = session.http.get(url).send().await?.error_for_status()?.bytes().await?.to_vec();
    if !bytes.starts_with(b"\0asm") {
        bail!("the Vesta module link didn't give a WebAssembly module");
    }
    Ok(CACHE.get_or_init(|| bytes).clone())
}

/// Whether the account has a PIN backup, and how many tries are left.
pub async fn pin_status(session: &crate::session::Session) -> Result<PinStatus> {
    let vars = serde_json::json!({"request": {"fbid": vesta_client_id(session).await?, "product_use_case": PRODUCT_USE_CASE, "vesta_client_api_version": 6}});
    let v = session.graphql("VestaServerGraphQLLoginProviderInitQuery", INIT_DOC, &vars).await?;
    let init = v.pointer("/data/fbid_based_auth_layer_vesta_login_init").ok_or_else(|| anyhow!("no Vesta login info"))?;
    Ok(PinStatus { registered: init["is_registered"].as_bool().unwrap_or(false), attempts_remaining: init["attempts_remaining"].as_i64() })
}

/// Read-only check of everything before the PIN: the backup's status, the
/// module download, and Meta's signatures on the HSM keys (checked locally).
pub async fn check_backup(session: &crate::session::Session) -> Result<(PinStatus, bool)> {
    let vars = serde_json::json!({"request": {"fbid": vesta_client_id(session).await?, "product_use_case": PRODUCT_USE_CASE, "vesta_client_api_version": 6}});
    let v = session.graphql("VestaServerGraphQLLoginProviderInitQuery", INIT_DOC, &vars).await?;
    let init = v.pointer("/data/fbid_based_auth_layer_vesta_login_init").cloned().ok_or_else(|| anyhow!("no Vesta login info"))?;
    let status = PinStatus { registered: init["is_registered"].as_bool().unwrap_or(false), attempts_remaining: init["attempts_remaining"].as_i64() };
    let field = |k: &str| init[k].as_str().ok_or_else(|| anyhow!("Vesta login info lacks {k}")).and_then(unb64url);
    let mut vesta = Vesta::load(&fetch_module(session).await?)?;
    let keys_ok = vesta.validate_hsm_key(&field("base64url_encoded_island_rsa_pub")?, &field("base64url_encoded_island_rsa_pub_signature")?)?
        && vesta.validate_hsm_key(&field("base64url_encoded_island_ed25519_pub")?, &field("base64url_encoded_island_ed25519_pub_signature")?)?;
    Ok((status, keys_ok))
}

/// The PIN login: returns the user secret (the backup's recovery code).
/// Every check the web makes happens before the PIN leaves the module, and
/// nothing is retried: a failure stops here.
pub async fn secret_from_pin(session: &crate::session::Session, pin: &str) -> Result<Vec<u8>, PinError> {
    let fbid = vesta_client_id(session).await?;
    // 1. Init: the HSM keys and whether there's a backup.
    let vars = serde_json::json!({"request": {"fbid": fbid, "product_use_case": PRODUCT_USE_CASE, "vesta_client_api_version": 6}});
    let v = session.graphql("VestaServerGraphQLLoginProviderInitQuery", INIT_DOC, &vars).await?;
    let init = v.pointer("/data/fbid_based_auth_layer_vesta_login_init").cloned().ok_or_else(|| anyhow!("no Vesta login info"))?;
    let field = |k: &str| init[k].as_str().ok_or_else(|| anyhow!("Vesta login info lacks {k}")).and_then(unb64url);
    if !init["is_registered"].as_bool().unwrap_or(false) {
        return Err(PinError::NotRegistered);
    }
    let attempts = init["attempts_remaining"].as_i64();
    let (rsa_pub, rsa_sig) = (field("base64url_encoded_island_rsa_pub")?, field("base64url_encoded_island_rsa_pub_signature")?);
    let (ed_pub, ed_sig) = (field("base64url_encoded_island_ed25519_pub")?, field("base64url_encoded_island_ed25519_pub_signature")?);

    // 2. The module checks Meta's signatures on the HSM keys.
    let mut vesta = Vesta::load(&fetch_module(session).await?)?;
    if !vesta.validate_hsm_key(&rsa_pub, &rsa_sig)? || !vesta.validate_hsm_key(&ed_pub, &ed_sig)? {
        return Err(anyhow!("the backup service's keys didn't verify; stopped before sending the PIN").into());
    }

    // 3–4. Begin: l1 out, the HSM's signed l2 back.
    let (l1, state) = vesta.begin_login(pin)?;
    let input = serde_json::json!({"input": {
        "fbid": fbid, "product_use_case": PRODUCT_USE_CASE, "base64url_encoded_opaque_l1": b64url(&l1),
        "vesta_client_api_version": 6, "vesta_client_protocol_version": 1,
    }});
    let v = session.graphql("VestaServerGraphQLLoginProviderBeginLoginMutation", BEGIN_DOC, &input).await?;
    let begin = v.pointer("/data/fbid_based_auth_layer_vesta_begin_login").cloned().ok_or_else(|| anyhow!("no answer to the PIN login"))?;
    let l2 = begin["base64url_encoded_opaque_l2"].as_str().ok_or_else(|| anyhow!("PIN login: no l2")).and_then(unb64url)?;
    let l2_sig = begin["base64url_encoded_opaque_l2_sig"].as_str().ok_or_else(|| anyhow!("PIN login: no l2 signature")).and_then(unb64url)?;

    // 5. The module verifies the HSM's signature and makes l3. A wrong PIN
    //    shows up here or in the server's finish answer.
    let keys = match vesta.finish_login(&l2, &l2_sig, &ed_pub, &state) {
        Ok(k) => k,
        Err(e) if e.is::<WrongPin>() => return Err(PinError::WrongPin { attempts_remaining: attempts.map(|n| n - 1) }),
        Err(e) => return Err(e.into()),
    };

    // 6. Finish: the login payload if the PIN was right.
    let input = serde_json::json!({"input": {
        "fbid": fbid, "product_use_case": PRODUCT_USE_CASE, "base64url_encoded_opaque_l3": b64url(&keys.l3),
        "vesta_client_api_version": 6, "vesta_client_protocol_version": 1,
    }});
    let v = session.graphql("VestaServerGraphQLLoginProviderFinishLoginMutation", FINISH_DOC, &input).await?;
    let finish = v.pointer("/data/fbid_based_auth_layer_vesta_finish_login_v2").cloned().ok_or_else(|| anyhow!("no answer to the PIN check"))?;
    if finish["__typename"].as_str() == Some("XFBVestaError") {
        return Err(anyhow!("backup service error {}", finish["vesta_error_code"]).into());
    }
    let remaining = finish["attempts_remaining"].as_i64().or(attempts);
    if !finish["login_success"].as_bool().unwrap_or(false) {
        return Err(PinError::WrongPin { attempts_remaining: remaining });
    }
    let payload = finish["base64url_encoded_login_payload"].as_str().ok_or_else(|| anyhow!("PIN accepted but no payload")).and_then(unb64url)?;

    // 7. Open the payload.
    Ok(vesta.unpack_login_payload(&payload, &keys)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs only with `VESTA_WASM=/path/to/vesta.wasm` (the module isn't in
    /// the repo): a login start with a test password, entirely offline.
    #[test]
    fn begin_login_offline() {
        let Some(path) = std::env::var_os("VESTA_WASM") else { return };
        let mut vesta = Vesta::load(&std::fs::read(path).unwrap()).unwrap();
        let (l1, state) = vesta.begin_login("test-password-123").unwrap();
        assert_eq!(l1.len(), 98, "opaque-ke CredentialRequest");
        assert!(!state.is_empty());
        // Twice in a row on the same instance works too.
        let (l1b, _) = vesta.begin_login("test-password-123").unwrap();
        assert_ne!(l1, l1b, "blinded with fresh randomness");
    }
}
