# Messenger Desktop (built with Rust)

**Messenger Desktop** is a fast, native **Messenger desktop app for Mac**, written
in **Rust**. It is an unofficial **Facebook Messenger client for macOS** with an
Apple **Liquid Glass** interface, real-time messaging, group chats, reactions,
photos, and **end-to-end encrypted chats**. No Electron and no embedded browser:
one small native app (about 20 MB) that opens instantly with your recent chats.

> **Unofficial project.** Messenger Desktop is not affiliated with, endorsed by,
> or sponsored by Meta Platforms, Inc. "Messenger" and "Facebook" are trademarks
> of Meta. Meta offers no public API for personal chats, so using a third-party
> client may break Meta's Terms of Service and could get your account
> checkpointed or restricted. It also breaks whenever Meta changes its web app.
> Use it at your own risk.

## Why Messenger Desktop?

- **Native macOS app, built with Rust.** Small (about 20 MB, a 10 MB download),
  low memory, starts in a moment. The UI is drawn with
  [egui](https://github.com/emilk/egui), and the surfaces use real macOS Liquid
  Glass (`NSGlassEffectView`).
- **Instant start.** Your inbox and recent messages load from a local cache
  while it reconnects in the background, with no loading screens.
- **Real-time.** New messages, typing indicators, reactions and read receipts
  arrive live, over the same real-time gateway that facebook.com/messages uses.
- **End-to-end encrypted chats.** Messenger now encrypts most 1:1 chats.
  Messenger Desktop registers as one of your devices and decrypts them on your
  Mac with the Signal protocol, so it can show and send messages in 🔒 chats.
- **Private by design.** You sign in on Facebook's own page; your password is
  never seen or stored. Session cookies live in the macOS Keychain, and local
  data is readable only by your user account.

## Features

### Chats and inbox
- Chat list with avatars, last-message previews, unread dots, relative times,
  search, and All / Unread / Groups filters
- Group chats with member nicknames, member list and per-group quick emoji
- Messages grouped like Messenger, with time separators, sender names and big
  emoji
- Older history loads automatically as you scroll up
- Unread count on the Dock icon, and native macOS notifications that skip muted
  and open chats

### Messaging
- Send text, emoji and the chat's quick emoji (👍 or the group's own)
- **@mentions** in groups: type `@` to pick a member; tags are highlighted and
  sent as real Messenger mentions
- Reply, edit (within 15 minutes) and unsend for everyone
- Send photos and files: attach, or drag and drop onto the window
- Delivery status under your last message (sending, sent, delivered) and "seen
  by" avatars showing who read up to where
- Typing indicators

### Reactions
- Right-click any message for a native Liquid Glass menu with the Messenger
  reaction set (❤️ 😆 😮 😢 😡 👍)
- **+** opens a full emoji picker like the web app's: search, your reactions,
  every emoji by category, and a category bar

### Media and links
- Photos, videos, stickers, files and voice messages
- Sticker panel in Liquid Glass with a sticker store: browse packs, preview
  them, add or remove them from your tray, send from any pack
- Photos open in an in-app viewer; videos play in a native AVKit player window
- Shared posts and reels as preview cards (picture, title, source)

### End-to-end encrypted (🔒) chats
- Receive and send text in encrypted 1:1 chats
- Send photos (encrypted and uploaded the same way the web client does)
- Receive photos and videos: downloaded, verified and decrypted on your Mac
  (implemented, not yet confirmed against a real incoming photo)
- Delivery and read receipts
- Older encrypted history, which needs your Messenger PIN, isn't supported yet

### macOS design
- Apple **Liquid Glass**: floating glass sidebars and a glass composer over a
  blurred window background
- SF Symbols icons, SF Pro text, adjustable text size (⌘ + / ⌘ − / ⌘ 0)
- Original app icon with light, dark and tinted variants

## Download and build

