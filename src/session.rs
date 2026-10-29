//! Facebook web session: cookies (kept in the macOS Keychain), page bootstrap
//! (tokens scraped from /messages/) and the Lightspeed GraphQL request.

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use serde_json::{json, Value};

use crate::store::now_ms;

const KEYCHAIN_SERVICE: &str = "messenger-rs";
const KEYCHAIN_ACCOUNT: &str = "facebook-cookies";
pub const BASE: &str = "https://www.facebook.com";
pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

/// Raw `Cookie:` header value, e.g. `c_user=..; xs=..; datr=..`.
#[derive(Clone)]
pub struct Cookies(pub String);

impl Cookies {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.split(';').map(str::trim).find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (k == name).then_some(v)
        })
    }
    pub fn is_logged_in(&self) -> bool {
        self.get("c_user").is_some() && self.get("xs").is_some()
    }

    pub fn load() -> Option<Cookies> {
        let bytes = security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT).ok()?;
        let c = Cookies(String::from_utf8(bytes).ok()?);
        c.is_logged_in().then_some(c)
    }
    pub fn save(&self) -> Result<()> {
        let _ = security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT);
        security_framework::passwords::set_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, self.0.as_bytes())
            .context("saving cookies to Keychain")
    }
    pub fn clear() {
        let _ = security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT);
    }
}

/// Values scraped from the messenger page that every later request needs.
#[derive(Debug, Clone)]
pub struct Bootstrap {
    pub fbid: i64,
    pub dtsg: String,
    pub lsd: String,
    pub ls_app_id: String,
    pub mqtt_app_id: i64,
    pub mqtt_endpoint: String,
    pub ls_doc_id: String,
    pub ls_version: i64,
    /// Page build/session fields the web client sends with every request.
    pub page_params: Vec<(&'static str, String)>,
    /// E2EE crypto auth token (`MessengerWebInitData.cryptoAuthToken`) and the
    /// app id it belongs to; empty when the page doesn't carry them.
    pub e2ee_cat: String,
    pub e2ee_app_id: String,
}

pub struct Session {
    pub http: reqwest::Client,
    pub cookies: Cookies,
    pub boot: Bootstrap,
    req_counter: std::sync::atomic::AtomicU64,
    /// Lightspeed client device id, the same for GraphQL requests and the MQTT
    /// connection. Sync cursors and task results belong to a device: with a
    /// new id per request the server answers every sync with a fresh snapshot
    /// and delivers fetched history to a "device" that never asks for it.
    pub device_id: String,
}

fn capture(html: &str, re: &str) -> Option<String> {
    Regex::new(re).ok()?.captures(html)?.get(1).map(|m| m.as_str().to_string())
}

impl Session {
    pub async fn connect(cookies: Cookies) -> Result<Session> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()?;
        let html = http
            .get(format!("{BASE}/messages/"))
            .header("Cookie", &cookies.0)
            .header("Accept", "text/html,application/xhtml+xml")
            .header("Sec-Fetch-Mode", "navigate")
            .header("Sec-Fetch-Site", "none")
            .header("Sec-Fetch-Dest", "document")
            .send()
            .await?
            .text()
            .await?;

