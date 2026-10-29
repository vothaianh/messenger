# Vendored: wacore-binary 0.7.0

- Source: crates.io `wacore-binary` 0.7.0, from https://github.com/jlucaso1/whatsapp-rust
- License: MIT (as declared in the crate metadata). The original copyright
  belongs to the whatsapp-rust authors.
- Why it's vendored: upstream encodes a Messenger device JID
  (`<user>:<device>@msgr`) as `JID_PAIR`, which drops the device. Acks and
  receipts to the user's other Messenger devices then go to the wrong address
  and the server keeps redelivering.
- Patch (`src/encoder.rs`): `write_fb_jid` emits `FB_JID` (token 246: user,
  u16 device, `msgr`), the layout `read_fb_jid` already decodes. It's used by
  `write_jid_ref` and `write_jid_owned` for **every** `msgr` JID, including bare users
  (`device = 0`). The server resets the connection on a `JID_PAIR` msgr JID,
  e.g. in `fbid:devices` queries or as an ack's `recipient`.
- Wired up through `[patch.crates-io]` in the top-level `Cargo.toml`.
