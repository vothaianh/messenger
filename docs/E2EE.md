# End-to-end encrypted chats: phase 1 research (2026-10-28)

Read-only research into how Messenger web handles E2EE ("Armadillo") chats, for
adding them to Messenger Desktop. Nothing was registered or sent.

## What the web client does

Verified live in Chrome on facebook.com/messages:

| Piece | Finding |
|---|---|
| Thread list | LS GraphQL sync of **database 95** (`updateOrInsertThread`, thread type 15). Already implemented in `store.rs`. |
| Engine | "MAW" code running in a Web Worker, plus a `.wasm` crypto module |
| Socket | `wss://web-chat-e2ee.facebook.com/ws/chat`, Noise_XX / Noise_IK handshake (the WhatsApp multi-device protocol) |
| Auth | `MessengerWebInitData.cryptoAuthToken.encrypted_serialized_cat` (CAT, ~2.3 KB, valid 14 days). It is in the same `/messages/` HTML that `session.rs` already fetches. A GraphQL `MAWCatQuery` refreshes it. |
| Device registration | ICDC (`icdc` and `ICDC` appear 50 times in the JS bundles) |
| Message format | Armadillo protobufs: `MessageTransport` → `MessageApplication` / `ConsumerApplication` |
| Key storage | IndexedDB tables `e2ee_prekey`, `e2ee_prekeyGeneration`, `registration_id` |
| History backup | "Labyrinth" (`useMWEBBackupState`): server-side encrypted archive unlocked with the Messenger PIN |

## Protocol flow

From reading the open-source mautrix-meta (`pkg/messagix`, AGPL-3.0) and
whatsmeow (MPL-2.0). These were only read for protocol understanding. Don't copy
their code into this MIT/unlicensed project.