        let dtsg = capture(&html, r#""DTSGInitialData",\[\],\{"token":"([^"]+)""#)
            .ok_or_else(|| anyhow!("session expired — please log in again"))?;
        let lsd = capture(&html, r#""LSD",\[\],\{"token":"([^"]+)""#).unwrap_or_default();
        let fbid: i64 = capture(&html, r#""MqttWebConfig",\[\],\{"fbid":"(\d+)""#)
            .or_else(|| cookies.get("c_user").map(str::to_string))
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow!("could not determine user id"))?;
        let mqtt_app_id = capture(&html, r#""MqttWebConfig",\[\],\{[^}]*"appID":(\d+)"#)
            .and_then(|s| s.parse().ok())
            .unwrap_or(219994525426954);
        let mqtt_endpoint = capture(&html, r#""MqttWebConfig",\[\],\{[^}]*"endpoint":"([^"]+)""#)
            .map(|s| s.replace("\\/", "/"))
            .unwrap_or_else(|| "wss://edge-chat.facebook.com/chat?region=prn".into());
        let ls_app_id = capture(&html, r#""appId":(\d+)"#).unwrap_or_else(|| "2220391788200892".into());
        let ls_doc_id = capture(&html, r#"LSPlatformGraphQLLightspeedRequestQueryRelayPreloader[^"]*","queryID":"(\d+)""#)
            .unwrap_or_else(|| "9697184873702141".into());
        let ls_version: i64 = capture(&html, r#"LSPlatformGraphQLLightspeedRequestQuery[^}]*?\\"version\\":(\d{8,})"#)
            .or_else(|| capture(&html, r#"\\"version\\":(\d{8,})"#))
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow!("could not find Lightspeed schema version in page"))?;

        let mut page_params = vec![("__comet_req", "15".to_string())];
        for (key, re) in [
            ("__rev", r#""client_revision":(\d+)"#),
            ("__spin_r", r#""__spin_r":(\d+)"#),
            ("__spin_b", r#""__spin_b":"([^"]+)""#),
            ("__spin_t", r#""__spin_t":(\d+)"#),
            ("__hs", r#""haste_session":"([^"]+)""#),
            ("__hsi", r#""hsi":"(\d+)""#),
        ] {
            if let Some(v) = capture(&html, re) {
                page_params.push((key, v));
            }
        }

        // MessengerWebInitData holds the E2EE crypto auth token (CAT).
        let (mut e2ee_cat, mut e2ee_app_id) = (String::new(), String::new());
        if let Some(i) = html.find("\"MessengerWebInitData\"") {
            let seg = &html[i..(i + 8000).min(html.len())];
            e2ee_app_id = capture(seg, r#""appId":(\d+)"#).unwrap_or_default();
            if let Some(raw) = capture(seg, r#""encrypted_serialized_cat":"((?:[^"\\]|\\.)*)""#) {
                e2ee_cat = serde_json::from_str(&format!("\"{raw}\"")).unwrap_or(raw);
            }
        }

        Ok(Session {
            http,
            cookies,
            boot: Bootstrap { fbid, dtsg, lsd, ls_app_id, mqtt_app_id, mqtt_endpoint, ls_doc_id, ls_version, page_params, e2ee_cat, e2ee_app_id },
            req_counter: Default::default(),
            device_id: uuid::Uuid::new_v4().to_string(),
        })
    }

    fn jazoest(&self) -> String {
        let sum: u32 = self.boot.dtsg.bytes().map(u32::from).sum();
        format!("2{sum}")
    }

    fn base_form(&self) -> Vec<(&'static str, String)> {
        let n = self.req_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        vec![
            ("av", self.boot.fbid.to_string()),
            ("__user", self.boot.fbid.to_string()),
            ("__a", "1".into()),
            ("__req", format!("{n:x}")),
            ("dpr", "2".into()),
            ("fb_dtsg", self.boot.dtsg.clone()),
            ("jazoest", self.jazoest()),
            ("lsd", self.boot.lsd.clone()),
        ]
        .into_iter()
        .chain(self.boot.page_params.iter().cloned())
        .collect()
    }

    /// Run the Lightspeed sync query for `database` and return the raw LS payload.
    /// From a cursor this is a delta sync (type 2, no sync params, like the web
    /// client); without one, a snapshot (type 1).
    pub async fn ls_sync(&self, database: i64, cursor: Option<&str>) -> Result<String> {
        let (kind, sync_params) = match cursor {
            Some(_) => (2, Value::Null),
            None => (1, Value::String(ls_sync_params())),
        };
        let request_payload = json!({
            "database": database,
            "epoch_id": 0,
            "failure_count": null,
            "last_applied_cursor": cursor,
            "sync_params": sync_params,
            "version": self.boot.ls_version,
        })
        .to_string();
        self.ls_request(kind, request_payload).await
    }

    /// Run Lightspeed tasks (`(label, payload, queue)`) over GraphQL and return
    /// the raw LS payload of the response (e.g. fetched message history).
    pub async fn ls_tasks(&self, tasks: &[(&str, Value, String)]) -> Result<String> {
        let base_id = crate::store::now_ms() % 1_000_000;
        let tasks: Vec<Value> = tasks
            .iter()
            .enumerate()
            .map(|(i, (label, payload, queue))| {
                json!({
                    "failure_count": null, "label": label, "payload": payload.to_string(),
                    "queue_name": queue, "task_id": base_id + i as i64,
                })
            })
            .collect();
        let request_payload = json!({
            "epoch_id": crate::store::now_ms() << 22,
            "tasks": tasks,
            "version_id": self.boot.ls_version.to_string(),
        })
        .to_string();
        self.ls_request(3, request_payload).await
    }

    /// `LSPlatformGraphQLLightspeedRequestQuery`: type 1 = database sync, 3 = tasks.
    async fn ls_request(&self, request_type: i64, request_payload: String) -> Result<String> {
        let variables = json!({
            "deviceId": self.device_id,
            "requestId": 0,
            "requestPayload": request_payload,
            "requestType": request_type,
        })
        .to_string();
        let mut form = self.base_form();
        form.extend([
            ("fb_api_caller_class", "RelayModern".to_string()),
            ("fb_api_req_friendly_name", "LSPlatformGraphQLLightspeedRequestQuery".into()),
            ("server_timestamps", "true".into()),
            ("doc_id", self.boot.ls_doc_id.clone()),
            ("variables", variables),
        ]);
        let text = self
            .http
            .post(format!("{BASE}/api/graphql/"))
            .header("Cookie", &self.cookies.0)
            .header("Origin", BASE)
            .header("Referer", format!("{BASE}/messages/"))
            .header("X-FB-LSD", &self.boot.lsd)
            .header("X-FB-Friendly-Name", "LSPlatformGraphQLLightspeedRequestQuery")
            .header("Accept", "*/*")
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Sec-Fetch-Dest", "empty")
            .header("Sec-Fetch-Mode", "cors")
            .header("Sec-Fetch-Site", "same-origin")
            .form(&form)
            .send()
            .await?
            .text()
            .await?;
        let first = text.trim_start_matches("for (;;);").lines().next().unwrap_or("");
        let v: Value = serde_json::from_str(first)
            .with_context(|| format!("parsing GraphQL response: {}", text.chars().take(200).collect::<String>()))?;
        if let Some(err) = v.get("errors") {
            bail!("GraphQL error: {err}");
        }
        let viewer = v
            .pointer("/data/viewer")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("unexpected GraphQL response: {}", first.chars().take(300).collect::<String>()))?;
        // The field name has changed between releases (lightspeed_web_request,
        // lightspeed_web_request_for_us, ...) — take whichever holds a payload.
        viewer
            .values()
            .find_map(|f| f.get("payload").and_then(Value::as_str))
            .map(str::to_string)
            .ok_or_else(|| anyhow!("no Lightspeed payload in response"))
    }

    /// Upload a file for sending as an attachment. Returns the attachment fbid.
    pub async fn upload(&self, path: &std::path::Path) -> Result<i64> {
        let bytes = tokio::fs::read(path).await?;
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
        let mime = mime_for(&name);
        let part = reqwest::multipart::Part::bytes(bytes).file_name(name).mime_str(mime)?;
        let form = reqwest::multipart::Form::new().part("farr", part);
        let query: Vec<(&str, String)> = self.base_form();
        let text = self
            .http
            .post(format!("{BASE}/ajax/mercury/upload.php"))
            .query(&query)
            .header("Cookie", &self.cookies.0)
            .header("Origin", BASE)
            .header("Referer", format!("{BASE}/messages/"))
            // Same as the web client; without them Facebook answers error
            // 1357054 ("couldn't process your request"), as GraphQL did.
            .header("X-FB-LSD", &self.boot.lsd)
            .header("Accept", "*/*")
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Sec-Fetch-Dest", "empty")
            .header("Sec-Fetch-Mode", "cors")
            .header("Sec-Fetch-Site", "same-origin")
            .multipart(form)
            .send()
            .await?
            .text()
            .await?;
        let v: Value = serde_json::from_str(text.trim_start_matches("for (;;);")).context("parsing upload response")?;
        if let Some(code) = v.get("error").and_then(Value::as_i64) {
            let summary = v["errorSummary"].as_str().unwrap_or("Upload failed");
            let detail = v["errorDescription"].as_str().unwrap_or("");
            bail!("{summary}. {detail} (error {code})");
        }
        let meta = v.pointer("/payload/metadata/0").ok_or_else(|| anyhow!("the upload answer had no file id"))?;
        ["image_id", "video_id", "audio_id", "gif_id", "file_id", "fbid"]
            .iter()
            .find_map(|k| meta.get(*k).and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok())))
            .ok_or_else(|| anyhow!("upload response had no id"))
    }
}

fn mime_for(name: &str) -> &'static str {
    match name.rsplit('.').next().map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("heic") => "image/heic",
        Some("mp4") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("mp3") => "audio/mpeg",
        Some("m4a") => "audio/mp4",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Offline threading id: millisecond timestamp shifted left 22 bits + random low bits,
/// matching what the web client generates.
pub fn new_otid() -> i64 {
    let rand = (uuid::Uuid::new_v4().as_u128() & 0x3f_ffff) as i64;
    (now_ms() << 22) | rand
}

/// `sync_params` for Lightspeed database syncs (GraphQL and MQTT alike). The
/// server ignores an MQTT sync request whose `sync_params` is null.
pub fn ls_sync_params() -> String {
    json!({
        "locale": "en_US",
        "snapshot_num_threads_per_page": 50,
        "full_height": 200, "preview_height": 360, "preview_width": 480,
        "preview_height_large": 360, "preview_width_large": 480, "scale": 2,
    })
    .to_string()
}
