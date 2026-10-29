# AGENTS.md

Notes for coding agents working on Messenger Desktop (native Rust macOS client).

## After every change to the app

Rebuild the bundle, then quit the running app and relaunch it yourself. Don't
ask the user to restart it.

```sh
./bundle.sh
pkill -f "Messenger Desktop.app/Contents/MacOS/Messenger Desktop"
open "target/Messenger Desktop.app"
```

- `bundle.sh` signs with the local "Apple Development" identity, so the code
  signature (and the Keychain "Always Allow" grant) stays the same across
  rebuilds. Don't switch back to ad-hoc signing (`-s -`): it changes every
  build and makes macOS ask for Keychain access on every launch. If a Keychain
  prompt does appear, only the user may answer it. Never click or dismiss it
  for them.
- The bare `target/release/messenger-rs` binary (used by `debug-*`) isn't
  signed this way, so it can still trigger Keychain prompts.
- To see connection status (`[status] Connecting → Syncing → Online`), launch
  the binary directly and keep stderr, e.g.
  `"target/Messenger Desktop.app/Contents/MacOS/Messenger Desktop" 2> app.log &`.
  Set `MESSENGER_DEBUG=1` to log the procedures in each `/ls_resp`.

## Useful commands

- `DIST=1 ./bundle.sh`: distribution build (profile `dist`: fat LTO, one
  codegen unit, stripped; ~3 min). Signs with the hardened runtime and a
  secure timestamp and writes `target/Messenger Desktop.zip`. On 2026-10-29:
  app 20 MB (vs 32 MB release), zip 10.7 MB. It's signed with the
  "Apple Development" certificate, so it runs on this Mac but Gatekeeper
  rejects it elsewhere. Sharing it needs a "Developer ID Application"
  certificate plus notarization (`xcrun notarytool`), which the user sets up
  in their Apple Developer account.

- `cargo test`: decoder, store and MQTT codec tests
- `./target/release/messenger-rs debug-sync`: bootstrap plus one GraphQL
  Lightspeed sync, prints thread, contact and message counts
- `./target/release/messenger-rs debug-mqtt`: connects MQTT, requests older
  messages for the newest thread (read-only), prints counts
- `./target/release/messenger-rs import-cookies`: save a `Cookie` header pasted
  on stdin (fallback when the login window can't finish)

## Boundaries

- Talk to Facebook only with the same requests, parameters and headers the web
  client sends (protocol reverse-engineering). Don't spoof TLS fingerprints, and
  don't automate CAPTCHAs, checkpoints, passkeys or password login.
- Never type the user's password anywhere. They log in themselves in the login
  window.
- Don't send messages, reactions, edits or unsends from the user's account to
  test. Ask the user to test write actions.
- Don't use Meta's Messenger logo or brand assets. The app icon is original.

## App icon

- `assets/AppIcon.icon` (Icon Composer format) is the real icon. The tile is
  blue in light mode. Dark mode deliberately has no `fill` specialization, so
  macOS draws its own near-black dark tile, matching Apple's apps (a custom
  charcoal fill looked grey next to them). The bolt layer comes from `assets/bolt-light.svg` and
  `assets/bolt-dark.svg`, which `bundle.sh` renders to PNG and compiles with
  `actool`.
- `assets/icon.svg` and `assets/icon-dark.svg` are full flat renderings, used
  for the window icon, the non-Xcode fallback, and previews.
- Inside the bundle, `main.rs` passes `IconData::default()` on purpose. If the
  viewport has no icon, eframe sets egui's own logo (a hexagon) as the Dock
  icon at runtime, which hides the bundle icon.

## Visual style: Liquid Glass

The app follows Apple's Liquid Glass design (macOS 26+). Keep new UI in this
style.

- **Real glass is native.** egui renders into one transparent layer
  (`with_transparent(true)`, `clear_color` = transparent). Behind it,
  `macos::sync_glass` places a behind-window blur (`NSVisualEffectView`,
  `UnderWindowBackground`) and one `NSGlassEffectView` per floating surface.
  Each frame the UI pushes the surface rects into `MessengerApp::glass` with
  `glass_rect(rect, radius)`. Don't fake glass with opaque egui fills.
- **Floating surfaces:** both side columns are glass panes inset
  `GLASS_INSET` (8 pt) from the window edge with `GLASS_RADIUS` (22 pt)
  corners. Their content gets the inset added to its margins. The composer
  input is a glass capsule (radius = half its height). New floating elements
  (toolbars, popovers that stay open) get a pane the same way.
- **Content area:** the conversation sits on a dark translucent tint (`BG`)
  over the blur, so text keeps its contrast. Message bubbles: own messages
  solid blue, others translucent white (`BUBBLE_OTHER`).
- **Colors on glass are translucent white, premultiplied** (white at alpha a =
  `from_rgba_premultiplied(a, a, a, a)`; in light, translucent black
  `(0, 0, 0, a)`): `input` for controls, `quote` for cards and hover,
  `selected` for the selected row, `border` for hairlines. Accent is `blue`.
  Popups and menus stay opaque (`popup`) for legibility.