Messenger Desktop builds from source on macOS (Apple Silicon). You need
[Rust](https://rustup.rs) and, for the layered app icon, Xcode's command line
tools.

```sh
git clone <this repository>
cd messenger
./bundle.sh                         # builds "target/Messenger Desktop.app"
open "target/Messenger Desktop.app"
```

For a smaller, optimized build (about 20 MB, plus a zip for sharing):

```sh
DIST=1 ./bundle.sh
```

`bundle.sh` signs the app with your local "Apple Development" certificate, so
it runs on your own Mac. To give the app to other people, sign it with a
"Developer ID Application" certificate and notarize it.

### First launch

1. The app opens on Facebook's login page, inside its own window. Sign in
   there yourself; two-factor authentication and security checks work as
   usual. The same page comes back if your session expires.
2. The app restarts with your inbox. Only the session cookies (never
   your password) are saved, in the macOS Keychain under `messenger-rs`.
3. Encrypted chats need a one-time device registration
   (`messenger-rs e2ee-register`). This adds "Messenger Desktop" as a device on
   your account.

To sign out, use **⋯ → Log out**. This also deletes the local cache.

## Privacy and security

- **Your password never touches the app.** You type it into Facebook's own
  login page.
- **Cookies** are stored in the macOS Keychain.
- **Encryption keys** for your E2EE device are in the Keychain. The Signal
  session store, decrypted messages and media are kept in
  `~/Library/Application Support/Messenger Desktop/` and
  `~/Library/Caches/Messenger Desktop/`, all readable only by your user account
  (mode 0600).
- **No telemetry, no third-party servers.** The app talks only to Facebook's own
  endpoints, the same ones the web app uses.

## How it works

Messenger Desktop speaks the same private protocols as facebook.com/messages:

- **Lightspeed** for regular chats: a GraphQL snapshot sync, then live updates
  over Meta's **DGW gateway** WebSocket (`gateway.facebook.com/ws/lightspeed`).
  Older history uses a fetch task plus a delta sync over HTTPS.
- **Armadillo** for end-to-end encrypted chats: a Noise-protocol WebSocket
  (`web-chat-e2ee.facebook.com`), the Signal protocol for messages, and
  WhatsApp-style encrypted media.

| File | Role |
|---|---|
| `src/login.rs` | Standalone WebKit login window (`messenger-rs login`); the app itself shows the login page in its main window (`macos::login_view`) |
| `src/session.rs` | Page bootstrap (tokens, versions), GraphQL Lightspeed requests, uploads |
| `src/dgw.rs` | DGW gateway transport: frames, streams, acks |
| `src/client.rs` | Background engine: sync, live updates, commands, history, recovery |
| `src/lightspeed.rs` | Decoder for Lightspeed payloads (`[5,"procName",args…]`) |
| `src/store.rs` | In-memory inbox model and the on-disk cache |
| `src/e2ee*.rs`, `src/armadillo.rs` | Encrypted chats: device registration, socket, Signal store, receive/send, media, message protobufs |
| `src/macos.rs` | Liquid Glass, native menus, notifications, SF Symbols, Dock badge |
| `src/ui.rs` | The egui interface |

Research notes for encrypted chats are in [`docs/E2EE.md`](docs/E2EE.md);
contributor notes, including the visual style rules, are in
[`AGENTS.md`](AGENTS.md).

## Protocol notes

Verified against the live web app (October 2026):

- Live updates come from DGW, `wss://gateway.facebook.com/ws/lightspeed`.
  Authentication is the session cookie plus `x-dgw-uuid` = your user id. Each
  database sync and each task gets its own stream. The older MQTT endpoint
  (`edge-chat.facebook.com`) still answers requests but no longer pushes updates.
- Initial inbox: `LSPlatformGraphQLLightspeedRequestQuery`, database 1 (inbox)
  and 95 (encrypted chat list). A delta sync from a cursor uses request type 2.
- Argument positions:
  - `upsertMessage`: 0 text, 3 threadKey, 5 timestamp, 8 messageId, 9 otid,
    10 senderId, 23 replySourceId, 28 replyText, 29 replyToUserId
  - `deleteThenInsertThread`: 0 lastActivity, 1 lastRead, 2 snippet, 3 name,
    4 picture, 7 threadKey, 9 type, 31 customEmoji
  - `addParticipantIdToGroupThread`: 0 thread, 1 contact, 2 readWatermark,
    4 deliveredWatermark, 5 nickname
  - `verifyContactRowExists`: 0 id, 2 picture, 3 name
  - `upsertReaction`: 0 thread, 2 mid, 3 actor, 4 emoji
  - `updateReadReceipt`: 0 ts, 1 thread, 2 contact
  - `insertBlobAttachment`: 3/8 urls, 14/15 size, 27 thread, 32 mid
  - `insertXmaAttachment` (shares): 8 preview, 11 MIME type, 13/14 size,
    25 thread, 30 mid, 58 title, 59 source, 102 page name
  - `executeFirstBlockForSyncTransactionV4`: 3 cursor
- Stickers use the web picker's GraphQL queries: `CometStickerPickerCardQuery`
  (doc `28475074195489368`, `{"feedbackID":null,"scale":3,"stickerInterface":"MESSAGES"}`)
  for the tray, `picker_plugins.sticker_picker.sticker_store.tray_packs`;
  `CometStickerPickerPackContentRootQuery` (doc `25236323532672635`,
  `{"packID","stickerWidth":128,"stickerHeight":128,"scale":3}`) for a pack's
  stickers. A sticker is sent with task 46, `send_type` 2 and `sticker_id`.
- Task labels (from the open-source mautrix-meta and the web client): 46 send,
  21 read, 29 react, 33 unsend, 742 edit, 228 fetch messages, 145 fetch
  threads, 144 mute; typing is request type 4, label 3. Uploads go to
  `ajax/mercury/upload.php` with the web client's browser headers.

## FAQ

**How is this different from using Messenger in a browser?**
Messenger Desktop is a separate native app written in Rust: small, quick to
start, with its own window, Dock badge, notifications and Liquid Glass design,
and no browser tabs to keep open. It's an independent project, not an app
from Meta.

**Does it work with end-to-end encrypted chats?**
Yes, for new messages: it receives and sends text, sends photos, and handles
receipts in 🔒 chats. History from before the device was registered needs the
Messenger PIN, which isn't supported yet.

**Does it support voice or video calls?**
Not inside the app. The call buttons open the call in your browser.

**Which platforms are supported?**
macOS on Apple Silicon. The UI layer is cross-platform, but the glass effects,
menus, notifications and Keychain storage are macOS-specific.

**Is my password safe?**
The app never sees it. You sign in on Facebook's own page, and only session
cookies are kept, in the macOS Keychain.

## Known limitations

- Calls, stories, polls, voice recording and starting new chats aren't
  available.
- Encrypted chats: no group chats yet, and no sending of videos or files yet.
  Older encrypted history needs the Messenger PIN.
- Meta can change its private protocols at any time, which can break features
  until they're updated.

---

*Keywords: Messenger desktop app, Messenger for Mac, Facebook Messenger client,
native macOS Messenger app, Messenger desktop built with Rust, Rust egui desktop
app, Liquid Glass macOS app, end-to-end encrypted Messenger client.*