1. **Keys**: generate a Signal identity key pair, registration ID, signed
   prekey and one-time prekeys, and a random device UUID. Persist them (we lose
   the device if they're lost).
2. **ICDC fetch**: POST `https://reg-e2ee.facebook.com/v2/fb_icdc_fetch` (form:
   `fbid`, `fb_cat`, `app_id`, `device_id`). The response has the account's
   current device identity keys (base64url, 32 bytes each) and `icdc_seq`.
3. **ICDC register**: append our identity key and bump the seq. Sign an
   `ICDCIdentityList{seq, timestamp, devices, signing_device_index}` with our
   identity key, wrapped in `SignedICDCIdentityList`. POST
   `.../v2/fb_register_v…` with `e_regid`, `e_keytype`, `e_ident`,
   `e_skey_id/val/sig`, `icdc_list`, `icdc_ts`, `icdc_seq`, and `ihash` (first
   10 bytes of SHA-256 over the sorted identities). The response gives
   `wa_device_id`, so our JID becomes `<fbid>.<device>@msgr`.
4. **Connect**: Noise handshake to `web-chat-e2ee.facebook.com`. The
   `ClientPayload` carries `product = MESSENGER`, `fbCat`, `username = fbid`,
   `device`, and platform `BLUE_WEB`. After that it's the WhatsApp binary-node
   protocol: upload prekeys, receive `<message>` nodes, Signal-decrypt, then
   parse the Armadillo protobufs.
5. **Old history** (`securehistory`) is a separate, heavier flow. It needs the
   user's **Messenger PIN** (or recovery code), checked against Meta's HSM
   backup service ("Vesta"). Then it derives and enrolls a virtual device,
   fetches encrypted archive ranges and decrypts them ("Minos" cipher, epochs).

## Reusable Rust code

- **whatsapp-rust** (github.com/jlucaso1/whatsapp-rust, MIT, active, ~157k
  downloads) is a WhatsApp Web client.
  - Has: `wacore` with the Noise handshake, binary node codec, prekeys,
    sessions, and message send/receive. Its `waproto/whatsapp.proto` already
    has `ClientPayload.fbCat`, product `MESSENGER`, and some Armadillo messages.
  - Lacks Messenger mode: no ICDC, no `@msgr` server, no CAT handling, no web
    endpoint selection.
  - Its vendored `wacore/libsignal` is marked MIT but carries "Signal Messenger,
    LLC" copyright headers (upstream libsignal is AGPL-3.0). Check the licensing
    before shipping.

## Feasibility

- **New incoming and outgoing E2EE messages**: feasible. Every piece exists in
  open source and the token is already in our bootstrap. Estimate: several
  sessions. Most of the work is adding a "Messenger mode" on top of
  `whatsapp-rust`'s core and parsing Armadillo payloads.
- **Old history**: possible but the hardest part. It needs the user's PIN and
  the Vesta/Minos/epochs flow. Do it last.
- **Account impact**: registering adds a new E2EE device to the account, visible
  in the device list and possibly notified. Needs the user's explicit OK
  (phase 2).

## Phase 2 status

- Implemented in `src/e2ee.rs`. Keys are generated with `x25519-dalek` and
  signed with `xeddsa`, the ICDC protobufs are encoded with `prost`, and the
  device JSON is stored in the Keychain (`messenger-rs` / `e2ee-device`).
- `messenger-rs e2ee-register --check` (read-only) works: it fetched the
  account's 10 E2EE devices.
- The actual registration (`messenger-rs e2ee-register`) changes the user's
  account. The user runs it themselves; the agent's permission policy blocks it.
- **Done 2026-10-28**: the user ran it and the device registered as
  `<fbid>.91@msgr` (the account went from 10 to 11 E2EE devices). Don't
  register again: re-running prints "Already registered". Losing the Keychain
  item means registering a new device.

## Phase 3 plan (receive E2EE messages)

`whatsapp-rust` hardcodes its WebSocket URL (`wacore/src/net.rs`,
`WHATSAPP_WEB_WS_URL`) and builds the `ClientPayload` internally
(`wacore/src/store/device.rs`, `build_base_client_payload`). The plan is to
vendor it under `vendor/whatsapp-rust` (MIT: keep its LICENSE and attribution)
with a small "Messenger mode" patch:

1. Configurable socket URL → `wss://web-chat-e2ee.facebook.com/ws/chat`.
2. A login `ClientPayload` for Messenger: `product = MESSENGER`, `fbCat`,
   `username = fbid`, `device = 91`, platform `BLUE_WEB`, no pairing or ADV
   flow. The ADV identity is a zero-filled placeholder, as in the reference
   clients.
3. JID server `msgr` alongside `s.whatsapp.net`.
4. Seed its device store from our Keychain `e2ee::Device` (identity, regid,
   signed prekey) and persist Signal sessions and prekeys locally. Use a
   SQLite store in `~/Library/Application Support/Messenger Desktop/`, with the
   encryption key in the Keychain.
5. After connecting: upload one-time prekeys, receive `<message>` nodes, decrypt
   them, unwrap `MessageTransport` → `MessageApplication` /
   `ConsumerApplication`, and map text, media, reactions and edits into
   `store.rs` for E2EE threads (thread key ↔ `<user>@msgr`).
6. UI: replace the E2EE notice with the normal conversation view, showing
   messages received since registration, plus a "history needs PIN" banner
   until phase 5.

Messages sent to our device before we first connect wait on the server as
offline messages. Connect soon after registration.

## Phase 3 progress

- **Step 1, done (2026-10-28): Noise handshake and login work.**
  `src/e2ee_socket.rs` uses the MIT crates `wacore-noise`, `wacore-binary`
  (with `default-features = false`, because its `simd` feature needs nightly),
  `wacore-libsignal` and `waproto` (buffa types: `MessageField::some`, typed
  enums). It sends a Messenger `ClientPayload` (`product = Messenger`, CAT,
  device 91, `BlueWeb`, app version 301.0.2). The server cert verifies against
  the standard WA root key. `messenger-rs e2ee-connect` printed `<success>`,
  `<ib><edge_routing/>` and `<ib><offline/>`.
- The static Noise key lives in the Keychain device JSON (`noise_priv`), created
  on the first connect.
- Next steps:
  1. Prekeys: `<iq xmlns="encrypt" type="get"><count/>`, then upload one-time
     prekeys (`type="set"`: registration, type, identity, list, skey). Persist
     them.
  2. Signal store (identities, sessions, prekeys, sender keys) on disk, plus
     decryption of `<enc type="pkmsg|msg|skmsg">`.
  3. Unwrap the Armadillo protos (`MessageTransport` → `MessageApplication` /
     `ConsumerApplication`). Check which of these `waproto` already has.
  4. Send `<ack>` and receipts, and retry receipts on decrypt failure.
  5. Wire it into `client.rs`, `store.rs` and the UI.
- **Steps 1–2 implemented (2026-10-28).**
  - `src/e2ee_store.rs` is the Signal store: a clone-able `Arc<Mutex>` handle,
    because libsignal's `message_decrypt` takes one `&mut dyn` per store role.
    It's JSON at `~/Library/Application Support/Messenger Desktop/e2ee-store.json`
    (0600), rewritten on every change.
  - `src/e2ee_listen.rs`:
    - Prekeys: `<iq xmlns=encrypt type=get><count/>`; below 50, upload 100
      (`registration`, `type`, `identity`, `list` of `key{id,value}`, and
      `skey{id,value,signature}`). Verified: the server count went 0 → 100.
    - Decrypts `pkmsg`/`msg` with sessions and `skmsg` with sender keys. It
      removes a consumed one-time prekey only after the session is saved.
    - Appends plaintext to `e2ee-inbox.jsonl` (0600) **before** sending `<ack>`.
    - Answers pings, and acks notifications and receipts.
  - `messenger-rs e2ee-listen [secs]` prints ids, sizes and protobuf field
    outlines, never content.
- Not yet verified: decrypting a real message. Test by sending from the
  user's phone in an E2EE chat; their own devices, including 91, get a copy.
- **Findings from live tests (2026-10-28 20:00–20:15):**
  - The connection needs `<iq xmlns=passive type=set><active/>` after login and
    a `w:p` ping every 20s. Without them the server resets it within ~2 min.
    Servers also restart with `<stream:error code=503>`, so reconnect.
  - The user's phone (device 86) sent a message after registration, but it
    reached device 91 as `<message type=text>` with `<unavailable>` and **no
    `<enc>` for us**: the phone's cached device list didn't include 91 yet.
  - Acks to `<fbid>:86@msgr` lost the `:86`, because upstream
    `wacore-binary` encodes msgr device JIDs as `JID_PAIR`. Fixed by vendoring
    `wacore-binary` with an `FB_JID` encoder patch
    (`vendor/wacore-binary/VENDORED.md`); the roundtrip test passes.
  - Even with the correct `to`, a plain `<ack class=message>` for the
    `<unavailable>` message doesn't stop redelivery. Next: send a **retry
    receipt** (`<receipt type="retry">` with `<retry count id t v>`,
    `<registration>` and `<keys>` holding our identity, a one-time prekey and
    the signed prekey). It asks the sending device to re-encrypt for 91, and
    should also settle the queue entry.
- **Retry receipts implemented (2026-10-28).** On `<unavailable>` without
  `<enc>`, or on a decrypt failure, the client sends
  `<receipt type=retry to=<sender device>>` with `retry{count,id,t,v=1}`,
  `registration` and `keys` (type 0x05, identity, a fresh one-time prekey,
  skey, and a zero-filled `ADVSignedDeviceIdentity`), capped at 5 per message,
  followed by the ack. The server accepted it (no error). The phone didn't
  re-send within 90s; it likely has to be in the foreground to process retries.
- **First successful E2EE decryption (2026-10-28 20:38).**
  - Root cause of the earlier failures: upstream `wacore-binary` encodes
    **every** msgr JID as `JID_PAIR`. That drops the device (`:86`), and even
    for bare `fbid@msgr` it's rejected. The server reset the connection on a
    `fbid:devices` query and silently ignored acks whose `recipient` was
    encoded that way. The vendored patch now writes `FB_JID` for all msgr JIDs
    (test `bare_msgr_jid_uses_fb_jid_token`).
  - `messenger-rs e2ee-devices` (read-only) shows the server's device list for
    the account: 11 devices including 91.
  - The user's message from the phone (device 86) to an E2EE 1:1 chat arrived as
    `pkmsg` and decrypted to 128 bytes, outline `#1:bytes[61] #2:bytes[63]`
    (Armadillo `MessageTransport`: payload + protocol). We sent a `type=sender`
    receipt. The offline queue then drained: the stale `<unavailable>` message
    and its receipts stopped redelivering.
  - Next: the Armadillo protos (`MessageTransport`, `MessageApplication`,
    `ConsumerApplication`) to get text, media and reactions, then wire into the
    app.

## Phase 4: sending text to 1:1 chats (implemented 2026-10-29, awaiting first user test)

- `Listener::send_text` (`src/e2ee_listen.rs`):
  1. `fbid:devices` for the peer and for us; drop our own device (91).
  2. Devices without a Signal session get a prekey bundle via
     `<iq xmlns=encrypt type=get><key><user jid=…/>` → `process_prekey_bundle`.
  3. One `MessageApplication` (ConsumerApplication text, consumer v1;
     metadata franking key = 32 random bytes, franking version 0) shared by all
     devices. Per device, a `MessageTransport` (application v2, future-proof
     PLACEHOLDER, random 1–15 byte padding, backup directive UPSERT with the
     message id; for our own devices also the DSM destination `peer@msgr`).
  4. `<message id to=peer@msgr type=text><participants><to jid><enc v=3
     type=pkmsg|msg>` plus `<franking><franking_tag>` = HMAC-SHA256(franking
     key, MessageApplication bytes). The id is `(ms << 22) | 22 random bits`.
  5. Wait for `<ack id>` (an `error` attribute means rejected), then log it to
     `e2ee-inbox.jsonl` as `enc_type: "sent"`, so it replays after restarts.
- UI: encrypted chats use the normal composer (text and 👍 only; no files,
  replies, reactions, edits or typing indicators yet).

## Media in encrypted chats (2026-10-29; sending awaiting first user test)

- Scheme (`src/e2ee_media.rs`), same as WhatsApp: random 32-byte media key;
  HKDF-SHA256 with info "WhatsApp Image/Video/Audio/Document Keys" gives IV,
  AES-256 key and HMAC key. Blob = AES-256-CBC(PKCS#7) || first 10 bytes of
  HMAC-SHA256(iv || ciphertext).
- Hosts: `<iq xmlns="w:m" type="set"><media_conn/>` on the E2EE socket returns
  an auth token (TTL 300 s) and hosts: `scontent.*.fbcdn.net` (download) and
  `rupload.facebook.com` (upload).
- Upload (web module `WAMediaUrl` / `WAHttpRegularUploadMedia`): POST the
  blob, `Content-Type: text/plain`, to
  `https://rupload.facebook.com/wa-msgr/mms/<image|video|audio|document>/<b64url(enc sha256)>?auth=<auth>&token=<b64url(enc sha256)>`
  (`mediaUploadRoot` is `/wa-msgr/mms`). The JSON answer has `direct_path`
  and optionally `object_id` and `handle`.
- Message (web module `WAMediaTransport.pb`, field numbers confirmed):
  `ConsumerApplication.Content.imageMessage(2) = { image(1): SubProtocol {
  payload: ImageTransport, version: 1 }, caption(2) }`.
  `ImageTransport { integral(1) { transport(1): WAMediaTransport }, ancillary(2)
  { height(1), width(2) } }`. `WAMediaTransport { integral(1) { fileSha256(1),
  mediaKey(2), fileEncSha256(3), directPath(4), mediaKeyTimestamp(5) },
  ancillary(2) { fileLength(1), mimetype(2), thumbnail(3) { jpegThumbnail(1),
  …, width(3), height(4) }, objectId(4) } }`. Video ancillary: height(4), width(5).
- Receiving: the listener downloads `https://<host><direct path>`, checks and
  decrypts, and caches the file (0600) in
  `~/Library/Caches/Messenger Desktop/e2ee-media/<sha256>.<ext>` before the UI
  sees the message.
- Sending: pictures only so far (`Listener::send_image`): encrypt, upload,
  then the normal per-device send. A plain copy goes into the cache.

## Old history with the PIN: research (2026-10-29)

Read-only look at the web client's encrypted-backup ("EB") code. Nothing was
entered, registered or restored.

- **PIN check = Vesta, an OPAQUE login against Meta's HSMs.** GraphQL:
  `VestaServerGraphQLLoginProviderInitQuery` (`28587565877599806`),
  `...BeginLoginMutation` (`28781838094734033`),
  `...FinishLoginMutation` (`28557176640587196`); registration has the same
  three (`...RegisterProvider...`). User info:
  `useMWChatEncryptedBackupsVestaUserInfoRefetchQuery` (`23870520485955982`).
- **The client crypto is a Rust crate compiled to wasm** (`VestaGetWasm`,
  protobuf API `VestaWasm.pb`: `ValidateHsmKey`, `BeginRegister`,
  `FinishRegister`, `BeginLogin{password}` → `{message, client_state}`,
  `FinishLogin{client_state, l2_message, l2_message_sig, island_ed25519_pub}`
  → `{session_key, export_key}`, `UnpackLoginPayload{login_payload,
  export_key}` → user secret). Vendored crates in the wasm: **opaque-ke
  1.3.0**, curve25519-dalek 3.2 (Ristretto255), sha2 0.9 (SHA-512),
  ed25519-dalek 2.2 (the HSM "island" signs its messages), rsa 0.9 (HSM RSA
  key at registration), pbkdf2 0.12, hkdf 0.11, aes-gcm 0.10.
- **Archive ("Minos")**: `useMWEncryptedBackupsFetchBackupIdsV2Query`
  (`25220416984279164`), `EBMinosMekFetchOnMissQuery` (`27665928069709083`,
  message encryption keys), `EBMinosFetchContactKeysQuery`
  (`26531892553066563`); decryption in `EncryptedBackupsSymmetricEncryptionUtils`.
  The restore registers a virtual device (`...GetVirtualDevices...`).
- **Risk**: the HSM counts failed logins (`login_attempts`) and, like other
  HSM PIN schemes, can lock or wipe the backup after too many. A client that
  gets any OPAQUE detail wrong (ciphersuite, slow hash, identities, context)
  burns an attempt with a correct PIN. Validate the implementation offline
  (e.g. against opaque-ke 1.3 test vectors and our own server) before the
  first real login, and stop after one failure.

### Offline cross-test of the Vesta client (2026-10-29)

- The Vesta client wasm (`VestaGetWasm`, ~760 KB, `static.xx.fbcdn.net`) is a
  WASI command module (`xplat/rust/vesta_client`) with one export,
  `execute_command`. The web's `WAWasmReactor` writes a `CliCommand` protobuf
  to the file `input` on a virtual drive and reads a `CliResponse` from
  `output`. Field numbers: `CliCommand{validateHsmKey=1, beginRegister=2,
  finishRegister=3, beginLogin=4, finishLogin=5, unpackLoginPayload=6}`,
  `CliResponse{errorCode=1, ... same order from 2}`; requests and replies as
  listed above. Error codes: 1 InsufficientResultBuffer, 2 InvalidUserPassword,
  3 MalformedServerResponse, 4 MalformedClientInput, 5 Encryption,
  6 Decryption, 7 Serialization, 8 Deserialization, 9 RsaInvalidKey,
  10 RsaVerification, 11 Ed25519InvalidKey, 12 Ed25519Verification.
- It runs fine under Node's WASI with a temp dir preopened (no network).
  `BeginLogin` gives a 98-byte `l1` = opaque-ke 1.3 `CredentialRequest`
  (Ristretto255 alpha, nonce, empty info with u16 length, ephemeral key).
- Not plain opaque-ke defaults: the slow hash is PBKDF2
  (`opaque_slow_hash_pbkdf.rs`, salt label `OPAQUE_PBKDF2_SALT`), the
  server messages are protobuf-wrapped (`proto/messages.rs`:
  `registration_response_bytes`, `login_first_message_bytes`,
  `login_second_message_bytes`, ...), and HSM signatures look like a
  structured "OTA" signature (errors `OtaSignature1Unverified`,
  `OtaInvalidRole`, `Expired`). A bare Ed25519 signature over the opaque
  `CredentialResponse` fails with error 12, whatever the hashing.
- So re-implementing the client means disassembling the wasm for the PBKDF2
  parameters and the signature format. The safer option is to run this same
  wasm in the app (e.g. `wasmi` with a tiny in-memory WASI), fetched at
  runtime like the web does: byte-for-byte the web's client, so a correct PIN
  can't fail because of us.

### PIN login flow (from `E2EEBackupSecretRestoreModule`, 2026-10-29)

1. `VestaServerGraphQLLoginProviderInitQuery` with
   `{request: {fbid, product_use_case: "MSGR_ENC_BACKUP",
   vesta_client_api_version: 6}}` →
   `fbid_based_auth_layer_vesta_login_init {base64url_encoded_island_rsa_pub(_signature),
   base64url_encoded_opaque_pub(_signature),
   base64url_encoded_island_ed25519_pub(_signature), is_registered,
   attempts_remaining}`. Stop if not registered.
2. wasm `validateHsmKey(island_rsa_pub, sig)` and
   `validateHsmKey(island_ed25519_pub, sig)`; stop if either is false.
3. wasm `beginLogin(pin)` → `l1`, client state.
4. `...BeginLoginMutation` `{input: {fbid, product_use_case,
   base64url_encoded_opaque_l1, vesta_client_api_version: 6,
   vesta_client_protocol_version: 1}}` →
   `fbid_based_auth_layer_vesta_begin_login {base64url_encoded_opaque_l2,
   base64url_encoded_opaque_l2_sig}`.
5. wasm `finishLogin(l2, l2_sig, island_ed25519_pub, state)` → `l3`,
   session key, export key.
6. `...FinishLoginMutation` (same input with `base64url_encoded_opaque_l3`) →
   `fbid_based_auth_layer_vesta_finish_login_v2 {login_success,
   base64url_encoded_login_payload, attempts_remaining,
   trigger_post_login_register}` or `XFBVestaError {vesta_error_code}`.
7. wasm `unpackLoginPayload(payload, session_key, export_key,
   use_protobuf_mode = true)` → the user secret (backup key data).

The wasm URL is in the `/messages/` HTML as bx asset `72238`
(`"72238":{"uri":"…wasm…"}`). Next to research: what the web does with the
secret (`useMWChatEncryptedBackupsAddDevice`, virtual devices, `EBMinos*`
queries) to fetch and decrypt the archived messages.

### After the PIN (2026-10-29)

- `mwEBGetPINSecretFromVesta` runs the Vesta login above and decodes the
  user secret as UTF-8 text: it is the account's **recovery code**. A wrong
  PIN surfaces as `BackupErrorType.INVALID_PASSWORD`.
- `mwEBAddDevice` → `EBSMAPI.addDeviceWithRecoveryCode({recoveryCode,
  userId})`, which runs in the MAW worker (`EBDeps.getEBSMAPI()`,
  `EBSMWorkerThreadAPI`, `EBAddDeviceWithRecoveryCode`, `EBAddDeviceGraphQL`):
  it adds this client as a backup device, then
  `MWEncryptedBackupsInitializeRestoreDeferred` starts the restore.
- The restore is the Minos pipeline, also mostly in the worker: `EBMinosDb`
  (IndexedDB), `useMinosMailboxKeyFetch`, `EBMinosProcessMessages`
  (`EBMinosMessageEncryptionVersion`, `...MetadataVersion`), LS procedures
  `LSMinosVerifySingleEpoch`, `LSWriteMinosPublicKey`, and GraphQL
  `EBMinosMekFetchOnMissQuery`, `EBMinosFetchContactKeysQuery`,
  `useMWEncryptedBackupsFetchBackupIdsV2Query`. The worker bundle isn't in
  the page's resource list; it has to be located (MAW worker init script) to
  read the key derivation from the recovery code and the message decryption.
- Plan: (1) Vesta login in the app via the wasm (in-memory WASI on `wasmi`),
  ending with the recovery code; (2) read the worker's EB/Minos code for the
  recovery-code key derivation, device add and archive decryption; (3)
  implement those; (4) PIN screen. Also: since the recovery code alone
  unlocks the backup, the app could accept it directly as an alternative.

### In the app (2026-10-29)

- `src/vesta.rs` runs the Vesta wasm in `wasmi` with an in-memory WASI
  (files `input`/`output` only; stdout/stderr dropped). Test:
  `VESTA_WASM=/path/vesta.wasm cargo test vesta`.
- The Vesta `fbid` field is not the user's fbid: it's the
  `client_generated_id` of the backup's virtual device with
  `device_type == "HSM"` (`MWChatEncryptedBackupsSettingsDialogQuery`,
  `31918369997811363`, `data.xfb_backup.virtual_devices`). With the real
  fbid the init query fails with `field_exception`.
- `messenger-rs debug-vesta-status` (read-only, no PIN): finds the module
  link, loads the module, reads `is_registered` / `attempts_remaining`, and
  verifies the HSM keys with the module. Verified working.
- `vesta::secret_from_pin` implements the full login; not run yet (it sends
  the PIN). The restore after it (recovery code → device add → Minos) is
  still to be researched.

### Restore internals (2026-10-29, later)

- The web worker is `static_resources/webworker/init_script/` and loads the
  same module registry, so its modules can be read from the page.
- `EBAddDeviceWithRecoveryCode`: waits for the "EBLS" database (the
  encrypted-backup Lightspeed DB inside the worker), reads the backup id from
  its `encrypted_backups` table, calls `EBAddDeviceGraphQL.addDeviceGraphQL
  ({backupId, recoveryCode, userId})`, then waits for a row in
  `secure_encrypted_backups_recovery_code_status` and checks it with
  `MWEncryptedBackupsRecoveryCodeStatus`. The restore itself then runs as
  Lightspeed stored procedures in that DB (`LSMinosVerifySingleEpoch`,
  `LSWriteMinosPublicKey`, ...) plus the EB/Minos JS and a second wasm
  (`COs9e0Kj0ic.wasm`, VOPRF over Ristretto). Re-implementing it means
  re-implementing that whole EBLS state machine: far bigger than Vesta.

### Minos crypto is a wasm too (2026-10-29)

- The archive crypto runs in a second Meta wasm, `EBGetMinosWasm`, bx asset
  **31853** (also in the `/messages/` HTML). Interface `EBMinosWasm.pb`,
  wrapper `MinosCommandSpec`, 97 specs. JS wrappers `EBMinosWasm*`:
  `DeriveMessagingMailboxKeypairs`, `DeriveMailbox{Encryption,Signing,Auth}Keypair`,
  `MandrakeValidateAndDecryptSelfMmk` / `...NewMmkFromMailbox` /
  `MandrakeDecryptMek` (mailbox master key → message encryption keys),
  `DecryptMekForDistribution(FromTransportSender)`,
  `DecryptAndVerifyMessage`, `DeriveAttachment{PrimaryKey,AccessToken}Secret`,
  `MinosThreadIdFrom{OneToOneThread,ActThreadId}`.
- Plan for the native restore: reuse the `vesta.rs` wasm host for this
  module, and write the orchestration in Rust: recovery code → backup device
  (`EBAddDeviceGraphQL`) → mailbox keys → MEKs (`EBMinosMekFetchOnMissQuery`)
  → archived messages → `DecryptAndVerifyMessage` → store. Next step: read
  the JS around `EBMinosLoadMailboxKeys`, `EBMinosMailboxKeysCache`,
  `RecoveryCodeDB`, `EBCryptoUtils` for how the recovery code becomes the
  mailbox keys, and how messages are fetched.

### Adding the backup device (EBAddDeviceGraphQL, 20 KB)

Order of operations:
1. GraphQL `EBAddDeviceGraphQLEBFetchVirtualDeviceInfoQuery` (virtual device
   info).
2. `LSParseRecoveryCode_v2` (1.3 KB): `parseLSRecoveryCode`,
   `GF32RecoveryCodes` (the code's alphabet/checksum), `deriveLSKey`,
   `checkLSRecoveryCodeMatch`.
3. `EBGetLSVirtualDevices`, `getDeviceRegistrationId`.
4. `LSAddDeviceWithVirtualDeviceAndDecryptionKeyStoredProcedure`.
5. Labyrinth wasm `deriveAddDeviceEpochs` (`EBLabyrinthWasm*`, a third
   module, `EBWasm.pb`).
6. `LSDecryptVirtualDeviceSecretsForDeviceAdditionV2` (**28.7 KB** compiled
   Lightspeed procedure: base64, auth/encryption/signature key pairs, derived
   keys, signatures with the Armadillo identity key, debug tokens).
7. `LSHandleAddDeviceSuccessStoredProcedure`.

Each LS procedure is generated JS calling native crypto ops (`LSCreate*`,
`.nop` modules). Porting this step alone means translating ~30 KB of
generated code and its crypto ops exactly, and it changes the account's
backup (adds a device), so it can only be tested for real once.

## History restore via the hidden web engine (2026-10-29)

Chosen over a native rewrite (the backup engine is ~30 KB of generated LS
procedures plus two more wasm modules, and its device-add step changes the
account, so it can't be iterated on safely).

- `macos::web_open` keeps an off-screen WKWebView on facebook.com/messages
  with the login window's (default, persistent) WebKit data store, so it's
  logged in. `web_call_with(body, args)` runs an async function in the page
  world via `callAsyncJavaScript` (arguments passed as data, not code).
  Probe (`MESSENGER_WEB_PROBE=1`) confirmed: logged in, `require` available,
  Vesta status readable.
- `RESTORE_JS` (ui.rs) calls the web's own `mwEBGetPINSecretFromVesta` with
  the args its PIN dialog hook builds, and an `addDevice` that runs
  `mwEBAddDevice.EBAddDevice` with `require('CometRelayEnvironment')` and the
  `LSDatabaseSingleton` DB. QPL flows are no-op proxies.
- The PIN dialog (E2EE chat: "Restore with PIN") gets status and the HSM
  client id via `vesta.rs` (read-only), allows one attempt per click and
  never retries.
- Not done yet: reading the restored messages out of the web client's
  database into our store (inspect after a first real restore).
