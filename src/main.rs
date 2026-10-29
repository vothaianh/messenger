mod armadillo;
mod client;
mod dgw;
mod e2ee;
mod e2ee_socket;
mod e2ee_store;
mod e2ee_listen;
mod e2ee_media;
mod lightspeed;
mod login;
mod macos;
mod mqtt;
mod session;
mod store;
mod ui;

use session::Cookies;

fn main() -> anyhow::Result<()> {
    // reqwest and tokio-tungstenite pull in both rustls backends; pick one explicitly.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // The login window uses its own (tao) event loop, so it runs as a child process.
    if std::env::args().nth(1).as_deref() == Some("login") {
        return login::run();
    }
    // Fallback when the embedded login can't finish (e.g. passkey prompts, which
    // WebKit only allows for apps entitled to facebook.com): paste the `Cookie`
    // header from a logged-in browser on stdin.
    if std::env::args().nth(1).as_deref() == Some("import-cookies") {
        let mut header = String::new();
        std::io::stdin().read_line(&mut header)?;
        let header = header.trim().trim_start_matches("cookie:").trim_start_matches("Cookie:").trim();
        let cookies = Cookies(header.to_string());
        anyhow::ensure!(cookies.is_logged_in(), "the cookie must contain c_user and xs");
        cookies.save()?;
        println!("Saved. Run the app again.");
        return Ok(());
    }

    // Diagnostics: bootstrap + one Lightspeed sync, printing a summary.
    if std::env::args().nth(1).as_deref() == Some("debug-sync") {
        let cookies = Cookies::load().ok_or_else(|| anyhow::anyhow!("not logged in"))?;
        return tokio::runtime::Runtime::new()?.block_on(async {
            let s = session::Session::connect(cookies).await?;
            let b = &s.boot;
            println!("fbid ok={} dtsg_len={} lsd_len={} doc_id={} version={} ls_app={} mqtt={} page_params={:?}", b.fbid > 0, b.dtsg.len(), b.lsd.len(), b.ls_doc_id, b.ls_version, b.ls_app_id, b.mqtt_endpoint, b.page_params.iter().map(|(k, _)| *k).collect::<Vec<_>>());
            // Databases to sync: `debug-sync 1 95` (default: 1).
            let dbs: Vec<i64> = std::env::args().skip(2).filter_map(|a| a.parse().ok()).collect();
            let mut all = store::Store::default();
            all.me = s.boot.fbid;
            for db in if dbs.is_empty() { vec![1] } else { dbs } {
                match s.ls_sync(db, None).await.and_then(|p| lightspeed::decode_payload(&p)) {
                    Ok(calls) => {
                        let mut store = store::Store::default();
                        store.apply(&calls);
                        all.apply(&calls);
                        let mut procs: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
                        procs.sort();
                        procs.dedup();
                        let types: std::collections::BTreeSet<i64> = store.threads.values().map(|t| t.thread_type).collect();
                        println!("db={db} calls={} threads={} thread_types={types:?} contacts={} messages={}", calls.len(), store.threads.len(), store.contacts.len(), store.messages.values().map(Vec::len).sum::<usize>());
                        println!("  procs={procs:?}");
                        // MESSENGER_DEBUG_PROC=name prints the argument shapes of that
                        // procedure (strings redacted to their length) for mapping.
                        if let Ok(name) = std::env::var("MESSENGER_DEBUG_PROC") {
                            for c in calls.iter().filter(|c| c.name == name).take(3) {
                                let shape: Vec<String> = c.args.iter().enumerate().filter_map(|(i, a)| {
                                    let v = match a {
                                        lightspeed::Arg::Null | lightspeed::Arg::Expr => return None,
                                        lightspeed::Arg::Bool(b) => b.to_string(),
                                        lightspeed::Arg::Int(n) if n.abs() < 1_000 => format!("i:{n}"),
                                        lightspeed::Arg::Int(n) if (1_500_000_000_000..2_000_000_000_000).contains(n) => "TSms".into(),
                                        lightspeed::Arg::Int(n) => format!("ID{}", n.to_string().len()),
                                        lightspeed::Arg::Float(_) => "f".into(),
                                        lightspeed::Arg::Str(s) if s.starts_with("http") => "URL".into(),
                                        lightspeed::Arg::Str(s) => format!("str{}", s.chars().count()),
                                    };
                                    Some(format!("{i}:{v}"))
                                }).collect();
                                println!("  {name}: {}", shape.join(" "));
                            }
                        }
                    }
                    Err(e) => println!("db={db} error: {}", e.to_string().chars().take(200).collect::<String>()),
                }
            }
            // Threads whose title couldn't be resolved fall back to the numeric key.
            let untitled = all.threads.values().filter(|t| all.thread_title(t).chars().all(|c| c.is_ascii_digit())).count();
            let e2ee = all.threads.values().filter(|t| t.e2ee).count();
            println!("combined: threads={} e2ee={e2ee} untitled={untitled}", all.threads.len());
            Ok(())
        });
    }

    // E2EE device registration (phase 2, docs/E2EE.md).
    //   e2ee-register --check   read-only: show the account's E2EE devices
    //   e2ee-register           register this app as a new E2EE device
    if std::env::args().nth(1).as_deref() == Some("e2ee-register") {
        let check_only = std::env::args().any(|a| a == "--check");
        let cookies = Cookies::load().ok_or_else(|| anyhow::anyhow!("not logged in"))?;
        return tokio::runtime::Runtime::new()?.block_on(async {
            let s = session::Session::connect(cookies).await?;
            println!("CAT present: {} (len {}), app_id present: {}", !s.boot.e2ee_cat.is_empty(), s.boot.e2ee_cat.len(), !s.boot.e2ee_app_id.is_empty());
            let mut device = match e2ee::Device::load() {
                Some(d) => d,
                None => e2ee::Device::generate(),
            };
            if let Some(jid) = device.jid() {
                println!("Already registered as {jid}");
                return Ok(());
            }
            let current = e2ee::fetch_icdc(&s, &device).await?;
            println!("Account has {} E2EE device(s), icdc_seq={}", current.identities()?.len(), current.icdc_seq);
            if check_only {
                return Ok(());
            }
            // Keys first: never end up with a registered device whose keys are lost.
            device.fbid = Some(s.boot.fbid);
            device.save()?;
            let n = e2ee::register(&s, &device, &current).await?;
            device.wa_device_id = Some(n);
            device.save()?;
            println!("Registered as {}", device.jid().unwrap_or_default());
            Ok(())
        });
    }

    // E2EE socket probe (phase 3): handshake + login, then print the first
    // nodes the server pushes for ~10 s. Sends nothing after login and acks
    // nothing, so the server keeps (and redelivers) any messages.
    if std::env::args().nth(1).as_deref() == Some("e2ee-connect") {
        let cookies = Cookies::load().ok_or_else(|| anyhow::anyhow!("not logged in"))?;
        return tokio::runtime::Runtime::new()?.block_on(async {
            let s = session::Session::connect(cookies).await?;
            let mut device = e2ee::Device::load().ok_or_else(|| anyhow::anyhow!("no E2EE device; run e2ee-register"))?;
            let mut sock = e2ee_socket::ChatSocket::connect(&s, &mut device).await?;
            println!("Noise handshake OK");
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut n = 0;
            while n < 40 {
                match tokio::time::timeout_at(deadline, sock.recv()).await {
                    Ok(Ok(Some(node))) => {
                        n += 1;
                        println!("[{n}] {}", e2ee_socket::describe(&node).chars().take(300).collect::<String>());
                    }
                    Ok(Ok(None)) => { println!("socket closed"); break; }
                    Ok(Err(e)) => { println!("error: {e:#}"); break; }
                    Err(_) => break,
                }
            }
            sock.close().await;
            Ok(())
        });
    }

    // Decode the local E2EE inbox with the Armadillo parser; prints kinds and
    // sizes only, never message text.
    // `e2ee-inbox --map`: sync the thread lists and check which inbox messages
    // map onto an E2EE thread (prints yes/no only).
    if std::env::args().nth(1).as_deref() == Some("e2ee-inbox") && std::env::args().any(|a| a == "--map") {
        let cookies = Cookies::load().ok_or_else(|| anyhow::anyhow!("not logged in"))?;
        return tokio::runtime::Runtime::new()?.block_on(async {
            let s = session::Session::connect(cookies).await?;
            let mut st = store::Store::default();
            st.me = s.boot.fbid;
            for db in [1, 95] {
                st.apply(&lightspeed::decode_payload(&s.ls_sync(db, None).await?)?);
            }
            for rec in e2ee_listen::read_inbox() {
                match e2ee_listen::to_incoming(&rec, s.boot.fbid) {
                    Some(ev) => {
                        let mapped = st.apply_e2ee(&ev);
                        println!("{} from_me={} -> mapped onto E2EE thread: {}", rec.enc_type, ev.from_me, if mapped { "yes" } else { "NO" });
                    }
                    None => println!("{} -> not a 1:1 displayable message", rec.enc_type),
                }
            }
            Ok(())
        });
    }

    if std::env::args().nth(1).as_deref() == Some("e2ee-inbox") {
        let path = e2ee_store::data_dir()?.join("e2ee-inbox.jsonl");
        for line in std::fs::read_to_string(path).unwrap_or_default().lines() {
            let v: serde_json::Value = serde_json::from_str(line)?;
            let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, v["plaintext"].as_str().unwrap_or(""))?;
            let shape = match armadillo::decode(&raw) {
                Ok(d) => {
                    let kind = match &d.content {
                        armadillo::Content2::Text { text } => format!("Text ({} chars)", text.chars().count()),
                        armadillo::Content2::Link { text, .. } => format!("Link ({} chars)", text.chars().count()),
                        armadillo::Content2::Media { kind, file, .. } => match file {
                            Some(f) => format!(
                                "Media {kind:?} (file {}, {} bytes, {}x{}, thumbnail {} bytes, cached {})",
                                f.mimetype,
                                f.file_length,
                                f.width,
                                f.height,
                                f.thumbnail.len(),
                                e2ee_media::cached_path(f).is_some_and(|p| p.exists())
                            ),
                            None => format!("Media {kind:?} (no file reference decoded)"),
                        },
                        armadillo::Content2::Reaction { .. } => "Reaction".into(),
                        armadillo::Content2::Edit { .. } => "Edit".into(),
                        armadillo::Content2::Revoke { .. } => "Revoke".into(),
                        armadillo::Content2::None => "None (protocol only)".into(),
                        armadillo::Content2::Unsupported => "Unsupported".into(),
                    };
                    format!("{kind}, reply={}, dsm_destination={}, skdm={}", d.reply_to.is_some(), d.destination_jid.is_some(), d.skdm.is_some())
                }
                Err(e) => format!("decode error: {e:#}"),
            };
            println!("{} {} -> {shape}", v["enc_type"].as_str().unwrap_or(""), v["timestamp"]);
        }
        return Ok(());
    }

    // Read-only: ask the E2EE server which Messenger devices our account has.
    if std::env::args().nth(1).as_deref() == Some("e2ee-devices") {
        let cookies = Cookies::load().ok_or_else(|| anyhow::anyhow!("not logged in"))?;
        return tokio::runtime::Runtime::new()?.block_on(async {
            let s = session::Session::connect(cookies).await?;
            let mut device = e2ee::Device::load().ok_or_else(|| anyhow::anyhow!("no E2EE device"))?;
            let sock = e2ee_socket::ChatSocket::connect(&s, &mut device).await?;
            let store = e2ee_store::SignalStore::open(&device)?;
            let mut listener = e2ee_listen::Listener::new(sock, store, s.boot.fbid);
            let mut backlog = Vec::new();
            let own = format!("{}@msgr", s.boot.fbid);
            let resp = listener.query_devices(&[own.as_str()], &mut backlog).await?;
            print!("{}", e2ee_socket::describe_tree(&resp, 4));
            listener.sock.close().await;
            Ok(())
        });
    }

    // Read-only: the E2EE media hosts (`media_conn`); prints hostnames and the TTL only.
    if std::env::args().nth(1).as_deref() == Some("e2ee-mediaconn") {
        let cookies = Cookies::load().ok_or_else(|| anyhow::anyhow!("not logged in"))?;
        return tokio::runtime::Runtime::new()?.block_on(async {
            let s = session::Session::connect(cookies).await?;
            let mut device = e2ee::Device::load().ok_or_else(|| anyhow::anyhow!("no E2EE device"))?;
            let sock = e2ee_socket::ChatSocket::connect(&s, &mut device).await?;
            let store = e2ee_store::SignalStore::open(&device)?;
            let mut listener = e2ee_listen::Listener::new(sock, store, s.boot.fbid);
            let conn = listener.media_conn(&mut Vec::new()).await?;
            println!("hosts {:?}, ttl {}s, auth {} chars", conn.hosts, conn.ttl_secs, conn.auth.len());
            listener.sock.close().await;
            Ok(())
        });
    }

    // E2EE receive (phase 3 steps 1–2): top up prekeys, then decrypt incoming
    // messages for N seconds (default 120). Plaintext goes to the local inbox
    // file only; the console shows ids, sizes and protobuf field outlines.
    if std::env::args().nth(1).as_deref() == Some("e2ee-listen") {
        let secs: u64 = std::env::args().nth(2).and_then(|a| a.parse().ok()).unwrap_or(120);
        let cookies = Cookies::load().ok_or_else(|| anyhow::anyhow!("not logged in"))?;
        return tokio::runtime::Runtime::new()?.block_on(async {
            let s = session::Session::connect(cookies).await?;
            let stamp = || chrono::Local::now().format("%H:%M:%S").to_string();
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
            let decrypted = e2ee_listen::run(
                &s,
                Some(deadline),
                None,
                |m| {
                    let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &m.plaintext).unwrap_or_default();
                    println!("[{}]   ✓ decrypted {} ({}, {} bytes) fields: {}", stamp(), m.id, m.enc_type, raw.len(), e2ee_socket::proto_outline(&raw));
                },
                |_receipt| {},
                |line| println!("[{}] {line}", stamp()),
            )
            .await?;
            println!("[{}] Done: {decrypted} message(s) decrypted", stamp());
            Ok(())
        });
    }

    // Diagnostics: connect MQTT, request older messages for the newest thread
    // (read-only), and report what arrived.
    // Read-only: fetch older messages for a thread (by name) with the
    // FetchMessages task over GraphQL, and print message counts.
    if std::env::args().nth(1).as_deref() == Some("debug-older") {
        let cookies = Cookies::load().ok_or_else(|| anyhow::anyhow!("not logged in"))?;
        let name = std::env::args().nth(2).unwrap_or_default().to_lowercase();
        return tokio::runtime::Runtime::new()?.block_on(async {
            let s = session::Session::connect(cookies).await?;
            println!("ls_version in use: {} ls_app_id {} mqtt_app_id {}", s.boot.ls_version, s.boot.ls_app_id, s.boot.mqtt_app_id);
            let names: Vec<&str> = s.cookies.0.split("; ").filter_map(|c| c.split('=').next()).collect();
            println!("cookie names: {names:?}");
            let mut st = store::Store::default();
            st.me = s.boot.fbid;
            let first = lightspeed::decode_payload(&s.ls_sync(1, None).await?)?;
            for c in first.iter().filter(|c| c.name.contains("SyncTransaction") || c.name.contains("Sequence") || c.name.contains("Cursor")) {
                let shape: Vec<String> = c.args.iter().map(|a| match a {
                    lightspeed::Arg::Str(x) => format!("Str(len {} {}…)", x.len(), x.chars().take(6).collect::<String>()),
                    other => format!("{other:?}"),
                }).collect();
                println!("  {} {shape:?}", c.name);
            }
            st.apply(&first);
            for c in first.iter().filter(|c| c.name == "updateReadReceipt") {
                if st.threads.get(&c.arg(1).int()).is_some_and(|t| st.thread_title(t).to_lowercase().contains(&name)) {
                    println!("  updateReadReceipt {:?}", c.args);
                }
            }
            for c in first.iter().filter(|c| c.name == "addParticipantIdToGroupThread").take(400) {
                if st.threads.get(&c.arg(0).int()).is_some_and(|t| st.thread_title(t).to_lowercase().contains(&name)) {
                    let shape: Vec<String> = c.args.iter().take(8).map(|a| match a {
                        lightspeed::Arg::Str(x) => format!("Str(len {})", x.len()),
                        other => format!("{other:?}"),
                    }).collect();
                    println!("  addParticipant {shape:?}");
                }
            }
            for c in first.iter().filter(|c| c.name == "updateOrInsertReactionV2" || c.name == "upsertReaction").take(3) {
                let shape: Vec<String> = c.args.iter().enumerate().map(|(i, a)| match a {
                    lightspeed::Arg::Str(x) => format!("{i}:{:?}", x.chars().take(24).collect::<String>()),
                    other => format!("{i}:{other:?}"),
                }).collect();
                println!("  {} {}", c.name, shape.join(" "));
            }
            println!("  read receipts total in snapshot: {}", first.iter().filter(|c| c.name == "updateReadReceipt").count());
            for c in first.iter().filter(|c| c.name == "insertNewMessageRange" || c.name == "updateExistingMessageRange") {
                if st.threads.get(&c.arg(0).int()).is_some_and(|t| st.thread_title(t).to_lowercase().contains(&name)) {
                    println!("  {} {:?}", c.name, c.args);
                }
            }
            let t = st
                .sorted_threads()
                .into_iter()
                .filter(|t| !t.e2ee)
                .find(|t| st.thread_title(t).to_lowercase().contains(&name))
                .map(|t| t.key)
                .ok_or_else(|| anyhow::anyhow!("no matching thread"))?;
            for round in 1..=3 {
                let before = st.messages.get(&t).map_or(0, Vec::len);
                let (ts, mid) = st.messages.get(&t).and_then(|v| v.first()).map(|m| (m.timestamp_ms, m.id.clone())).unwrap_or_default();
                let p = serde_json::json!({
                    "thread_key": t, "direction": 0, "reference_timestamp_ms": ts,
                    "reference_message_id": mid, "sync_group": 1, "cursor": st.cursors.get(&1),
                });
                let raw = s.ls_tasks(&[("228", p, format!("mrq.{t}"))]).await?;
                println!("  task response payload: {} chars", raw.len());
                let calls = lightspeed::decode_payload(&raw)?;
                let mut names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
                names.sort();
                names.dedup();
                st.apply(&calls);
                let cursor = st.cursors.get(&1).cloned();
                println!("  cursor known: {}", cursor.is_some());
                let delta = lightspeed::decode_payload(&s.ls_sync(1, cursor.as_deref()).await?)?;
                let mut dn: Vec<&str> = delta.iter().map(|c| c.name.as_str()).collect();
                dn.sort();
                dn.dedup();
                st.apply(&delta);
                println!("round {round}: messages {before} -> {}; task procs {names:?}; delta procs {dn:?}", st.messages.get(&t).map_or(0, Vec::len));
                let kinds = st.messages.get(&t).map(|v| v.iter().flat_map(|m| &m.attachments).fold(std::collections::BTreeMap::new(), |mut acc, a| {
                    *acc.entry(format!("{:?}{}", a.kind, if a.title.is_empty() { "" } else { "+title" })).or_insert(0) += 1;
                    acc
                })).unwrap_or_default();
                println!("  attachments: {kinds:?}");
                let reacted = st.messages.get(&t).map_or(0, |v| v.iter().filter(|m| !m.reactions.is_empty()).count());
                let mine = st.messages.get(&t).map_or(0, |v| v.iter().filter(|m| m.reactions.contains_key(&st.me)).count());
                println!("  messages with reactions: {reacted} (with yours: {mine})");
                let parts = st.threads.get(&t).map(|th| th.participants.clone()).unwrap_or_default();
                let replies: Vec<_> = st.messages.get(&t).into_iter().flatten().filter(|m| m.reply_to_id.is_some()).collect();
                let named = replies.iter().filter(|m| m.reply_to_user.is_some_and(|u| parts.contains(&u))).count();
                println!("  replies: {} (author known and a member: {named})", replies.len());
                println!("  members with a nickname here: {} of {}", st.nicknames.get(&t).map_or(0, |n| n.len()), parts.len());
                if round == 1 {
                    for m in st.messages.get(&t).into_iter().flatten().filter(|m| !m.attachments.is_empty()) {
                        for a in &m.attachments {
                            let path = a.preview_url.split('?').next().unwrap_or("").rsplit('/').next().unwrap_or("");
                            println!("    {} {:?} title={:?} sub={:?} file={path} {}x{}", &m.id[..12.min(m.id.len())], a.kind, a.title.chars().take(20).collect::<String>(), a.subtitle, a.width, a.height);
                        }
                    }
                }
            }
            Ok(())
        });
    }

    // Read-only: open the DGW Lightspeed socket and report what the server does
    // (with "hello": also open the database 1 sync stream).
    if std::env::args().nth(1).as_deref() == Some("debug-dgw") {
        let cookies = Cookies::load().ok_or_else(|| anyhow::anyhow!("not logged in"))?;
        let hello = std::env::args().nth(2).as_deref() == Some("hello");
        return tokio::runtime::Runtime::new()?.block_on(async {
            let s = session::Session::connect(cookies).await?;
            let url = dgw::lightspeed_url(&s.boot.ls_app_id, s.boot.fbid, &s.device_id, s.boot.region.as_deref());
            let mut ws = dgw::Socket::open(&url, &s.cookies.0, session::USER_AGENT).await?;
            println!("connected");
            if hello {
                let body = serde_json::json!({
                    "app_id": s.boot.ls_app_id, "request_id": 1, "type": 1,
                    "payload": serde_json::json!({
                        "database": 1, "epoch_id": store::now_ms() << 22, "failure_count": null,
                        "last_applied_cursor": null, "sync_params": session::ls_sync_params(),
                        "version": s.boot.ls_version,
                    }).to_string(),
                }).to_string();
                let mut f = dgw::establish(0);
                f.extend(dgw::data(0, 0, body.as_bytes()));
                ws.send(f).await?;
            }
            for _ in 0..4 {
                match tokio::time::timeout(std::time::Duration::from_secs(8), ws.recv()).await {
                    Ok(Ok(Some(mut b))) => for f in dgw::parse(&mut b) {
                        match f {
                            dgw::Frame::Data { stream, body, .. } => println!("data stream {stream}: {} bytes", body.len()),
                            other => println!("{other:?}"),
                        }
                    },
                    Ok(Ok(None)) => { println!("closed"); break; }
                    Ok(Err(e)) => { println!("error: {e:#}"); break; }
                    Err(_) => println!("(nothing for 8s)"),
                }
            }
            Ok(())
        });
    }

    // Read-only: the sticker tray and the first pack's stickers (counts only).
    if std::env::args().nth(1).as_deref() == Some("debug-stickers") {
        let cookies = Cookies::load().ok_or_else(|| anyhow::anyhow!("not logged in"))?;
        return tokio::runtime::Runtime::new()?.block_on(async {
            let s = session::Session::connect(cookies).await?;
            let packs = s.sticker_packs().await?;
            println!("{} packs; thumbnails present: {}", packs.len(), packs.iter().filter(|p| p.thumbnail.starts_with("http")).count());
            if let Some(first) = packs.first() {
                let stickers = s.sticker_pack(&first.id).await?;
                println!("first pack: {} stickers, images present: {}", stickers.len(), stickers.iter().filter(|s| s.image.starts_with("http")).count());
            }
            Ok(())
        });
    }

    if std::env::args().nth(1).as_deref() == Some("debug-mqtt") {
        let cookies = Cookies::load().ok_or_else(|| anyhow::anyhow!("not logged in"))?;
        let (shared, tx) = client::spawn(cookies, || {});
        let wait_online = std::time::Instant::now();
        while *shared.status.lock().unwrap() != client::Status::Online {
            anyhow::ensure!(wait_online.elapsed().as_secs() < 30, "not online: {:?}", shared.status.lock().unwrap());
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        let (thread, before) = {
            let st = shared.store.lock().unwrap();
            // Optional name filter; skips encrypted chats (their history isn't on Lightspeed).
            let name = std::env::args().nth(2).map(|n| n.to_lowercase());
            let t = st
                .sorted_threads()
                .into_iter()
                .filter(|t| !t.e2ee)
                .find(|t| name.as_ref().is_none_or(|n| st.thread_title(t).to_lowercase().contains(n)))
                .map(|t| t.key)
                .ok_or_else(|| anyhow::anyhow!("no matching thread"))?;
            (t, st.messages.get(&t).map_or(0, Vec::len))
        };
        std::thread::sleep(std::time::Duration::from_secs(12));
        if std::env::var_os("MESSENGER_IDLE").is_some() {
            std::thread::sleep(std::time::Duration::from_secs(60));
            println!("idle done; status {:?}", shared.status.lock().unwrap());
            return Ok(());
        }
        tx.send(client::Command::LoadOlder { thread })?;
        std::thread::sleep(std::time::Duration::from_secs(25));
        let st = shared.store.lock().unwrap();
        println!("thread: messages before={before} after={}", st.messages.get(&thread).map_or(0, Vec::len));
        return Ok(());
    }

    let cookies = match Cookies::load() {
        Some(c) => c,
        None => {
            let status = std::process::Command::new(std::env::current_exe()?).arg("login").status()?;
            if !status.success() {
                return Ok(());
            }
            Cookies::load().ok_or_else(|| anyhow::anyhow!("login did not complete"))?
        }
    };

    let mut viewport = eframe::egui::ViewportBuilder::default()
            .with_title("Messenger Desktop")
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([720.0, 480.0])
            .with_maximized(true)
            // Content extends under a transparent titlebar (traffic lights stay).
            .with_fullsize_content_view(true)
            // Transparent, so the native Liquid Glass behind egui shows through.
            .with_transparent(true)
            .with_titlebar_shown(false)
            .with_title_shown(false);
    // Inside the .app, macOS picks the light/dark/tinted icon from Assets.car.
    // eframe replaces a missing icon with egui's logo at runtime, and only an
    // empty `IconData` makes it leave the Dock icon alone. `cargo run` (no
    // bundle) gets our PNG.
    let in_bundle = std::env::current_exe().is_ok_and(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"));
    viewport = if in_bundle {
        viewport.with_icon(eframe::egui::IconData::default())
    } else {
        match eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
            Ok(icon) => viewport.with_icon(icon),
            Err(_) => viewport,
        }
    };
    let options = eframe::NativeOptions {
        viewport,
        // Always open maximized; panel widths are still restored from egui memory.
        persist_window: false,
        ..Default::default()
    };
    eframe::run_native(
        "Messenger Desktop",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let (shared, tx) = client::spawn(cookies, move || ctx.request_repaint());
            Ok(Box::new(ui::MessengerApp::new(cc, shared, tx)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