- **Shapes:** capsule buttons and chips, continuous large radii (18–22 pt for
  surfaces, half-height for pills), no hard separator lines between panes
  (`show_separator_line(false)`); spacing does the separating.
- **Menus are native `NSMenu`s** (`macos::popup_menu`), which macOS draws as
  Liquid Glass. egui popups can't be glass: native glass sits behind egui, so
  the egui content under a popup would cover it. Commands get SF Symbol icons;
  reactions are a palette row (`MenuEntry::Palette`).
- **UI icons are drawn line icons** (painter strokes in `MUTED`, `BLUE` when
  active or hovered), not emoji: emoji render as colored glyphs and read as
  content (see `pill_search`'s magnifier, `smiley_button`). Emoji are only for
  emoji content, such as the 👍 quick-send and reactions.
- **Fields** are 36 pt capsules (`INPUT` fill, `BORDER` hairline, a blue ring
  while focused), lined up with the title's leading edge.
- **Light and dark appearances.** Colors come from `pal()`, which returns the
  `DARK` or `LIGHT` palette (`ui.rs`); never hardcode a color that only works
  on one of them (white is fine only on `blue`). The ⋯ menu picks Match System
  / Light / Dark (stored as `theme`); `apply_theme` swaps the palette and egui
  style and calls `macos::set_appearance`, which sets the window appearance
  and the glass tint (white 0.1 in light, navy `sRGB 0.04 0.08 0.17, alpha
  0.12` in dark; window tint `bg` 38% navy / 42% white over a fully opaque
  background blur; tuned with the user between "not glassy" and "too clear": the dark theme is dark blue). Keep tints light: the user
  wants the glass to look transparent, with the wallpaper showing through so text keeps its contrast on any wallpaper.
- **Glass panels over content** (the sticker panel) are their own egui
  viewport: a borderless transparent window positioned over the main one,
  given a glass pane by `macos::glass_window(title, radius)` and attached as
  a child window. Use this for any floating panel that must be Liquid Glass.
- **Sticker store** (in the sticker panel's "+"): `Session::sticker_store`
  (`CometStickersStoreDialogQuery`, then `...PackListPaginationQuery`) and
  `sticker_pack_tray` (`CometStickersStorePackMutation{Add,Remove}Mutation`,
  input `pack_id`). Adding a pack changes the user's account: only on their
  click.
- **Media viewing stays in the app:** images open in the egui overlay
  `image_viewer` (scrim, fitted picture, close/open-in-browser buttons),
  videos in `macos::play_video` (an `NSWindow` with AVKit's `AVPlayerView`,
  streaming the link; closing it stops playback via `reap_video`).
- Can't screenshot the app from the agent (no Screen Recording permission):
  ask the user for a screenshot to check visual changes.

## Feature parity: regular and encrypted chats

Every feature must work in both kinds of chat, kept in sync:

- **Regular chats** go through Lightspeed: GraphQL/HTTP tasks and the DGW
  sync streams (`client.rs`, `store.rs`).
- **Encrypted (🔒, E2EE) chats** go through Armadillo on the E2EE socket
  (`e2ee_listen.rs`, `armadillo.rs`, `e2ee_media.rs`, `store::apply_e2ee`).

When you add or change a feature (sending a kind of message, stickers,
media, reactions, replies, edits, unsend, read/delivery receipts, typing,
notifications, snippets, live updates), implement it for both paths in the
same change, or say clearly in your report which side is missing and why.
Don't hide a control in one kind of chat without telling the user (the
sticker button was hidden in E2EE chats and the user had to ask).
Current gaps in E2EE chats: replies, reactions, edits, unsend and typing
aren't sent yet; old history needs the PIN restore (in progress, see
`docs/E2EE.md`).

## Research notes

- `docs/E2EE.md` covers how Messenger E2EE (Armadillo) works: endpoints, ICDC
  registration, reusable crates, licensing, and the phased plan. Read it before
  touching E2EE.
- `src/dgw.rs` documents the Lightspeed transport. The web client moved from
  MQTT (`edge-chat`) to Meta's DGW gateway
  (`wss://gateway.facebook.com/ws/lightspeed`); MQTT still answers requests but
  never pushes. DGW auth = session cookie + `x-dgw-uuid` set to the user's fbid.
  DGW is for receiving only: every task (send, react, read, history 228, ...)
  goes over HTTP as a GraphQL request type 3 (`Conn::send_tasks_http`,
  `Conn::load_older`). A task sent on the DGW socket is acked, then the server
  drops the whole connection. Older history: task 228, then a delta sync of
  database 1 from the cursor with request type 2 (type 1 ignores the cursor and
  returns a snapshot). The first database 1 sync on DGW sometimes answers
  `handleSyncFailure`; the client then resyncs that database from a snapshot.
