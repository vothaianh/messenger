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
    /// The mailbox's region (`MessengerWebRegion.regionNullable`, e.g. "ATN"),
    /// sent to DGW as `x-dgw-regionhint`; without it the gateway answers syncs
    /// but never pushes live changes.
    pub region: Option<String>,
    /// Where the page loads the Vesta (backup PIN) client module from: bx
    /// asset 72238, `"72238":{"uri":"…wasm…"}`.
    pub vesta_wasm_url: Option<String>,
    /// Who's online and when contacts were last active, as the page had it.
    pub presence: Presence,
}

/// The page's `AvailableListInitialData`: `activeList` (ids online now) and
/// `lastActiveTimes` (id -> unix seconds). The web keeps it live over its
/// realtime presence stream; we refresh it by reloading the page now and then.
#[derive(Debug, Clone, Default)]
pub struct Presence {
    pub active: Vec<i64>,
    pub last_active: std::collections::HashMap<i64, i64>,
}

pub fn parse_presence(html: &str) -> Presence {
    let Some(i) = html.find("\"AvailableListInitialData\",[],") else { return Presence::default() };
    let rest = &html[i + "\"AvailableListInitialData\",[],".len()..];
    let Some(Ok(v)) = serde_json::Deserializer::from_str(rest).into_iter::<serde_json::Value>().next() else {
        return Presence::default();
    };
    let id = |x: &serde_json::Value| x.as_str().and_then(|s| s.parse().ok()).or_else(|| x.as_i64());
    Presence {
        active: v["activeList"].as_array().map(|a| a.iter().filter_map(id).collect()).unwrap_or_default(),
        last_active: v["lastActiveTimes"]
            .as_object()
            .map(|o| o.iter().filter_map(|(k, t)| Some((k.parse().ok()?, t.as_i64()?))).collect())
            .unwrap_or_default(),
    }
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
    /// Reload the messages page for fresh presence (who's active, last active).
    pub async fn fetch_presence(&self) -> Result<Presence> {
        let html = self
            .http
            .get(format!("{BASE}/messages/"))
            .header("Cookie", &self.cookies.0)
            .header("Accept", "text/html,application/xhtml+xml")
            .header("Sec-Fetch-Mode", "navigate")
            .header("Sec-Fetch-Site", "none")
            .header("Sec-Fetch-Dest", "document")
            .send()
            .await?
            .text()
            .await?;
        Ok(parse_presence(&html))
    }

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

        let region = capture(&html, r#""MessengerWebRegion",\[\],\{"regionNullable":"([A-Z0-9]+)"#);
        let presence = parse_presence(&html);
        let vesta_wasm_url = capture(&html, r#""72238":\{"uri":"([^"]+\.wasm[^"]*)""#).map(|u| u.replace("\\/", "/"));

        Ok(Session {
            http,
            cookies,
            boot: Bootstrap { fbid, dtsg, lsd, ls_app_id, mqtt_app_id, mqtt_endpoint, ls_doc_id, ls_version, page_params, e2ee_cat, e2ee_app_id, region, vesta_wasm_url, presence },
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
    pub async fn ls_request(&self, request_type: i64, request_payload: String) -> Result<String> {
        let variables = json!({
            "deviceId": self.device_id,
            "requestId": 0,
            "requestPayload": request_payload,
            "requestType": request_type,
        });
        let v = self.graphql("LSPlatformGraphQLLightspeedRequestQuery", &self.boot.ls_doc_id, &variables).await?;
        let first = v.to_string();
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

    /// Run a GraphQL query by its persisted `doc_id` the way the web client
    /// does (same form fields and headers); returns the first JSON object.
    pub async fn graphql(&self, friendly_name: &str, doc_id: &str, variables: &Value) -> Result<Value> {
        let mut form = self.base_form();
        form.extend([
            ("fb_api_caller_class", "RelayModern".to_string()),
            ("fb_api_req_friendly_name", friendly_name.to_string()),
            ("server_timestamps", "true".into()),
            ("doc_id", doc_id.to_string()),
            ("variables", variables.to_string()),
        ]);
        let text = self
            .http
            .post(format!("{BASE}/api/graphql/"))
            .header("Cookie", &self.cookies.0)
            .header("Origin", BASE)
            .header("Referer", format!("{BASE}/messages/"))
            .header("X-FB-LSD", &self.boot.lsd)
            .header("X-FB-Friendly-Name", friendly_name)
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
        Ok(v)
    }

    /// The sticker packs in the user's sticker tray (the web picker's tabs).
    pub async fn sticker_packs(&self) -> Result<Vec<StickerPack>> {
        let vars = json!({"feedbackID": null, "scale": 3, "stickerInterface": "MESSAGES"});
        let v = self.graphql("CometStickerPickerCardQuery", STICKER_TRAY_DOC, &vars).await?;
        let edges = v.pointer("/data/picker_plugins/sticker_picker/sticker_store/tray_packs/edges").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(edges
            .iter()
            .filter_map(|e| {
                let n = &e["node"];
                Some(StickerPack {
                    id: n["id"].as_str()?.to_string(),
                    name: n["name"].as_str().unwrap_or_default().to_string(),
                    thumbnail: n.pointer("/thumbnail_image/uri").or(n.get("thumbnail_image")).and_then(Value::as_str).unwrap_or_default().to_string(),
                })
            })
            .collect())
    }

    /// The sticker store, like the web's store dialog: the first page
    /// (`CometStickersStoreDialogQuery`), or the page after `cursor` of the
    /// store `store_id` (`CometStickersStorePackListPaginationQuery`).
    pub async fn sticker_store(&self, page: Option<(&str, &str)>) -> Result<StorePage> {
        let (store, list) = match page {
            None => {
                let v = self.graphql("CometStickersStoreDialogQuery", STICKER_STORE_DOC, &json!({"scale": 3})).await?;
                let store = v.pointer("/data/viewer/sticker_store").cloned().unwrap_or_default();
                (store["id"].as_str().unwrap_or_default().to_string(), store["available_packs"].clone())
            }
            Some((store_id, cursor)) => {
                let vars = json!({"count": 20, "cursor": cursor, "id": store_id, "scale": 3});
                let v = self.graphql("CometStickersStorePackListPaginationQuery", STICKER_STORE_PAGE_DOC, &vars).await?;
                (store_id.to_string(), v.pointer("/data/fetch__StickerStore/available_packs").cloned().unwrap_or_default())
            }
        };
        let uri = |n: &Value, key: &str| n.pointer(&format!("/{key}/uri")).and_then(Value::as_str).unwrap_or_default().to_string();
        let packs = list["edges"]
            .as_array()
            .map(|edges| {
                edges
                    .iter()
                    .filter_map(|e| {
                        let n = &e["node"];
                        Some(StorePack {
                            id: n["id"].as_str()?.to_string(),
                            name: n["name"].as_str().unwrap_or_default().to_string(),
                            artist: n["artist"].as_str().unwrap_or_default().to_string(),
                            preview: uri(n, "preview_image"),
                            thumbnail: uri(n, "thumbnail_image"),
                            in_tray: n["in_sticker_tray"].as_bool().unwrap_or(false),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let more = list.pointer("/page_info/has_next_page").and_then(Value::as_bool).unwrap_or(false);
        let next = list.pointer("/page_info/end_cursor").and_then(Value::as_str).filter(|_| more).map(str::to_string);
        Ok(StorePage { store_id: store, packs, next })
    }

    /// Add a store pack to the sticker tray, or remove it
    /// (`CometStickersStorePackMutation{Add,Remove}Mutation`).
    pub async fn sticker_pack_tray(&self, pack_id: &str, add: bool) -> Result<()> {
        let n = self.req_counter.load(std::sync::atomic::Ordering::Relaxed);
        let vars = json!({
            "input": {
                "feedback_target_id": null, "pack_id": pack_id,
                "actor_id": self.boot.fbid.to_string(), "client_mutation_id": n.to_string(),
            },
            "scale": 3,
            "stickerInterface": "MESSAGES",
        });
        let (name, doc) = if add {
            ("CometStickersStorePackMutationAddMutation", STICKER_PACK_ADD_DOC)
        } else {
            ("CometStickersStorePackMutationRemoveMutation", STICKER_PACK_REMOVE_DOC)
        };
        self.graphql(name, doc, &vars).await?;
        Ok(())
    }

    /// The stickers of one pack (first page, like the web picker).
    pub async fn sticker_pack(&self, pack_id: &str) -> Result<Vec<Sticker>> {
        let vars = json!({"packID": pack_id, "stickerWidth": 128, "stickerHeight": 128, "scale": 3});
        let v = self.graphql("CometStickerPickerPackContentRootQuery", STICKER_PACK_DOC, &vars).await?;
        let edges = v.pointer("/data/sticker_pack/stickers/edges").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(edges
            .iter()
            .filter_map(|e| {
                let n = &e["node"];
                Some(Sticker {
                    id: n["id"].as_str()?.parse().ok()?,
                    image: n.pointer("/image/uri").and_then(Value::as_str)?.to_string(),
                    label: n["label"].as_str().or(n["accessibility_label"].as_str()).unwrap_or_default().to_string(),
                })
            })
            .collect())
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

/// Persisted query ids of the web sticker picker (see README "Protocol notes").
const STICKER_TRAY_DOC: &str = "28475074195489368";
const STICKER_STORE_DOC: &str = "28719354961066192";
const STICKER_STORE_PAGE_DOC: &str = "28347266178275168";
const STICKER_PACK_ADD_DOC: &str = "28628969456733944";
const STICKER_PACK_REMOVE_DOC: &str = "28423263890675279";

/// A pack in the sticker store.
#[derive(Clone, Debug)]
pub struct StorePack {
    pub id: String,
    pub name: String,
    pub artist: String,
    /// A banner with a few of the pack's stickers.
    pub preview: String,
    pub thumbnail: String,
    pub in_tray: bool,
}

/// One page of the sticker store, and where the next one starts.
pub struct StorePage {
    pub store_id: String,
    pub packs: Vec<StorePack>,
    pub next: Option<String>,
}
const STICKER_PACK_DOC: &str = "25236323532672635";

#[derive(Clone, Debug)]
pub struct StickerPack {
    pub id: String,
    pub name: String,
    pub thumbnail: String,
}

#[derive(Clone, Debug)]
pub struct Sticker {
    pub id: i64,
    pub image: String,
    pub label: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_presence() {
        let html = r#"x,["AvailableListInitialData",[],{"activeList":["42","7"],"lastActiveTimes":{"42":1700000000,"9":1690000000},"chatNotif":0,"playingNow":[]},123],["Next""#;
        let p = parse_presence(html);
        assert_eq!(p.active, vec![42, 7]);
        assert_eq!(p.last_active.get(&9), Some(&1690000000));
        assert!(parse_presence("nothing").active.is_empty());
    }
}
