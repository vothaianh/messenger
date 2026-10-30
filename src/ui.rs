//! Native egui interface: chat list, conversation view, composer, info panel.
//! Layout and metrics follow Messenger web's dark theme.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use chrono::{Local, TimeZone};
use eframe::egui::{
    self, Align, Color32, CornerRadius, Frame, Key, Layout, Margin, Pos2, Rect, RichText, ScrollArea, Sense, Stroke,
    TextEdit, Vec2,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::client::{Command, Shared, Status};
use crate::store::{now_ms, Attachment, AttachmentKind, Message, Store, Thread};

// Liquid Glass palettes (see AGENTS.md, "Visual style"), one per appearance.
// Translucent colors are premultiplied: white at alpha a is (a, a, a, a),
// black at alpha a is (0, 0, 0, a).
struct Palette {
    blue: Color32,
    /// Conversation area: a tint over the window's background blur.
    bg: Color32,
    /// Controls on glass (buttons, search, menu pills).
    input: Color32,
    bubble_other: Color32,
    /// Quotes, cards and hover fills.
    quote: Color32,
    /// Popups and menus stay opaque so they read over anything.
    popup: Color32,
    text: Color32,
    muted: Color32,
    /// Hairline on glass edges and separators.
    border: Color32,
    selected: Color32,
    chip_on_bg: Color32,
    chip_on_text: Color32,
}

/// Dark blue: a deep navy tint over the blur; fills are translucent
/// periwinkle (180, 200, 255) instead of white, so they read as blue glass.
const DARK: Palette = Palette {
    blue: Color32::from_rgb(59, 139, 255),
    bg: Color32::from_rgba_premultiplied(4, 8, 14, 97),
    input: Color32::from_rgba_premultiplied(18, 20, 26, 26),
    bubble_other: Color32::from_rgba_premultiplied(32, 36, 46, 46),
    quote: Color32::from_rgba_premultiplied(16, 18, 22, 22),
    popup: Color32::from_rgb(22, 34, 58),
    text: Color32::from_rgb(230, 237, 247),
    muted: Color32::from_rgb(147, 164, 191),
    border: Color32::from_rgba_premultiplied(23, 26, 33, 33),
    selected: Color32::from_rgba_premultiplied(20, 38, 72, 77),
    chip_on_bg: Color32::from_rgba_premultiplied(15, 35, 64, 64),
    chip_on_text: Color32::from_rgb(110, 170, 255),
};

/// Light: a near-white tint over the blur, ink-dark text, and fills made of
/// translucent black so the glass still shows through.
const LIGHT: Palette = Palette {
    blue: Color32::from_rgb(0, 122, 255),
    bg: Color32::from_rgba_premultiplied(105, 105, 106, 107),
    input: Color32::from_rgba_premultiplied(0, 0, 0, 14),
    bubble_other: Color32::from_rgba_premultiplied(0, 0, 0, 20),
    quote: Color32::from_rgba_premultiplied(0, 0, 0, 10),
    popup: Color32::from_rgb(255, 255, 255),
    text: Color32::from_rgb(29, 29, 31),
    muted: Color32::from_rgb(108, 108, 114),
    border: Color32::from_rgba_premultiplied(0, 0, 0, 24),
    selected: Color32::from_rgba_premultiplied(0, 0, 0, 20),
    chip_on_bg: Color32::from_rgba_premultiplied(0, 15, 31, 31),
    chip_on_text: Color32::from_rgb(0, 102, 214),
};

static LIGHT_THEME: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The palette of the current appearance.
fn pal() -> &'static Palette {
    if LIGHT_THEME.load(Ordering::Relaxed) { &LIGHT } else { &DARK }
}

/// Floating glass surfaces: inset from the window edge, large continuous corners.
const GLASS_INSET: f32 = 8.0;
const GLASS_RADIUS: f32 = 22.0;
/// Height of the transparent macOS titlebar the content extends under.
const TITLEBAR: f32 = 30.0;
const AVATAR_SMALL: f32 = 28.0;
/// Load older history automatically (scrolled to the top or a short list).
const HISTORY_FETCH: bool = true;
/// The web client's default reaction bar (❤ is sent without U+FE0F, like the
/// web client; menus add it for display). "+" opens the full emoji grid.
const QUICK_REACTIONS: [&str; 6] = ["❤", "😆", "😮", "😢", "😡", "👍"];
const EMOJIS: [&str; 24] = [
    "😀", "😂", "😍", "😘", "😎", "🤔", "😭", "😡", "👍", "👎", "🙏", "👏", "🎉", "🔥", "❤", "💯", "😅", "🥰",
    "😴", "🤣", "😇", "🙌", "💪", "✨",
];

/// User text-size multiplier (Cmd +/-/0), stored as f32 bits so the free
/// `sf()` helper can read it.
static TEXT_SCALE: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0
const TEXT_SCALE_STEPS: [f32; 9] = [0.8, 0.9, 1.0, 1.1, 1.2, 1.3, 1.4, 1.5, 1.6];

fn text_scale() -> f32 {
    f32::from_bits(TEXT_SCALE.load(Ordering::Relaxed))
}

/// Apply a text scale: remember it and resize egui's built-in text styles.
fn set_text_scale(ctx: &egui::Context, scale: f32) {
    TEXT_SCALE.store(scale.to_bits(), Ordering::Relaxed);
    use egui::{FontFamily::Proportional, FontId, TextStyle};
    let mut style = (*ctx.global_style()).clone();
    style.text_styles = [
        (TextStyle::Small, FontId::new(13.0 * scale, Proportional)),
        (TextStyle::Body, FontId::new(15.0 * scale, Proportional)),
        (TextStyle::Button, FontId::new(15.0 * scale, Proportional)),
        (TextStyle::Heading, FontId::new(22.0 * scale, Proportional)),
        (TextStyle::Monospace, FontId::new(14.0 * scale, egui::FontFamily::Monospace)),
    ]
    .into();
    ctx.set_global_style(style);
}

#[derive(PartialEq, Clone, Copy)]
enum Filter {
    All,
    Unread,
    Groups,
}

pub struct MessengerApp {
    shared: Arc<Shared>,
    tx: UnboundedSender<Command>,
    selected: Option<i64>,
    search: String,
    filter: Filter,
    draft: String,
    reply_to: Option<(String, String)>,
    editing: Option<String>,
    show_info: bool,
    show_emoji: bool,
    typing_sent: Option<(i64, Instant)>,
    read_sent: HashSet<(i64, i64)>,
    /// Last older-history request: thread, oldest message id at the time, when.
    older_requested: Option<(i64, Option<String>, Instant)>,
    /// Native glass surfaces for this frame (see `macos::sync_glass`).
    glass: Vec<crate::macos::GlassRect>,
    /// Composer text height last frame (one line, or more for a multi-line draft).
    composer_text_h: f32,
    /// Members tagged in the current draft (id, inserted name).
    mention_picks: Vec<(i64, String)>,
    /// Highlighted row in the @mention picker.
    mention_sel: usize,
    /// The open "+" reaction picker, if any.
    reaction_picker: Option<ReactionPicker>,
    /// Focus the composer on the next frame (after Reply/Edit).
    focus_composer: bool,
    /// Chats whose newest messages were re-fetched this session.
    refreshed: HashSet<i64>,
    show_stickers: bool,
    /// Appearance chosen in the ⋯ menu, and the one last applied (light?).
    theme: ThemePref,
    applied_theme: Option<(ThemePref, bool)>,
    /// Pasted files waiting in the composer.
    pending_files: Vec<std::path::PathBuf>,
    /// Selected pack in the sticker picker.
    sticker_pack: Option<String>,
    /// The sticker store is open in the sticker panel, and the store pack
    /// being previewed (id, name).
    sticker_store: bool,
    /// The "restore encrypted history with your PIN" dialog, when open.
    pin_restore: Option<PinRestore>,
    /// The PIN restore succeeded once (kept): encrypted chats then fetch
    /// their restored history from the hidden web engine.
    history_restored: bool,
    /// Expired session: the old `xs` cookie (a login only counts once it
    /// changes) and when the login page's cookies were last checked.
    login_stale_xs: Option<Option<String>>,
    login_poll: f64,
    /// Encrypted chats whose restored history was fetched this run, and the
    /// one being fetched.
    history_fetched: HashSet<i64>,
    history_job: Option<HistoryJob>,
    /// Whether the hidden page already holds a restored backup (checked
    /// once, when an encrypted chat is first opened and the flag isn't set):
    /// None = not checked, Some(None) = waiting, Some(Some(id)) = running.
    restore_check: Option<Option<u64>>,
    store_preview: Option<(String, String)>,
    last_layout_log: String,
    /// Message list as of the previous frame, to keep the view in place when
    /// older messages are inserted above it.
    prev_view: Option<ListView>,
    /// Scroll offset to apply on the next pass (after older messages arrived).
    scroll_fix: Option<(i64, f32)>,
    focused: bool,
}

struct ListView {
    key: i64,
    /// When this thread's list was first shown (its first frames start at the
    /// top before stick_to_bottom takes over, so offsets aren't trusted yet).
    opened: Instant,
    first_id: Option<String>,
    content_h: f32,
    offset: f32,
    at_bottom: bool,
}

impl MessengerApp {
    pub fn new(cc: &eframe::CreationContext<'_>, shared: Arc<Shared>, tx: UnboundedSender<Command>) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);
        // Clean up restored messages saved before encrypted text references
        // were filtered out.
        shared.store.lock().unwrap().drop_secure_tokens();
        crate::macos::init_notifications();
        install_fonts(&cc.egui_ctx);
        apply_style(&cc.egui_ctx, false);
        // We handle Cmd +/-/0 ourselves to scale text only, not the whole UI.
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let stored = |key: &str| cc.storage.and_then(|s| s.get_string(key));
        let theme = match stored("theme").as_deref() {
            Some("light") => ThemePref::Light,
            Some("dark") => ThemePref::Dark,
            _ => ThemePref::System,
        };
        let scale = stored("text_scale").and_then(|v| v.parse::<f32>().ok()).filter(|s| (0.5..=2.0).contains(s)).unwrap_or(1.0);
        set_text_scale(&cc.egui_ctx, scale);
        MessengerApp {
            shared,
            tx,
            selected: stored("selected_thread").and_then(|v| v.parse().ok()),
            search: String::new(),
            filter: Filter::All,
            draft: String::new(),
            reply_to: None,
            editing: None,
            show_info: stored("show_info").is_none_or(|v| v == "true"),
            show_emoji: false,
            typing_sent: None,
            read_sent: HashSet::new(),
            older_requested: None,
            glass: Vec::new(),
            composer_text_h: 0.0,
            mention_picks: Vec::new(),
            mention_sel: 0,
            reaction_picker: None,
            focus_composer: false,
            refreshed: HashSet::new(),
            show_stickers: false,
            pending_files: Vec::new(),
            theme,
            applied_theme: None,
            sticker_pack: None,
            sticker_store: false,
            pin_restore: None,
            history_restored: stored("history_restored").is_some_and(|v| v == "true"),
            login_stale_xs: None,
            login_poll: 0.0,
            history_fetched: HashSet::new(),
            history_job: None,
            restore_check: None,
            store_preview: None,
            last_layout_log: String::new(),
            prev_view: None,
            scroll_fix: None,
            focused: true,
        }
    }

    /// Fetch one encrypted chat's restored history: the hidden page opens the
    /// chat (the web client restores a chat's history when it's opened), then
    /// its messages are read from the web's database every few seconds for
    /// two minutes and merged into ours.
    fn history_tick(&mut self, ctx: &egui::Context) {
        // One read-only check: does the hidden page hold a restored backup?
        match self.restore_check {
            Some(None) => {
                crate::macos::web_open("https://www.facebook.com/messages/", crate::session::USER_AGENT);
                if crate::macos::web_loaded() {
                    if let Some(id) = crate::macos::web_call(BACKUP_PRESENT_JS) {
                        self.restore_check = Some(Some(id));
                    }
                }
                ctx.request_repaint_after(std::time::Duration::from_millis(500));
            }
            Some(Some(id)) => {
                if let Some(r) = crate::macos::web_result(id) {
                    if r.as_deref() == Ok("yes") {
                        self.history_restored = true;
                        self.history_fetched.clear();
                    }
                    // Checked; don't ask again this run.
                    self.restore_check = Some(Some(u64::MAX));
                } else if id != u64::MAX {
                    ctx.request_repaint_after(std::time::Duration::from_millis(500));
                }
            }
            None => {}
        }
        let Some(mut job) = self.history_job.take() else { return };
        let key = job.thread.to_string();
        let mut done = false;
        match job.stage {
            HistoryStage::Start => {
                crate::macos::web_open("https://www.facebook.com/messages/", crate::session::USER_AGENT);
                if crate::macos::web_loaded() {
                    let _ = crate::macos::web_call_with(OPEN_THREAD_JS, &[("threadKey", &key)]);
                    job.stage = HistoryStage::Opening(Instant::now());
                }
            }
            HistoryStage::Opening(since) => {
                if crate::macos::web_loaded() && since.elapsed() > std::time::Duration::from_secs(8) {
                    job.stage = HistoryStage::Idle(Instant::now() - std::time::Duration::from_secs(10));
                }
            }
            HistoryStage::Idle(last) => {
                if last.elapsed() > std::time::Duration::from_secs(5) {
                    match crate::macos::web_call_with(READ_THREAD_JS, &[("threadKey", &key)]) {
                        Some(id) => job.stage = HistoryStage::Reading(id),
                        None => done = true,
                    }
                }
            }
            HistoryStage::Reading(id) => {
                if let Some(result) = crate::macos::web_result(id) {
                    if let Ok(text) = result {
                        if std::env::var_os("MESSENGER_DEBUG").is_some() {
                            // Counts only.
                            let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                            let atts = v["attachments"].as_array().map_or(0, Vec::len);
                            let thumbs = v["attachments"].as_array().map_or(0, |l| l.iter().filter(|a| a["thumb"].as_str().is_some_and(|t| !t.is_empty())).count());
                            let media = v["attachments"].as_array().map_or(0, |l| l.iter().filter(|a| a["media"].as_bool() == Some(true)).count());
                            let span = v["messages"].as_array().map(|l| l.iter().filter_map(|m| m["ts"].as_str()?.parse::<i64>().ok()).fold((i64::MAX, 0), |(lo, hi), t| (lo.min(t), hi.max(t))));
                            eprintln!("[history] read: {} messages {span:?}, {atts} attachments, {media} encrypted media, {thumbs} pictures", v["messages"].as_array().map_or(0, Vec::len));
                        }
                        let restored = parse_restored(&text);
                        let added = self.shared.store.lock().unwrap().add_restored(job.thread, restored);
                        job.found += added;
                        if std::env::var_os("MESSENGER_DEBUG").is_some() && added > 0 {
                            eprintln!("[history] {} new restored messages", added);
                        }
                    }
                    job.stage = HistoryStage::Idle(Instant::now());
                }
            }
        }
        if job.started.elapsed() > std::time::Duration::from_secs(120) {
            done = true;
        }
        if !done {
            self.history_job = Some(job);
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
    }

    fn open_pin_restore(&mut self, cmds: &mut Vec<Command>) {
        if self.pin_restore.is_none() {
            self.pin_restore = Some(PinRestore::default());
        }
        cmds.push(Command::LoadPinInfo);
    }

    /// The PIN dialog and the restore it drives: the web client's own restore
    /// code, run in the hidden web engine (see `macos::web_open` and
    /// `RESTORE_JS`). One PIN attempt per click; nothing is retried.
    fn pin_restore_ui(&mut self, ctx: &egui::Context) {
        let Some(mut st) = self.pin_restore.take() else { return };
        let info = self.shared.pin.lock().unwrap().clone();

        // Drive the restore.
        match st.stage {
            PinStage::OpeningWeb(since) => {
                if crate::macos::web_loaded() && since.elapsed() > std::time::Duration::from_secs(6) {
                    let pin = std::mem::take(&mut st.pin);
                    match info.client_id.as_deref().and_then(|id| crate::macos::web_call_with(RESTORE_JS, &[("pin", &pin), ("clientId", id)])) {
                        Some(id) => st.stage = PinStage::Running(id),
                        None => st.stage = PinStage::Failed("Couldn't start the restore".into()),
                    }
                } else if since.elapsed() > std::time::Duration::from_secs(60) {
                    st.stage = PinStage::Failed("Messenger didn't load in time. Try again.".into());
                }
                ctx.request_repaint_after(std::time::Duration::from_millis(300));
            }
            PinStage::Running(id) => {
                if let Some(result) = crate::macos::web_result(id) {
                    st.stage = match result.as_deref().map(serde_json::from_str::<serde_json::Value>) {
                        Ok(Ok(v)) if v["ok"].as_bool() == Some(true) => {
                            self.history_restored = true;
                            self.history_fetched.clear();
                            PinStage::Done
                        }
                        Ok(Ok(v)) if v["wrongPin"].as_bool() == Some(true) => PinStage::WrongPin,
                        Ok(Ok(v)) => PinStage::Failed(v["error"].as_str().unwrap_or("The restore failed").to_string()),
                        Ok(Err(_)) => PinStage::Failed("Unexpected answer from Messenger".into()),
                        Err(e) => PinStage::Failed(e.clone()),
                    };
                    if matches!(st.stage, PinStage::WrongPin) {
                        self.send(Command::LoadPinInfo);
                    }
                } else {
                    ctx.request_repaint_after(std::time::Duration::from_millis(300));
                }
            }
            _ => {}
        }

        let busy = matches!(st.stage, PinStage::OpeningWeb(_) | PinStage::Running(_));
        let mut close = !busy && ctx.input(|i| i.key_pressed(Key::Escape));
        let screen = ctx.content_rect();
        // A dimming scrim over the app; clicking it closes the dialog.
        egui::Area::new(egui::Id::new("pin_restore_scrim")).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
            let scrim = ui.allocate_rect(screen, Sense::click());
            ui.painter().rect_filled(screen, CornerRadius::ZERO, Color32::from_black_alpha(110));
            if scrim.clicked() && !busy {
                close = true;
            }
        });
        // The card is a Liquid Glass window of its own (like the sticker
        // panel): glass behind egui can't show through the app's content.
        let size = Vec2::new(380.0, 310.0);
        let origin = ctx.input(|i| i.viewport().inner_rect.map(|r| r.min)).unwrap_or_default();
        let builder = egui::ViewportBuilder::default()
            .with_title(crate::macos::PIN_WINDOW)
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(false)
            .with_taskbar(false)
            .with_position(origin + (screen.center() - size / 2.0).to_vec2())
            .with_inner_size(size);
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("pin_restore"), builder, |pctx, _| {
            crate::macos::glass_window(crate::macos::PIN_WINDOW, 24.0);
            if !busy && pctx.input(|i| i.key_pressed(Key::Escape)) {
                close = true;
            }
            let frame = Frame::NONE.stroke(Stroke::new(1.0, pal().border)).corner_radius(CornerRadius::same(24)).inner_margin(Margin::same(22));
            egui::CentralPanel::default().frame(frame).show(pctx, |ui| {
                ui.vertical_centered(|ui| {
                    // A drawn icon (SF Symbol), not the 🔒 emoji.
                    if let Some(png) = crate::macos::sf_symbol_png("lock.fill", 26.0, [pal().blue.r(), pal().blue.g(), pal().blue.b()]) {
                        let dim = |i: usize| png.get(i..i + 4).map_or(1.0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f32);
                        let (w, h) = (dim(16), dim(20));
                        let scale = 30.0 / w.max(h);
                        ui.add(egui::Image::from_bytes("bytes://sym/pin-lock.png", png).fit_to_exact_size(Vec2::new(w * scale, h * scale)));
                    }
                    ui.add_space(8.0);
                    ui.label(sf("Restore encrypted history", 17.0, 700));
                    ui.add_space(6.0);
                    let note = match (&info.registered, info.loading) {
                        (Some(false), _) => "This account has no Messenger PIN backup.".to_string(),
                        _ => "Enter the 6-digit PIN you use for Messenger's secure storage.".to_string(),
                    };
                    ui.add(egui::Label::new(sf(&note, 13.0, 400).color(pal().muted)).wrap());
                    ui.add_space(12.0);
                    // A capsule field (INPUT fill, hairline, blue ring when focused).
                    let field_rect = ui.allocate_exact_size(Vec2::new(200.0, 44.0), Sense::hover()).0;
                    let field_id = egui::Id::new("pin_field");
                    let focused = ui.memory(|m| m.has_focus(field_id));
                    ui.painter().rect_filled(field_rect, CornerRadius::same(22), pal().input);
                    let ring = if focused { Stroke::new(2.0, pal().blue) } else { Stroke::new(1.0, pal().border) };
                    ui.painter().rect_stroke(field_rect, CornerRadius::same(22), ring, egui::StrokeKind::Inside);
                    let field = TextEdit::singleline(&mut st.pin)
                        .id(field_id)
                        .password(true)
                        .char_limit(6)
                        .frame(Frame::NONE)
                        .horizontal_align(Align::Center)
                        .font(egui::FontId::proportional(22.0))
                        .hint_text("••••••");
                    let resp = ui.put(field_rect.shrink2(Vec2::new(16.0, 8.0)), |ui: &mut egui::Ui| ui.add_enabled(!busy, field));
                    st.pin.retain(|c| c.is_ascii_digit());
                    if !st.focused {
                        // Make the dialog's window key so the PIN can be typed.
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Focus);
                        resp.request_focus();
                        st.focused = true;
                    }
                    ui.add_space(8.0);
                    let status = match &st.stage {
                        PinStage::Idle => info.attempts_remaining.map(|n| format!("{n} attempts left")).unwrap_or_default(),
                        PinStage::OpeningWeb(_) => "Connecting to secure storage…".into(),
                        PinStage::Running(_) => "Checking your PIN…".into(),
                        PinStage::WrongPin => match info.attempts_remaining {
                            Some(n) => format!("Incorrect PIN. {n} attempts left."),
                            None => "Incorrect PIN.".into(),
                        },
                        PinStage::Failed(e) => format!("Couldn't restore: {}", truncate(e, 120)),
                        PinStage::Done => "PIN accepted. Your history is being restored in the background; messages will appear as they're decrypted.".into(),
                    };
                    let color = match st.stage {
                        PinStage::WrongPin | PinStage::Failed(_) => Color32::from_rgb(240, 90, 90),
                        _ => pal().muted,
                    };
                    ui.add(egui::Label::new(sf(&status, 12.0, 500).color(color)).wrap());
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        let w = (ui.available_width() - 8.0) / 2.0;
                        let done = matches!(st.stage, PinStage::Done);
                        if ui.add_enabled(!busy, egui::Button::new(sf(if done { "Close" } else { "Cancel" }, 14.0, 600)).fill(pal().input).corner_radius(CornerRadius::same(16)).min_size(Vec2::new(w, 34.0))).clicked() {
                            close = true;
                        }
                        let ready = st.pin.len() == 6 && !busy && !done && info.client_id.is_some() && info.registered == Some(true);
                        let label = if busy { "Restoring…" } else { "Restore" };
                        let submit = ui.add_enabled(ready, egui::Button::new(sf(label, 14.0, 600).color(Color32::WHITE)).fill(pal().blue).corner_radius(CornerRadius::same(16)).min_size(Vec2::new(w, 34.0)));
                        if submit.clicked() || (ready && resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter))) {
                            crate::macos::web_open("https://www.facebook.com/messages/", crate::session::USER_AGENT);
                            st.stage = PinStage::OpeningWeb(Instant::now());
                        }
                    });
                    if let Some(e) = &info.error {
                        ui.add_space(6.0);
                        ui.add(egui::Label::new(sf(&truncate(e, 120), 11.0, 400).color(pal().muted)).wrap());
                    }
                });
            });
        });
        if close {
            // The hidden page keeps running after a successful PIN: the
            // restore continues in its worker.
            st.pin.clear();
            return;
        }
        self.pin_restore = Some(st);
    }

    /// Follow the chosen appearance (or the system's): palette, egui style,
    /// and the native window appearance the glass is drawn with.
    fn apply_theme(&mut self, ctx: &egui::Context) {
        let light = match self.theme {
            ThemePref::Light => true,
            ThemePref::Dark => false,
            // Asked from macOS each frame: egui's `system_theme` can miss a
            // switch, leaving the palette and the native glass out of step.
            ThemePref::System => crate::macos::system_is_light(),
        };
        if self.applied_theme == Some((self.theme, light)) {
            return;
        }
        self.applied_theme = Some((self.theme, light));
        LIGHT_THEME.store(light, Ordering::Relaxed);
        apply_style(ctx, light);
        // Always force the resolved appearance, so the native glass can't
        // differ from the egui palette.
        let [r, g, b, a] = pal().bg.to_srgba_unmultiplied();
        crate::macos::set_appearance(Some(light), light, [r, g, b, a].map(|c| c as f64 / 255.0));
        ctx.request_repaint();
    }

    fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }
}

impl eframe::App for MessengerApp {
    /// Transparent, so the native glass and background blur show through.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if let Some(t) = self.selected {
            storage.set_string("selected_thread", t.to_string());
        }
        storage.set_string("show_info", self.show_info.to_string());
        storage.set_string("text_scale", text_scale().to_string());
        storage.set_string("theme", self.theme.key().into());
        storage.set_string("history_restored", self.history_restored.to_string());
        // Also keep the inbox for the next launch (eframe calls this every
        // ~30 s and on exit); the file is written off the UI thread.
        if let Some(bytes) = self.shared.store.lock().unwrap().cache_bytes() {
            std::thread::spawn(move || crate::store::write_cache(&bytes));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.glass.clear();
        self.focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        handle_text_size_keys(&ctx);
        self.apply_theme(&ctx);
        crate::macos::reap_video();
        self.pin_restore_ui(&ctx);
        self.history_tick(&ctx);
        web_probe(&ctx);
        image_viewer(&ctx);
        let status = self.shared.status.lock().unwrap().clone();
        if let Status::LoggedOut(_) = &status {
            self.login_screen(ui);
            crate::macos::sync_glass(&self.glass);
            return;
        }
        let shared = self.shared.clone();
        let mut store = shared.store.lock().unwrap();
        let mut cmds = Vec::new();

        self.notify(&mut store);
        self.update_dock_badge(&store);

        // Symmetric layout: both side columns share one width derived from the
        // window, so the conversation stays the wide, centered focus.
        let side = (ui.available_width() * 0.22).clamp(280.0, 380.0);

        // Layout by fixed rects: left column, optional info column, conversation
        // in between. (egui's side panels put the info panel partly off-window
        // with the conversation drawn over it.) The side columns float as glass
        // panes (native, behind egui); their content sits inside the inset.
        let full = ui.available_rect_before_wrap();
        let show_info = self.show_info && self.selected.is_some_and(|k| store.threads.contains_key(&k));
        let left_rect = Rect::from_min_max(full.min, Pos2::new(full.left() + side, full.bottom()));
        let info_rect = show_info.then(|| Rect::from_min_max(Pos2::new(full.right() - side, full.top()), full.max));
        let center_rect = Rect::from_min_max(Pos2::new(left_rect.right(), full.top()), Pos2::new(info_rect.map_or(full.right(), |r| r.left()), full.bottom()));
        ui.allocate_rect(full, Sense::hover());

        let column = |ui: &mut egui::Ui, rect: Rect, margin: Margin| {
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect - margin).layout(Layout::top_down(Align::Min)));
            child.set_clip_rect(rect);
            child
        };
        let inset = GLASS_INSET as i8;
        let mut left = column(ui, left_rect, Margin { left: 8 + inset, right: 8 + inset, top: inset, bottom: 8 + inset });
        self.thread_list(&mut left, &store, &status, &mut cmds);
        self.glass.push(glass_rect(left_rect.shrink(GLASS_INSET), GLASS_RADIUS));

        if let (Some(rect), Some(key)) = (info_rect, self.selected) {
            let mut info = column(ui, rect, Margin { left: 16 + inset, right: 16 + inset, top: inset, bottom: 12 + inset });
            self.info_panel(&mut info, &store, key, &mut cmds);
            self.glass.push(glass_rect(rect.shrink(GLASS_INSET), GLASS_RADIUS));
        }

        let mut center = ui.new_child(egui::UiBuilder::new().max_rect(center_rect));
        center.set_clip_rect(center_rect);
        egui::CentralPanel::default()
            // The content tint is native (`macos::set_appearance`), under the
            // glass panes too, so no lighter blur shows around them.
            .frame(Frame::NONE)
            .show(&mut center, |ui| match self.selected.and_then(|k| store.threads.get(&k).cloned()) {
                Some(thread) => self.conversation(ui, &store, &thread, &mut cmds),
                None => {
                    titlebar_drag(ui, ui.max_rect().with_max_y(ui.max_rect().top() + TITLEBAR));
                    ui.centered_and_justified(|ui| {
                        ui.label(sf(
                            if store.threads.is_empty() && matches!(status, Status::Syncing | Status::Connecting) {
                                "Loading your chats…"
                            } else {
                                "Select a chat to start messaging"
                            },
                            18.0,
                            500,
                        )
                        .color(pal().muted));
                    });
                }
            });

        self.reaction_picker_ui(&ctx, &store, &mut cmds);

        // Debug: column geometry, logged when it changes (layout issues).
        if std::env::var_os("MESSENGER_DEBUG").is_some() {
            let shape = format!("[layout] window {:?} left {left_rect:?} center {center_rect:?} info {info_rect:?}", ctx.content_rect());
            if self.last_layout_log != shape {
                eprintln!("{shape}");
                self.last_layout_log = shape;
            }
        }
        drop(store);
        for c in cmds {
            self.send(c);
        }
        crate::macos::sync_glass(&self.glass);
    }
}

/// Whether our newest message for `t` is its latest activity (not, say, a
/// reaction or a rename after it), so the snippet can be built from it with
/// the sender's name.
fn newest_is_latest(store: &Store, t: &Thread) -> bool {
    store
        .messages
        .get(&t.key)
        .and_then(|v| v.iter().rev().find(|m| !m.unsent && !m.pending))
        .is_some_and(|m| m.timestamp_ms + 1000 >= t.last_activity_ms)
}

/// "You: …" / "Name: …" (groups) / "…" for the newest message we have: used
/// for encrypted chats (decrypted on this Mac), and for any chat whose server
/// snippet is empty (a live photo, video or sticker carries no text).
fn e2ee_snippet(store: &Store, t: &Thread) -> String {
    let Some(m) = store.messages.get(&t.key).and_then(|v| v.iter().rev().find(|m| !m.unsent)) else {
        return t.snippet.replace('\n', " ");
    };
    let text = match m.attachments.first() {
        _ if !m.text.is_empty() => m.text.replace('\n', " "),
        Some(a) if a.kind == AttachmentKind::Image => "📷 Photo".to_string(),
        Some(a) if a.kind == AttachmentKind::Video => "🎬 Video".to_string(),
        Some(a) if a.kind == AttachmentKind::Audio => "🎤 Voice message".to_string(),
        Some(a) if a.kind == AttachmentKind::Sticker => "Sticker".to_string(),
        Some(a) if a.kind == AttachmentKind::Link => if a.title.is_empty() { "🔗 Link".to_string() } else { format!("🔗 {}", a.title.replace('\n', " ")) },
        Some(a) if a.kind == AttachmentKind::File => format!("📎 {}", if a.title.is_empty() { "File" } else { &a.title }),
        _ => "Sent an attachment".to_string(),
    };
    if m.sender_id == store.me {
        format!("You: {text}")
    } else if t.is_group() {
        // First name only, like Messenger ("Truc: …").
        let name = store.member_name(t.key, m.sender_id);
        format!("{}: {text}", name.split_whitespace().next().unwrap_or(&name))
    } else {
        text
    }
}

/// The "@query" being typed at the end of the draft: the '@' byte index and
/// the query, if the '@' starts a word and the query is short and on one line.
fn mention_query(draft: &str) -> Option<(usize, &str)> {
    let at = draft.rfind('@')?;
    let before_ok = draft[..at].chars().next_back().is_none_or(char::is_whitespace);
    let query = &draft[at + 1..];
    (before_ok && !query.contains('\n') && query.chars().count() <= 30).then_some((at, query))
}

/// Lowercase without Vietnamese diacritics, so "@truc" finds "Trực".
fn fold_name(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.nfd()
        .filter(|c| !('\u{300}'..='\u{36f}').contains(c))
        .map(|c| if c == 'đ' || c == 'Đ' { 'd' } else { c })
        .collect::<String>()
        .to_lowercase()
}

/// Where each tagged name sits in the final text (in order, UTF-16 units).
/// Names no longer in the text (edited away) are dropped.
fn locate_mentions(text: &str, picks: &[(i64, String)]) -> Vec<crate::client::Mention> {
    let mut out = Vec::new();
    let mut from = 0;
    for (id, name) in picks {
        let Some(found) = text[from..].find(name.as_str()) else { continue };
        let start = from + found;
        out.push(crate::client::Mention {
            id: *id,
            offset: text[..start].encode_utf16().count(),
            length: name.encode_utf16().count(),
        });
        from = start + name.len();
    }
    out
}

struct ReactionPicker {
    thread: i64,
    message_id: String,
    /// Where it was opened (the pointer), in points.
    anchor: Pos2,
    query: String,
    /// Category to scroll to (set by the bottom bar).
    jump_to: Option<usize>,
    /// First frame: focus the search field.
    opened: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum ThemePref {
    System,
    Light,
    Dark,
}

impl ThemePref {
    fn key(self) -> &'static str {
        match self {
            ThemePref::System => "system",
            ThemePref::Light => "light",
            ThemePref::Dark => "dark",
        }
    }

    fn label(self) -> &'static str {
        match self {
            ThemePref::System => "Match System",
            ThemePref::Light => "Light",
            ThemePref::Dark => "Dark",
        }
    }
}

/// egui's style for the current palette.
fn apply_style(ctx: &egui::Context, light: bool) {
    let mut style = (*ctx.global_style()).clone();
    style.visuals = if light { egui::Visuals::light() } else { egui::Visuals::dark() };
    style.visuals.window_fill = pal().popup;
    style.visuals.panel_fill = Color32::TRANSPARENT;
    style.visuals.extreme_bg_color = pal().input;
    style.visuals.selection.bg_fill = pal().blue;
    style.visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    style.visuals.override_text_color = Some(pal().text);
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, pal().border);
    style.visuals.hyperlink_color = pal().blue;
    style.spacing.item_spacing = Vec2::new(8.0, 5.0);
    style.spacing.button_padding = Vec2::new(10.0, 5.0);
    ctx.set_global_style(style);
}

/// An image from a link that plays if it's animated (stickers are animated
/// WebP). egui only animates URIs ending in .webp/.gif, and Facebook's signed
/// links end in a query string, so animated files are handed over as bytes
/// (egui detects those by their header) once the HTTP loader has them.
fn media_image<'a>(ctx: &egui::Context, url: &str) -> egui::Image<'a> {
    let path = url.split('?').next().unwrap_or(url);
    let animated = path.ends_with(".webp") || path.ends_with(".gif");
    if animated && url.contains('?') {
        if let Ok(egui::load::BytesPoll::Ready { bytes, .. }) = ctx.try_load_bytes(url) {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            path.hash(&mut h);
            let ext = if path.ends_with(".gif") { "gif" } else { "webp" };
            return egui::Image::from_bytes(format!("bytes://anim/{:016x}.{ext}", h.finish()), bytes);
        }
    }
    egui::Image::new(url.to_string())
}

/// A pack's stickers in a grid; a click sends one to `thread`.
fn sticker_grid(ui: &mut egui::Ui, stickers: &Option<Vec<crate::session::Sticker>>, thread: i64, cmds: &mut Vec<Command>, close: &mut bool) {
    const CELL: f32 = 84.0;
    match stickers {
        None => {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| ui.spinner());
        }
        Some(list) if list.is_empty() => {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| ui.label(sf("No stickers", 13.0, 500).color(pal().muted)));
        }
        Some(list) => {
            ui.spacing_mut().item_spacing = Vec2::splat(2.0);
            ui.horizontal_wrapped(|ui| {
                for s in list {
                    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(CELL), Sense::click());
                    if resp.hovered() {
                        ui.painter().rect_filled(rect, CornerRadius::same(10), pal().selected);
                    }
                    if ui.is_rect_visible(rect) {
                        media_image(ui.ctx(), &s.image).paint_at(ui, rect.shrink(6.0));
                    }
                    let resp = if s.label.is_empty() { resp } else { resp.on_hover_text(&s.label) };
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        cmds.push(Command::SendSticker { thread, sticker_id: s.id, image: s.image.clone(), label: s.label.clone() });
                        *close = true;
                    }
                }
            });
        }
    }
}

/// The "+" at the end of the sticker tabs (opens the store): a drawn plus
/// in a 38 pt tab, like the pack tabs.
fn plus_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(38.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(8), pal().selected);
    }
    let c = rect.center();
    let color = if resp.hovered() { pal().blue } else { pal().muted };
    let st = Stroke::new(1.8, color);
    ui.painter().line_segment([c - Vec2::new(7.0, 0.0), c + Vec2::new(7.0, 0.0)], st);
    ui.painter().line_segment([c - Vec2::new(0.0, 7.0), c + Vec2::new(0.0, 7.0)], st);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A drawn "back" chevron button.
fn chevron_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
    if resp.hovered() {
        ui.painter().circle_filled(rect.center(), 14.0, pal().quote);
    }
    let c = rect.center();
    let st = Stroke::new(2.0, if resp.hovered() { pal().blue } else { pal().text });
    ui.painter().line_segment([c + Vec2::new(3.0, -6.0), c + Vec2::new(-3.0, 0.0)], st);
    ui.painter().line_segment([c + Vec2::new(-3.0, 0.0), c + Vec2::new(3.0, 6.0)], st);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A 32 pt round action with an SF Symbol: filled blue (`!quiet`) or a quiet
/// translucent one; a spinner while `busy`.
fn circle_action(ui: &mut egui::Ui, symbol: &str, quiet: bool, busy: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::click());
    let fill = match (quiet, resp.hovered()) {
        (false, _) => pal().blue,
        (true, true) => pal().selected,
        (true, false) => pal().input,
    };
    ui.painter().circle_filled(rect.center(), 16.0, fill);
    if busy {
        egui::Spinner::new().size(14.0).paint_at(ui, Rect::from_center_size(rect.center(), Vec2::splat(14.0)));
    } else {
        let ink = if quiet { pal().text } else { Color32::WHITE };
        if let Some(png) = crate::macos::sf_symbol_png(symbol, 13.0, [ink.r(), ink.g(), ink.b()]) {
            let dim = |i: usize| png.get(i..i + 4).map_or(1.0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f32);
            let (w, h) = (dim(16), dim(20));
            let scale = 15.0 / w.max(h);
            let r = Rect::from_center_size(rect.center(), Vec2::new(w * scale, h * scale));
            egui::Image::from_bytes(format!("bytes://sym/{symbol}-{:?}.png", ink), png).paint_at(ui, r);
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Up to three emoji and nothing else, shown large like Messenger does.
/// Emoticons such as "=))" or ":)" are punctuation, not emoji: they stay text.
fn is_big_emoji(text: &str) -> bool {
    let joiner = |c: char| matches!(c, '\u{200D}' | '\u{FE0E}' | '\u{FE0F}' | '\u{20E3}') || ('\u{1F3FB}'..='\u{1F3FF}').contains(&c) || ('\u{E0020}'..='\u{E007F}').contains(&c);
    let pictograph = |c: char| {
        ('\u{1F000}'..='\u{1FAFF}').contains(&c)
            || ('\u{2600}'..='\u{27BF}').contains(&c)
            || ('\u{2B00}'..='\u{2BFF}').contains(&c)
            || ('\u{2190}'..='\u{21FF}').contains(&c)
            || ('\u{2300}'..='\u{23FF}').contains(&c)
            || matches!(c, '\u{00A9}' | '\u{00AE}' | '\u{203C}' | '\u{2049}' | '\u{2122}' | '\u{2139}' | '\u{3030}' | '\u{303D}' | '\u{3297}' | '\u{3299}')
    };
    let text = text.trim();
    let mut count = 0;
    let mut regional_seen = 0;
    let mut prev_zwj = false;
    for c in text.chars() {
        if joiner(c) {
            prev_zwj = c == '\u{200D}';
            continue;
        }
        if !pictograph(c) {
            return false;
        }
        // Parts of a ZWJ sequence (family, profession emoji) count once, and
        // so does a flag (a pair of regional indicators).
        let regional = ('\u{1F1E6}'..='\u{1F1FF}').contains(&c);
        if regional {
            regional_seen += 1;
        }
        if !prev_zwj && (!regional || regional_seen % 2 == 1) {
            count += 1;
        }
        prev_zwj = false;
    }
    count > 0 && count <= 3
}

/// `MESSENGER_WEB_PROBE=restore`: where the web client keeps restored
/// history. Lists the local database's tables whose names mention messages,
/// backups or restore, with row counts. Names and counts only, no content.
const RESTORE_PROBE_JS: &str = r#"
    const out = {};
    try {
        const db = await require('LSDatabaseSingleton').getLSDatabaseSingletonPromiseOrValue();
        const ReQL = require('ReQL');
        const names = Object.keys(db.tables || {});
        out.tableCount = names.length;
        out.tables = {};
        for (const name of names.filter((n) => /message|backup|restore|minos|epoch|e2ee|secure|thread/i.test(n))) {
            try {
                const rows = await ReQL.toArrayAsync(ReQL.fromTableAscending(db.tables[name]));
                out.tables[name] = rows.length;
            } catch (e) { out.tables[name] = 'err ' + String(e).slice(0, 40); }
        }
    } catch (e) { out.error = String(e).slice(0, 160); }
    return JSON.stringify(out);
"#;

/// `MESSENGER_WEB_PROBE=1`: open the hidden web engine and log (to stderr)
/// whether the web client's restore modules are reachable. Read-only.
fn web_probe(ctx: &egui::Context) {
    thread_local! {
        static PROBE: std::cell::Cell<(u8, u64, Option<Instant>)> = const { std::cell::Cell::new((0, 0, None)) };
    }
    if std::env::var_os("MESSENGER_WEB_PROBE").is_none() {
        return;
    }
    let (stage, id, since) = PROBE.get();
    match stage {
        0 => {
            if std::env::var("MESSENGER_WEB_PROBE").as_deref() != Ok("media") {
                crate::macos::web_open("https://www.facebook.com/messages/", crate::session::USER_AGENT);
            }
            PROBE.set((1, 0, Some(Instant::now())));
        }
        // The media probe reads the chat the history job opened instead.
        1 if std::env::var("MESSENGER_WEB_PROBE").as_deref() == Ok("media") && since.is_some_and(|t| t.elapsed() > std::time::Duration::from_secs(40)) => {
            if let Some(id) = crate::macos::web_call(MEDIA_PROBE_JS) {
                PROBE.set((2, id, since));
            }
        }
        1 if std::env::var("MESSENGER_WEB_PROBE").as_deref() != Ok("media") && crate::macos::web_loaded() && since.is_some_and(|t| t.elapsed() > std::time::Duration::from_secs(8)) => {
            let body = if std::env::var("MESSENGER_WEB_PROBE").as_deref() == Ok("restore") { RESTORE_PROBE_JS } else if std::env::var("MESSENGER_WEB_PROBE").as_deref() == Ok("media") { MEDIA_PROBE_JS } else { r#"
                const out = {path: location.pathname, hasRequire: typeof require};
                try { out.vesta = Object.keys(require('VestaClient')); } catch (e) { out.vestaErr = String(e).slice(0, 80); }
                try {
                    const dtsg = require('DTSGInitialData').token, lsd = require('LSD').token;
                    const body = new URLSearchParams({fb_dtsg: dtsg, lsd, fb_api_caller_class: 'RelayModern', fb_api_req_friendly_name: 'useMWChatEncryptedBackupsVestaUserInfoRefetchQuery', doc_id: '23870520485955982', variables: '{}', server_timestamps: 'true'});
                    const t = await (await fetch('/api/graphql/', {method: 'POST', body, headers: {'X-FB-LSD': lsd}})).text();
                    out.vestaInfo = JSON.parse(t.split('\n')[0]).data.viewer.messenger_default_encrypted_backup.vesta_user_info;
                } catch (e) { out.infoErr = String(e).slice(0, 120); }
                return JSON.stringify(out);
            "# };
            if let Some(id) = crate::macos::web_call(body) {
                PROBE.set((2, id, since));
            }
        }
        2 => {
            if let Some(r) = crate::macos::web_result(id) {
                eprintln!("[web probe] {r:?}");
                PROBE.set((3, 0, None));
            }
        }
        _ => {}
    }
    if stage < 3 {
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }
}

struct HistoryJob {
    thread: i64,
    stage: HistoryStage,
    started: Instant,
    found: usize,
}

enum HistoryStage {
    Start,
    /// The hidden page is navigating to the chat.
    Opening(Instant),
    /// Waiting before the next read (time of the last one).
    Idle(Instant),
    Reading(u64),
}

/// "yes" if the web client's database holds a restored backup (the PIN
/// restore ran on this Mac). Row counts only.
const BACKUP_PRESENT_JS: &str = r#"
    const ReQL = require('ReQL');
    const db = await require('LSDatabaseSingleton').getLSDatabaseSingletonPromiseOrValue();
    const t = db.tables.secure_encrypted_backups_recovery_code_status;
    if (!t) return 'no';
    const rows = await ReQL.toArrayAsync(ReQL.fromTableAscending(t));
    return rows.length > 0 ? 'yes' : 'no';
"#;

/// Navigate the hidden page to an encrypted chat (`threadKey`), which makes
/// the web client restore that chat's history from the backup.
const OPEN_THREAD_JS: &str = r#"
    location.assign('/messages/e2ee/t/' + encodeURIComponent(threadKey));
    return 'ok';
"#;

/// Read one chat's messages (`threadKey`) and their attachments from the web
/// client's database, as JSON. 64-bit columns are I64 pairs in that database.
/// Read-only probe (`MESSENGER_WEB_PROBE=media`): where the web keeps the
/// keys of encrypted media. Database, store and field names only, no content.
const MEDIA_PROBE_JS: &str = r#"
    // Run with the hidden page on an encrypted chat (the app opens the
    // selected one). Table and field names only.
    await new Promise((ok) => setTimeout(ok, 20000));
    const I64 = require('I64');
    const ReQL = require('ReQL');
    const db = await require('LSDatabaseSingleton').getLSDatabaseSingletonPromiseOrValue();
    const str = (v) => v == null ? null : (Array.isArray(v) ? I64.to_string(v) : String(v));
    const key = (location.pathname.match(/\/t\/(\d+)/) || [])[1];
    const msgs = (await ReQL.toArrayAsync(ReQL.fromTableAscending(db.tables.messages))).filter((r) => str(r.threadKey) === key);
    const ids = new Set(msgs.map((m) => m.messageId));
    const out = { path: location.pathname, messages: msgs.length, tables: {}, msgFields: {} };
    for (const m of msgs) for (const k of Object.keys(m)) if (m[k] != null && m[k] !== '' && m[k] !== false) out.msgFields[k] = (out.msgFields[k] || 0) + 1;
    for (const name of Object.keys(db.tables)) {
        try {
            const rows = await ReQL.toArrayAsync(ReQL.fromTableAscending(db.tables[name]));
            const hit = rows.filter((r) => r && (ids.has(r.messageId) || ids.has(r.message_id)));
            if (hit.length && name !== 'messages') out.tables[name] = { rows: hit.length, fields: Object.keys(hit[0]) };
        } catch (e) {}
    }
    return JSON.stringify(out);
"#;

const READ_THREAD_JS: &str = r#"
    const I64 = require('I64');
    const ReQL = require('ReQL');
    const db = await require('LSDatabaseSingleton').getLSDatabaseSingletonPromiseOrValue();
    const str = (v) => v == null ? null : (Array.isArray(v) ? I64.to_string(v) : String(v));
    const all = (t) => t ? ReQL.toArrayAsync(ReQL.fromTableAscending(t)) : Promise.resolve([]);
    // Encrypted chats' text is "vaulted" at rest (MAWVault); open it like
    // the web does when it shows a message. Unopenable text comes back empty.
    let vault = null;
    try { vault = require('MAWVault'); } catch (e) {}
    const open = async (v) => {
        if (typeof v !== 'string' || !v) return v || '';
        if (!vault || !vault.isVaulted(v)) return v;
        try { const t = await vault.unvault(v); return typeof t === 'string' ? t : ''; } catch (e) { return ''; }
    };
    // Scroll the open chat's message list back, like a user reading, so the
    // web loads (and restores) older messages and media for the next read.
    try {
        const root = document.querySelector('[role="main"]') || document.body;
        let list = null;
        for (const el of root.querySelectorAll('div')) {
            if (el.scrollHeight > el.clientHeight + 100 && /auto|scroll/.test(getComputedStyle(el).overflowY) && (!list || el.scrollHeight > list.scrollHeight)) list = el;
        }
        // One screen per read: the web restores a message's media when it
        // renders it, so every message has to pass through view.
        if (list) list.scrollTop = Math.max(0, list.scrollTop - list.clientHeight * 0.8);
    } catch (e) {}
    const rows = (await all(db.tables.messages)).filter((r) => str(r.threadKey) === threadKey);
    const ids = new Set(rows.map((r) => r.messageId));
    const atts = (await all(db.tables.attachments)).filter((a) => ids.has(a.messageId));
    const messages = [];
    for (const r of rows) {
        messages.push({ id: r.messageId, ts: str(r.timestampMs), sender: str(r.senderId), text: await open(r.text), unsent: !!r.isUnsent, admin: !!r.isAdminMessage });
    }
    // Encrypted photos have no link: ask the web's media backend for the
    // picture by its plaintext hash (attachmentFbid), as the web's own
    // thumbnail view does, and hand it over as a data: URL.
    const toDataUrl = async (url) => {
        if (typeof url !== 'string' || !url) return '';
        if (url.startsWith('data:')) return url;
        if (!url.startsWith('blob:')) return '';
        const blob = await (await fetch(url)).blob();
        return await new Promise((ok) => { const r = new FileReader(); r.onload = () => ok(String(r.result)); r.onerror = () => ok(''); r.readAsDataURL(blob); });
    };
    const thumbnail = async (a) => {
        try {
            const preview = await open(a.previewUrl);
            if (preview && !String(preview).startsWith('http')) return await toDataUrl(preview);
            const hash = a.attachmentFbid;
            if (typeof hash !== 'string' || !hash || /^\d+$/.test(hash)) return '';
            const url = await Promise.race([
                require('MAWBridgeSendAndReceive').sendAndReceive('backend', 'getThumbnailBlobDataUrlByHashedPlaintextHash', { description: 'messenger_desktop', plaintextHash: require('WAHashUtils').stringToPlaintextHash(hash) }),
                new Promise((ok) => setTimeout(() => ok(''), 10000)),
            ]);
            return await toDataUrl(url);
        } catch (e) { return ''; }
    };
    const attachments = [];
    for (const a of atts) {
        attachments.push({ id: a.messageId, mime: a.previewUrlMimeType || '', preview: await open(a.previewUrl), w: str(a.previewWidth), h: str(a.previewHeight), title: await open(a.titleText), thumb: await thumbnail(a),
            media: typeof a.attachmentFbid === 'string' && a.attachmentFbid !== '' && !/^\d+$/.test(a.attachmentFbid),
            xma: !!a.hasXma, cta: typeof a.defaultCtaType === 'string' ? a.defaultCtaType : '',
            subtitle: await open(a.subtitleText), description: await open(a.descriptionText),
            dataclass: typeof a.xmaDataclass === 'string' ? a.xmaDataclass : '' });
    }
    return JSON.stringify({ messages, attachments });
"#;

/// Messages from `READ_THREAD_JS`'s JSON. Attachments with a web link come
/// along; ones only the page can open (blob: URLs) show as a label.
fn parse_restored(json: &str) -> Vec<Message> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return Vec::new() };
    let num = |x: &serde_json::Value| x.as_str().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let mut out: Vec<Message> = v["messages"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter(|m| !m["admin"].as_bool().unwrap_or(false))
                .filter(|m| !crate::store::is_secure_text_token(m["text"].as_str().unwrap_or_default()))
                .map(|m| Message {
                    id: m["id"].as_str().unwrap_or_default().to_string(),
                    sender_id: num(&m["sender"]),
                    timestamp_ms: num(&m["ts"]),
                    text: m["text"].as_str().unwrap_or_default().to_string(),
                    unsent: m["unsent"].as_bool().unwrap_or(false),
                    ..Default::default()
                })
                .collect()
        })
        .unwrap_or_default();
    for a in v["attachments"].as_array().into_iter().flatten() {
        let Some(m) = out.iter_mut().find(|m| m.id == a["id"].as_str().unwrap_or_default()) else { continue };
        let mime = a["mime"].as_str().unwrap_or_default();
        let preview = a["preview"].as_str().unwrap_or_default();
        let local = a["thumb"].as_str().and_then(save_restored_picture);
        if let Some(uri) = local {
            m.attachments.push(Attachment {
                kind: AttachmentKind::Image,
                url: uri.clone(),
                preview_url: uri,
                title: String::new(),
                subtitle: String::new(),
                width: num(&a["w"]) as f32,
                height: num(&a["h"]) as f32,
            });
            if matches!(m.text.as_str(), "📷 Photo" | "🎬 Video") {
                m.text.clear();
            }
        } else if preview.starts_with("http") {
            m.attachments.push(Attachment {
                kind: if mime.starts_with("video") { AttachmentKind::Video } else { AttachmentKind::Image },
                url: preview.to_string(),
                preview_url: preview.to_string(),
                title: a["title"].as_str().unwrap_or_default().to_string(),
                subtitle: String::new(),
                width: num(&a["w"]) as f32,
                height: num(&a["h"]) as f32,
            });
        } else if m.text.is_empty() {
            m.text = restored_label(a, mime);
        }
    }
    // Nothing to show yet (text the page couldn't open): skip it, a later
    // read may have it.
    out.retain(|m| !m.text.is_empty() || !m.attachments.is_empty() || m.unsent);
    out
}

/// A call length like the phone shows it: "4 min 26 sec", "1 hr 3 min", "12 sec".
fn call_duration(secs: i64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    match (h, m) {
        (0, 0) => format!("{s} sec"),
        (0, _) => format!("{m} min {s} sec"),
        _ => format!("{h} hr {m} min"),
    }
}

/// What to show for a restored attachment the app can't display. Calls are
/// share cards (XMA) whose default action is `xma_rtc_*`; their title (e.g.
/// "Video call ended") is used when the page could open it.
fn restored_label(a: &serde_json::Value, mime: &str) -> String {
    let cta = a["cta"].as_str().unwrap_or_default();
    let readable = |k: &str| a[k].as_str().map(str::trim).filter(|t| !t.is_empty() && !crate::store::is_secure_text_token(t)).map(str::to_string);
    let title = readable("title").unwrap_or_default();
    let title = title.as_str();
    if std::env::var_os("MESSENGER_DEBUG").is_some() && cta.starts_with("xma_rtc") {
        // Which fields are readable, not their text.
        fn paths(v: &serde_json::Value, at: String, out: &mut Vec<String>) {
            match v {
                serde_json::Value::Object(o) => o.iter().for_each(|(k, x)| paths(x, format!("{at}.{k}"), out)),
                serde_json::Value::Array(l) => l.iter().take(2).enumerate().for_each(|(i, x)| paths(x, format!("{at}[{i}]"), out)),
                serde_json::Value::String(s) => out.push(format!("{at}:str{}", s.len())),
                other => out.push(format!("{at}:{}", if other.is_number() { "num" } else { "other" })),
            }
        }
        let mut dataclass_keys = Vec::new();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(a["dataclass"].as_str().unwrap_or("{}")) {
            paths(&v, String::new(), &mut dataclass_keys);
        }
        eprintln!("[history] call card: title {} subtitle {} description {} dataclass keys {dataclass_keys:?}", !title.is_empty(), readable("subtitle").is_some(), readable("description").is_some());
    }
    // Call cards keep the call in `xmaDataclass.content.custom_template_data`:
    // `call_type` (video/audio), `call_state`, `call_duration_sec`.
    let call = serde_json::from_str::<serde_json::Value>(a["dataclass"].as_str().unwrap_or("{}")).ok().map(|v| v["content"]["custom_template_data"].clone()).filter(|c| c.is_object());
    if let Some(c) = call.filter(|_| cta.starts_with("xma_rtc")) {
        let video = c["call_type"].as_str().map_or(cta.contains("video"), |t| t.contains("video"));
        let secs = c["call_duration_sec"].as_i64().unwrap_or(0);
        let state = c["call_state"].as_str().unwrap_or_default().to_lowercase();
        // 📹 for video, 📞 for audio; the length on its own line, like the phone.
        let (icon, kind) = if video { ("📹", "Video call") } else { ("📞", "Audio call") };
        if state.contains("miss") || (secs == 0 && cta.contains("missed")) {
            return format!("{icon} Missed {}", kind.to_lowercase());
        }
        return if secs > 0 { format!("{icon} {kind}\n{}", call_duration(secs)) } else { format!("{icon} {kind}") };
    }
    if cta.starts_with("xma_rtc") {
        // "Video call" plus the duration line ("4 min 26 sec"), like the phone.
        let detail = readable("subtitle").or_else(|| readable("description"));
        if !title.is_empty() {
            return match detail {
                Some(d) => format!("📞 {title} · {d}"),
                None => format!("📞 {title}"),
            };
        }
        let video = cta.contains("video");
        return match (cta.contains("missed"), video) {
            (true, true) => "📞 Missed video call",
            (true, false) => "📞 Missed call",
            (false, true) => "📞 Video call",
            (false, false) => "📞 Audio call",
        }
        .into();
    }
    if a["xma"].as_bool() == Some(true) {
        return if title.is_empty() || crate::store::is_secure_text_token(title) { "🔗 Shared link".into() } else { format!("🔗 {title}") };
    }
    if mime.starts_with("video") { "🎬 Video".into() } else { "📷 Photo".into() }
}

/// Save a restored photo (a `data:image/...;base64,` URL from the web) to
/// the media cache, 0600, named by its hash. Returns its `file://` URL.
fn save_restored_picture(data_url: &str) -> Option<String> {
    use base64::Engine;
    use sha2::Digest;
    let (head, body) = data_url.strip_prefix("data:")?.split_once(',')?;
    let ext = head.strip_prefix("image/")?.split(';').next()?.replace("jpeg", "jpg");
    let bytes = base64::engine::general_purpose::STANDARD.decode(body).ok()?;
    let name: String = sha2::Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    let path = crate::e2ee_media::cache_dir().ok()?.join(format!("{name}.{ext}"));
    if !path.exists() {
        crate::e2ee_media::write_private(&path, &bytes).ok()?;
    }
    Some(format!("file://{}", path.display()))
}

#[derive(Default)]
struct PinRestore {
    pin: String,
    stage: PinStage,
    focused: bool,
}

#[derive(Default)]
enum PinStage {
    #[default]
    Idle,
    /// Waiting for the hidden Messenger page to load.
    OpeningWeb(Instant),
    /// The restore script is running (web call id).
    Running(u64),
    WrongPin,
    Failed(String),
    Done,
}

/// Runs in the hidden facebook.com/messages page with `pin` and `clientId`
/// (the backup's HSM device id) as arguments. It calls the web client's own
/// restore, as its PIN dialog does (`useMWChatEncryptedBackupsPinCodeRestore`):
/// `mwEBGetPINSecretFromVesta` (PIN login → recovery code) with an
/// `addDevice` that runs `mwEBAddDevice` (adds this backup device and starts
/// the history restore). QPL logging flows are stubbed with no-ops.
const RESTORE_JS: &str = r#"
    const load = async (name) => { const m = await require('requireDeferred')(name).load(); return m; };
    const noop = () => {};
    const flow = new Proxy({}, { get: (_, key) => key === 'then' ? undefined : noop });
    const env = require('CometRelayEnvironment');
    const db = await require('LSDatabaseSingleton').getLSDatabaseSingletonPromiseOrValue();
    const addDeviceModule = await load('mwEBAddDevice');
    const getSecretModule = await load('mwEBGetPINSecretFromVesta');
    const getSecret = getSecretModule.default || getSecretModule;
    let finish;
    const added = new Promise((resolve) => { finish = resolve; });
    const addDevice = (recoveryCode) => {
        addDeviceModule.EBAddDevice({
            environment: env, recoveryCode, restoreDeviceAuthQplFlow: flow, storage: db,
            onSuccess: () => finish({ ok: true }),
            onFailure: (e) => finish({ ok: false, error: 'add device: ' + String(e) }),
            onInvalidRecoveryCode: () => finish({ ok: false, error: 'invalid recovery code' }),
        });
    };
    let wrongPin = false;
    try {
        await getSecret({
            addDevice, clientID: clientId, environment: env, getSourceFromContext: () => 'thread_view',
            onSubmitPin: noop, onSubmitPinFailure: noop, onVestaSecretFetch: noop, pinCode: pin,
            productUseCase: 'MSGR_ENC_BACKUP', setIsInProgress: noop,
            setIsPinCodeIncorrect: (v) => { wrongPin = !!v; }, setIsVestaError: noop,
            startRestoreDeviceAuthQplFlow: () => flow,
        });
    } catch (e) {
        return JSON.stringify({ ok: false, wrongPin, error: String((e && e.message) || e).slice(0, 200) });
    }
    if (wrongPin) return JSON.stringify({ ok: false, wrongPin: true });
    const result = await Promise.race([added, new Promise((r) => setTimeout(() => r({ ok: false, error: 'adding the device timed out' }), 180000))]);
    return JSON.stringify(result);
"#;

/// Memory key of the image open in the in-app viewer.
const IMAGE_VIEWER: &str = "image_viewer";

/// The in-app image viewer: the picture fitted over a dark scrim, above
/// everything. Esc, the close button or a click outside the picture close it;
/// the arrow button opens the original in the browser.
fn image_viewer(ctx: &egui::Context) {
    let id = egui::Id::new(IMAGE_VIEWER);
    let Some(Some(url)) = ctx.data(|d| d.get_temp::<Option<String>>(id)) else { return };
    let mut close = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    let screen = ctx.content_rect();
    egui::Area::new(egui::Id::new("image_viewer_area"))
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            let scrim = ui.allocate_rect(screen, Sense::click());
            ui.painter().rect_filled(screen, CornerRadius::ZERO, Color32::from_black_alpha(225));
            let area = screen.shrink2(Vec2::new(48.0, 56.0));
            let image = egui::Image::new(url.as_str()).max_size(area.size()).corner_radius(CornerRadius::same(12));
            let size = image.load_and_calc_size(ui, area.size()).unwrap_or(Vec2::splat(64.0));
            let rect = Rect::from_center_size(area.center(), size);
            image.paint_at(ui, rect);
            if scrim.clicked() && !ui.rect_contains_pointer(rect) {
                close = true;
            }
            // Close (✕) and open-in-browser buttons, top right.
            let button = |ui: &mut egui::Ui, center: Pos2, salt: &str| {
                let r = Rect::from_center_size(center, Vec2::splat(32.0));
                let resp = ui.interact(r, egui::Id::new(("viewer_btn", salt)), Sense::click());
                let fill = if resp.hovered() { Color32::from_white_alpha(60) } else { Color32::from_white_alpha(30) };
                ui.painter().circle_filled(center, 16.0, fill);
                resp.on_hover_cursor(egui::CursorIcon::PointingHand)
            };
            let c = Pos2::new(screen.right() - 32.0, screen.top() + 40.0);
            let x = button(ui, c, "close");
            let st = Stroke::new(1.8, Color32::WHITE);
            ui.painter().line_segment([c + Vec2::splat(-5.5), c + Vec2::splat(5.5)], st);
            ui.painter().line_segment([c + Vec2::new(-5.5, 5.5), c + Vec2::new(5.5, -5.5)], st);
            if x.on_hover_text("Close").clicked() {
                close = true;
            }
            let c2 = c - Vec2::new(44.0, 0.0);
            let open = button(ui, c2, "open");
            ui.painter().line_segment([c2 + Vec2::new(-4.5, 4.5), c2 + Vec2::new(4.5, -4.5)], st);
            ui.painter().line_segment([c2 + Vec2::new(-0.5, -4.5), c2 + Vec2::new(4.5, -4.5)], st);
            ui.painter().line_segment([c2 + Vec2::new(4.5, -4.5), c2 + Vec2::new(4.5, 0.5)], st);
            if open.on_hover_text("Open in browser").clicked() && url.starts_with("http") {
                ui.ctx().open_url(egui::OpenUrl::new_tab(&url));
            }
        });
    if close {
        ctx.data_mut(|d| d.remove::<Option<String>>(id));
    }
}

#[cfg(test)]
mod emoji_tests {
    use super::is_big_emoji;

    #[test]
    fn big_emoji_only_for_real_emoji() {
        assert!(is_big_emoji("👍"));
        assert!(is_big_emoji("😆😆😆"));
        assert!(is_big_emoji("❤️"));
        assert!(is_big_emoji("👨‍👩‍👧"));
        assert!(is_big_emoji("🇻🇳"));
        assert!(!is_big_emoji("=))"));
        assert!(!is_big_emoji(":)"));
        assert!(!is_big_emoji("..."));
        assert!(!is_big_emoji("😆😆😆😆"));
        assert!(!is_big_emoji("ok 👍"));
    }
}

/// Write a pasted clipboard image to a PNG in the cache, for sending.
fn save_pasted_image(img: &egui::ColorImage) -> anyhow::Result<std::path::PathBuf> {
    let [w, h] = img.size;
    let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_srgba_unmultiplied()).collect();
    let buffer = image::RgbaImage::from_raw(w as u32, h as u32, rgba).ok_or_else(|| anyhow::anyhow!("bad image size"))?;
    let dir = crate::e2ee_media::cache_dir()?.join("pasted");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("Pasted image {}.png", chrono::Local::now().format("%Y-%m-%d at %H.%M.%S")));
    buffer.save_with_format(&path, image::ImageFormat::Png)?;
    Ok(path)
}

/// Category title and SF Symbol for the "+" reaction picker.
fn emoji_group_label(group: emojis::Group) -> (&'static str, &'static str) {
    use emojis::Group::*;
    match group {
        SmileysAndEmotion => ("Smileys & Emotion", "face.smiling"),
        PeopleAndBody => ("People & Body", "hand.raised"),
        AnimalsAndNature => ("Animals & Nature", "pawprint"),
        FoodAndDrink => ("Food & Drink", "fork.knife"),
        TravelAndPlaces => ("Travel & Places", "car"),
        Activities => ("Activities", "soccerball"),
        Objects => ("Objects", "lightbulb"),
        Symbols => ("Symbols", "heart"),
        Flags => ("Flags", "flag"),
    }
}

fn glass_rect(r: Rect, radius: f32) -> crate::macos::GlassRect {
    [r.left(), r.top(), r.width(), r.height(), radius]
}

impl MessengerApp {
    /// Logged out or session expired: Facebook's login page filling the main
    /// window. Once it yields a new session, save it and relaunch with it.
    fn login_screen(&mut self, ui: &mut egui::Ui) {
        const LOGIN_URL: &str = "https://www.facebook.com/login/?next=https%3A%2F%2Fwww.facebook.com%2Fmessages%2F";
        let full = ui.available_rect_before_wrap();
        ui.allocate_rect(full, Sense::hover());
        titlebar_drag(ui, full.with_max_y(full.top() + TITLEBAR));

        // The page fills the window below the titlebar strip (traffic lights).
        let page = full.with_min_y(full.top() + TITLEBAR);
        crate::macos::login_view(LOGIN_URL, crate::session::USER_AGENT, [page.left(), page.top(), page.width(), page.height()]);

        let stale = self.login_stale_xs.get_or_insert_with(|| crate::session::Cookies::load().and_then(|c| c.get("xs").map(str::to_owned)));
        if let Some(header) = crate::macos::login_cookies() {
            let cookies = crate::session::Cookies(header);
            if cookies.is_logged_in() && cookies.get("xs") != stale.as_deref() {
                match cookies.save() {
                    Ok(()) => {
                        crate::macos::login_view_close();
                        relaunch();
                    }
                    Err(e) => *self.shared.error.lock().unwrap() = Some(format!("Couldn't save the session: {e:#}")),
                }
            }
        }
        let now = ui.input(|i| i.time);
        if now - self.login_poll >= 1.0 {
            self.login_poll = now;
            crate::macos::request_login_cookies();
        }
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    }

    fn status_line(&mut self, ui: &mut egui::Ui, status: &Status) {
        let (text, color) = match status {
            Status::Online => (String::new(), pal().muted),
            // Cached chats show while connecting, so no "loading" notice.
            Status::Connecting | Status::Syncing => (String::new(), pal().muted),
            Status::Reconnecting(e) => (format!("Reconnecting — {e}"), Color32::from_rgb(240, 180, 60)),
            Status::LoggedOut(e) => (e.clone(), Color32::from_rgb(240, 90, 90)),
        };
        let err = self.shared.error.lock().unwrap().clone();
        if text.is_empty() && err.is_none() {
            return;
        }
        Frame::default().fill(pal().quote).corner_radius(CornerRadius::same(8)).inner_margin(Margin::symmetric(10, 6)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if !text.is_empty() {
                ui.add(egui::Label::new(RichText::new(text).size(13.0).color(color)).wrap());
            }
            if let Some(e) = err {
                // Message wrapped in the column minus room for a drawn close button.
                let width = (ui.available_width() - 24.0).max(40.0);
                ui.horizontal_top(|ui| {
                    ui.allocate_ui(Vec2::new(width, 0.0), |ui| {
                        ui.set_width(width);
                        ui.add(egui::Label::new(RichText::new(truncate(&e, 240)).size(13.0).color(Color32::from_rgb(240, 90, 90))).wrap());
                    });
                    let (rect, close) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
                    let color = if close.hovered() { pal().text } else { pal().muted };
                    let c = rect.center();
                    ui.painter().line_segment([c + Vec2::splat(-4.0), c + Vec2::splat(4.0)], Stroke::new(1.5, color));
                    ui.painter().line_segment([c + Vec2::new(-4.0, 4.0), c + Vec2::new(4.0, -4.0)], Stroke::new(1.5, color));
                    if close.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        *self.shared.error.lock().unwrap() = None;
                    }
                });
            }
        });
        ui.add_space(6.0);
    }

    fn thread_list(&mut self, ui: &mut egui::Ui, store: &Store, status: &Status, cmds: &mut Vec<Command>) {
        // Leave room for the traffic lights; the strip drags the window.
        let strip = ui.allocate_space(Vec2::new(ui.available_width(), TITLEBAR)).1;
        titlebar_drag(ui, strip);

        ui.horizontal(|ui| {
            ui.add_space(2.0);
            ui.label(sf("Chats", 24.0, 700));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let menu = ui.add(egui::Button::new(sf("⋯", 18.0, 700)).fill(pal().input).corner_radius(CornerRadius::same(18)).min_size(Vec2::splat(36.0)));
                if menu.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    // Native NSMenu: macOS draws it as Liquid Glass.
                    use crate::macos::MenuEntry;
                    const THEMES: [ThemePref; 3] = [ThemePref::System, ThemePref::Light, ThemePref::Dark];
                    let mut entries = vec![MenuEntry::Header("Appearance".into())];
                    entries.extend(THEMES.iter().map(|&t| MenuEntry::Choice { title: t.label().into(), checked: self.theme == t }));
                    entries.push(MenuEntry::Separator);
                    entries.push(MenuEntry::Item { title: "Log out".into(), symbol: "rectangle.portrait.and.arrow.right" });
                    match crate::macos::popup_menu(&entries) {
                        Some(i) if i < THEMES.len() => self.theme = THEMES[i],
                        Some(_) => {
                            crate::session::Cookies::clear();
                            crate::store::clear_cache();
                            crate::macos::clear_web_data(relaunch);
                        }
                        None => {}
                    }
                }
            });
        });
        ui.add_space(8.0);
        pill_search(ui, &mut self.search);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            for (f, label) in [(Filter::All, "All"), (Filter::Unread, "Unread"), (Filter::Groups, "Groups")] {
                let on = self.filter == f;
                let b = egui::Button::new(sf(label, 15.0, if on { 600 } else { 500 }).color(if on { pal().chip_on_text } else { pal().text }))
                    .fill(if on { pal().chip_on_bg } else { Color32::TRANSPARENT })
                    .corner_radius(CornerRadius::same(18))
                    .min_size(Vec2::new(0.0, 36.0));
                if ui.add(b).clicked() {
                    self.filter = f;
                }
            }
        });
        ui.add_space(6.0);
        self.status_line(ui, status);

        let query = self.search.to_lowercase();
        let threads: Vec<&Thread> = store
            .sorted_threads()
            .into_iter()
            .filter(|t| match self.filter {
                Filter::All => true,
                Filter::Unread => t.unread(),
                Filter::Groups => t.is_group(),
            })
            .filter(|t| {
                query.is_empty()
                    || store.thread_title(t).to_lowercase().contains(&query)
                    || t.snippet.to_lowercase().contains(&query)
            })
            .collect();

        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for t in threads {
                self.thread_row(ui, store, t, cmds);
            }
            ui.add_space(10.0);
            ui.vertical_centered(|ui| {
                if ui.add(egui::Button::new(sf("Load more chats", 14.0, 500).color(pal().muted)).frame(false)).clicked() {
                    cmds.push(Command::LoadMoreThreads);
                }
            });
        });
    }

    fn thread_row(&mut self, ui: &mut egui::Ui, store: &Store, t: &Thread, cmds: &mut Vec<Command>) {
        let selected = self.selected == Some(t.key);
        let unread = t.unread();
        let title = store.thread_title(t);
        let typing = !store.typing_in(t.key).is_empty();
        let row_id = ui.scope_id().with(("thread", t.key));
        let hovered = ui.ctx().read_response(row_id).is_some_and(|r| r.hovered());
        let fill = if selected {
            pal().selected
        } else if hovered {
            pal().quote
        } else {
            Color32::TRANSPARENT
        };
        let inner = Frame::default().fill(fill).corner_radius(CornerRadius::same(10)).inner_margin(Margin::symmetric(8, 8)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                thread_avatar(ui, store, t, &title, 56.0);
                ui.add_space(4.0);
                let right_w = 44.0;
                ui.allocate_ui_with_layout(
                    Vec2::new(ui.available_width() - right_w, 56.0),
                    Layout::top_down(Align::Min).with_main_align(Align::Center),
                    |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        let name = sf(&title, 15.0, if unread { 600 } else { 500 }).color(pal().text);
                        ui.add(egui::Label::new(name).truncate());
                        let snippet = if typing {
                            "typing…".to_string()
                        } else if t.e2ee || t.snippet.trim().is_empty() || newest_is_latest(store, t) {
                            // The server can't know encrypted content, and a live
                            // photo or video arrives without a snippet: describe the
                            // newest message we have.
                            e2ee_snippet(store, t)
                        } else {
                            t.snippet.replace('\n', " ")
                        };
                        let s = if unread { sf(&snippet, 13.0, 600).color(pal().text) } else { sf(&snippet, 13.0, 400).color(pal().muted) };
                        ui.add(egui::Label::new(s).truncate());
                    },
                );
                ui.with_layout(Layout::top_down(Align::Max).with_main_align(Align::Center), |ui| {
                    ui.label(sf(&short_time(t.last_activity_ms), 12.0, if unread { 600 } else { 400 }).color(if unread { pal().chip_on_text } else { pal().muted }));
                    if unread {
                        let (r, p) = ui.allocate_painter(Vec2::splat(12.0), Sense::hover());
                        p.circle_filled(r.rect.center(), 6.0, pal().blue);
                    } else if t.muted {
                        ui.label(RichText::new("🔕").size(12.0).color(pal().muted));
                    }
                });
            });
        });
        let resp = ui.interact(inner.response.rect, row_id, Sense::click());
        if resp.clicked() && !selected {
            self.selected = Some(t.key);
            self.reply_to = None;
            self.editing = None;
            self.draft.clear();
        }
        resp.context_menu(|ui| {
            if ui.button("Mark as read").clicked() {
                cmds.push(Command::MarkRead { thread: t.key });
                ui.close();
            }
            ui.menu_button("Mute notifications", |ui| {
                for (label, minutes) in [("For 15 minutes", Some(15)), ("For 1 hour", Some(60)), ("For 8 hours", Some(480)), ("For 24 hours", Some(1440)), ("Until I turn it back on", Some(-1))] {
                    if ui.button(label).clicked() {
                        cmds.push(Command::Mute { thread: t.key, minutes });
                        ui.close();
                    }
                }
            });
            if t.muted && ui.button("Unmute").clicked() {
                cmds.push(Command::Mute { thread: t.key, minutes: None });
                ui.close();
            }
        });
    }

    fn conversation(&mut self, ui: &mut egui::Ui, store: &Store, thread: &Thread, cmds: &mut Vec<Command>) {
        // First time a chat is shown this session: re-fetch its newest messages,
        // so anything missed while offline (a gap between the cache and the
        // last sync) fills in.
        if !thread.e2ee && self.refreshed.insert(thread.key) {
            cmds.push(Command::RefreshRecent { thread: thread.key });
        }
        let key = thread.key;
        let title = store.thread_title(thread);
        let messages: Vec<Message> = store.messages.get(&key).cloned().unwrap_or_default();

        // Mark read when viewing an unread thread while the window is focused.
        if thread.unread() && self.focused && self.read_sent.insert((key, thread.last_activity_ms)) {
            cmds.push(Command::MarkRead { thread: key });
        }

        // Header (extends under the transparent titlebar and drags the window)
        egui::Panel::top(("conv_header", key))
            .exact_size(64.0)
            .frame(Frame::default().inner_margin(Margin::symmetric(16, 0)))
            .show(ui, |ui| {
                titlebar_drag(ui, ui.max_rect());
                ui.horizontal_centered(|ui| {
                    thread_avatar(ui, store, thread, &title, 40.0);
                    ui.add_space(2.0);
                    // Measure the text block so the row can center it next to the
                    // avatar (a plain `vertical` would hug the top of the header).
                    let font = |size: f32, weight: u16| egui::FontId::new(size * text_scale(), egui::FontFamily::Name(format!("sf-{weight}").into()));
                    let line = |size: f32, weight: u16| ui.painter().layout_no_wrap("Ag".into(), font(size, weight), pal().text).size().y;
                    let subtitle = thread.is_group().then(|| format!("{} members", thread.participants.len()));
                    let height = line(17.0, 600) + if subtitle.is_some() { line(13.0, 400) } else { 0.0 };
                    let width = (ui.available_width() - 140.0).max(80.0);
                    ui.allocate_ui_with_layout(Vec2::new(width, height), Layout::top_down(Align::Min), |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.add(egui::Label::new(sf(&title, 17.0, 600)).truncate());
                        if let Some(subtitle) = subtitle {
                            ui.label(sf(&subtitle, 13.0, 400).color(pal().muted));
                        }
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let info = if self.show_info { "info.circle.fill" } else { "info.circle" };
                        if symbol_button(ui, info, pal().blue).on_hover_text("Conversation information").clicked() {
                            self.show_info = !self.show_info;
                        }
                        if symbol_button(ui, "video.fill", pal().blue).on_hover_text("Video call (opens browser)").clicked() {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(format!("https://www.facebook.com/groupcall/ROOM:{key}/?call_type=video")));
                        }
                        if symbol_button(ui, "phone.fill", pal().blue).on_hover_text("Audio call (opens browser)").clicked() {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(format!("https://www.facebook.com/groupcall/ROOM:{key}/?call_type=audio")));
                        }
                    });
                });
            });

        egui::Panel::bottom(("composer", key))
            .frame(Frame::default().inner_margin(Margin { left: 12, right: 12, top: 6, bottom: 12 }))
            .show_separator_line(false)
            .show(ui, |ui| {
                self.composer(ui, store, thread, cmds);
            });

        if thread.e2ee && !self.history_restored && self.restore_check.is_none() {
            self.restore_check = Some(None);
        }
        if thread.e2ee && self.history_restored && self.history_job.is_none() && self.history_fetched.insert(key) {
            self.history_job = Some(HistoryJob { thread: key, stage: HistoryStage::Start, started: Instant::now(), found: 0 });
        }
        if thread.e2ee && messages.is_empty() {
            if e2ee_notice(ui, key) {
                self.open_pin_restore(cmds);
            }
            return;
        }

        // Message list
        let mut area = ScrollArea::vertical().id_salt(("messages", key)).auto_shrink([false, false]).stick_to_bottom(true);
        if let Some((k, offset)) = self.scroll_fix.take() {
            if k == key {
                area = area.vertical_scroll_offset(offset);
            }
        }
        let out = area.show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add_space(12.0);
                if thread.e2ee {
                    ui.vertical_centered(|ui| {
                        ui.label(sf("🔒 End-to-end encrypted. Showing messages received on this Mac.", 12.0, 500).color(pal().muted));
                        if ui.add(egui::Button::new(sf("Restore older history with your PIN", 12.0, 600).color(pal().blue)).frame(false)).clicked() {
                            self.open_pin_restore(cmds);
                        }
                    });
                    ui.add_space(8.0);
                }
                if HISTORY_FETCH && thread.has_more_before && !thread.e2ee {
                    ui.vertical_centered(|ui| ui.spinner());
                    ui.add_space(8.0);
                }
                let readers = readers_by_message(store, thread, &messages);
                let mut prev: Option<&Message> = None;
                for (i, m) in messages.iter().enumerate() {
                    let new_block = prev.is_none_or(|p| m.timestamp_ms - p.timestamp_ms > 20 * 60_000);
                    if new_block {
                        ui.add_space(14.0);
                        ui.vertical_centered(|ui| {
                            ui.label(sf(&long_time(m.timestamp_ms), 12.0, 500).color(pal().muted));
                        });
                        ui.add_space(10.0);
                    }
                    let groups_with = |a: &Message, b: &Message| a.sender_id == b.sender_id && !a.is_admin && !b.is_admin && (b.timestamp_ms - a.timestamp_ms).abs() < 5 * 60_000;
                    let first = new_block || prev.is_none_or(|p| !groups_with(p, m));
                    let last = messages.get(i + 1).is_none_or(|n| !groups_with(m, n) || n.timestamp_ms - m.timestamp_ms > 20 * 60_000);
                    self.message_row(ui, store, thread, m, first, last, cmds);
                    if let Some(ids) = readers.get(&m.id) {
                        seen_by_row(ui, store, key, ids);
                    } else if i + 1 == messages.len() && m.sender_id == store.me && !m.unsent {
                        // The newest message is ours and nobody has seen it yet: show
                        // whether it's sending, sent or delivered (like Messenger).
                        let delivered = m.delivered
                            || store.delivered_receipts.get(&key).is_some_and(|d| d.iter().any(|(id, ts)| *id != store.me && *ts >= m.timestamp_ms));
                        let status = if m.pending { SendStatus::Sending } else if delivered { SendStatus::Delivered } else { SendStatus::Sent };
                        status_row(ui, status);
                    }
                    prev = Some(m);
                }
                let typing = store.typing_in(key);
                if !typing.is_empty() {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.add_space(16.0 + AVATAR_SMALL + 8.0);
                        typing_bubble(ui);
                        let names: Vec<_> = typing.iter().map(|id| store.member_name(key, *id)).collect();
                        if thread.is_group() {
                            ui.label(sf(&format!("{} is typing", names.join(", ")), 12.0, 400).color(pal().muted));
                        }
                    });
                }
                ui.add_space(12.0);
            });

        // Older messages were inserted above: keep the same messages in view
        // (unless we were at the bottom, where stick_to_bottom already does).
        let offset = out.state.clamped_offset().y;
        let first_id = messages.first().map(|m| m.id.clone());
        let opened = self.prev_view.as_ref().filter(|p| p.key == key).map_or_else(Instant::now, |p| p.opened);
        let settled = opened.elapsed().as_millis() > 300;
        if let Some(prev) = &self.prev_view {
            if settled && prev.key == key && prev.first_id != first_id && !prev.at_bottom && out.content_size.y > prev.content_h {
                self.scroll_fix = Some((key, prev.offset + out.content_size.y - prev.content_h));
                ui.ctx().request_discard("keep scroll position after loading older messages");
            }
        }
        self.prev_view = Some(ListView {
            key,
            opened,
            first_id,
            content_h: out.content_size.y,
            offset,
            at_bottom: offset + out.inner_rect.height() >= out.content_size.y - 2.0,
        });

        // Fetch older history while the list doesn't fill the view, holds only a
        // few messages, or is scrolled to the top. One request at a time: wait
        // for the oldest message to change (or 10 s) before asking again.
        let online = *self.shared.status.lock().unwrap() == crate::client::Status::Online;
        let wanted = (settled && offset < 60.0) || out.content_size.y <= out.inner_rect.height() || messages.len() < 20;
        let oldest = messages.iter().find(|m| !m.pending).map(|m| m.id.clone());
        let waiting = self.older_requested.as_ref().is_some_and(|(k, id, t)| {
            *k == key && ((*id == oldest && t.elapsed().as_secs() < 10) || t.elapsed().as_millis() < 500)
        });
        if HISTORY_FETCH && !thread.e2ee && thread.has_more_before && online && wanted && !waiting {
            self.older_requested = Some((key, oldest, Instant::now()));
            cmds.push(Command::LoadOlder { thread: key });
        }
    }


    #[allow(clippy::too_many_arguments)]
    fn message_row(&mut self, ui: &mut egui::Ui, store: &Store, thread: &Thread, m: &Message, first: bool, last: bool, cmds: &mut Vec<Command>) {
        if m.is_admin {
            ui.add_space(6.0);
            ui.vertical_centered(|ui| {
                ui.add(egui::Label::new(sf(&m.text, 12.0, 400).color(pal().muted)).wrap());
            });
            ui.add_space(6.0);
            return;
        }
        let mine = m.sender_id == store.me;
        let gutter = 16.0 + if mine { 0.0 } else { AVATAR_SMALL + 8.0 };
        let max_w = ((ui.available_width() - gutter) * 0.66).clamp(220.0, 560.0);

        if first && !mine && thread.is_group() && m.reply_to_id.is_none() {
            ui.horizontal(|ui| {
                ui.add_space(gutter + 12.0);
                ui.label(sf(&store.member_name(thread.key, m.sender_id), 12.0, 400).color(pal().muted));
            });
            ui.add_space(2.0);
        }

        let layout = if mine { Layout::right_to_left(Align::Min) } else { Layout::left_to_right(Align::Min) };
        let row = ui.with_layout(layout, |ui| {
            ui.add_space(gutter);
            let column = Layout::top_down(if mine { Align::Max } else { Align::Min });
            ui.with_layout(column, |ui| {
                ui.set_max_width(max_w);
                ui.spacing_mut().item_spacing.y = 2.0;
                if let Some(reply_id) = &m.reply_to_id {
                    reply_header(ui, store, thread, m, reply_id, mine);
                }
                self.bubble(ui, m, mine, first, last)
            })
            .inner
        });
        let bubble_rect = row.inner.rect;

        // Sender avatar, bottom-aligned with the last bubble of a group.
        if !mine && last {
            let pic = store.contacts.get(&m.sender_id).map(|c| c.picture.clone()).unwrap_or_default();
            let rect = Rect::from_min_size(Pos2::new(row.response.rect.left() + 16.0, bubble_rect.bottom() - AVATAR_SMALL), Vec2::splat(AVATAR_SMALL));
            paint_avatar(ui, rect, &pic, &store.contact_name(m.sender_id));
        }

        // Reactions: a pill overlapping the bubble's bottom-right corner.
        if !m.reactions.is_empty() {
            let mut counts: Vec<(String, usize)> = Vec::new();
            for e in m.reactions.values() {
                match counts.iter_mut().find(|(x, _)| x == e) {
                    Some(c) => c.1 += 1,
                    None => counts.push((e.clone(), 1)),
                }
            }
            let mut text = counts.iter().map(|(e, _)| e.as_str()).collect::<String>();
            if m.reactions.len() > 1 {
                text.push_str(&format!(" {}", m.reactions.len()));
            }
            let galley = ui.painter().layout_no_wrap(text, egui::FontId::proportional(16.0 * text_scale()), pal().text);
            let size = galley.size() + Vec2::new(12.0, 4.0);
            let x = bubble_rect.right() - size.x - 4.0;
            let pill = Rect::from_min_size(Pos2::new(x, bubble_rect.bottom() - 6.0), size);
            ui.painter().rect_filled(pill, CornerRadius::same(10), pal().quote);
            ui.painter().rect_stroke(pill, CornerRadius::same(10), Stroke::new(2.0, pal().bg), egui::StrokeKind::Outside);
            ui.painter().galley(pill.min + Vec2::new(6.0, 2.0), galley, pal().text);
            let who = m.reactions.iter().map(|(id, e)| format!("{e} {}", store.member_name(thread.key, *id))).collect::<Vec<_>>().join("\n");
            ui.interact(pill, ui.scope_id().with(("reactions", &m.id)), Sense::hover()).on_hover_text(who);
            ui.add_space(size.y);
        }

        let bubble = ui.interact(bubble_rect, ui.scope_id().with(("bubble", &m.id)), Sense::click()).on_hover_text(long_time(m.timestamp_ms));
        // Right-click: a native menu (drawn as Liquid Glass by macOS).
        if bubble.secondary_clicked() {
            self.message_menu(ui.ctx(), store, thread, m, mine, cmds);
        }
        ui.add_space(if last { 10.0 } else { 2.0 });
    }

    /// Draw the message content; returns the union of everything drawn.
    /// The "+" reaction picker, like the web client's: search, your quick
    /// reactions, every emoji by category, and a category bar to jump. Emoji
    /// are drawn in color by AppKit (`macos::emoji_png`), only when visible.
    fn reaction_picker_ui(&mut self, ctx: &egui::Context, store: &Store, cmds: &mut Vec<Command>) {
        // Fits 7 emoji per row and the 9 category buttons (36 pt each).
        const W: f32 = 344.0;
        const CELL: f32 = 44.0;
        let Some(picker) = self.reaction_picker.as_mut() else { return };
        let mine = store.messages.get(&picker.thread).and_then(|v| v.iter().find(|m| m.id == picker.message_id)).and_then(|m| m.reactions.get(&store.me).cloned());
        // Keep the panel on screen, opening up-left of the pointer when needed.
        let screen = ctx.content_rect();
        let size = Vec2::new(W + 20.0, 470.0);
        let pos = Pos2::new(
            picker.anchor.x.min(screen.right() - size.x - 8.0).max(screen.left() + 8.0),
            (picker.anchor.y - size.y / 2.0).clamp(screen.top() + 8.0, (screen.bottom() - size.y - 8.0).max(screen.top() + 8.0)),
        );
        let mut chosen: Option<String> = None;
        let mut close = ctx.input(|i| i.key_pressed(Key::Escape));
        let groups: Vec<emojis::Group> = emojis::Group::iter().collect();
        let area = egui::Area::new(egui::Id::NULL.with("reaction_picker")).order(egui::Order::Foreground).fixed_pos(pos).show(ctx, |ui| {
            Frame::default().fill(pal().popup).stroke(Stroke::new(1.0, pal().border)).corner_radius(CornerRadius::same(16)).inner_margin(Margin::same(10)).show(ui, |ui| {
                ui.set_width(W);
                let search_id = ui.make_persistent_id("reaction_search");
                Frame::default().fill(pal().input).corner_radius(CornerRadius::same(18)).inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
                    ui.set_width(W - 24.0);
                    ui.add(TextEdit::singleline(&mut picker.query).id(search_id).hint_text(sf("Search emoji", 15.0, 400).color(pal().muted)).frame(Frame::NONE).desired_width(f32::INFINITY));
                });
                if picker.opened {
                    ui.memory_mut(|m| m.request_focus(search_id));
                    picker.opened = false;
                }
                ui.add_space(6.0);
                let query = fold_name(picker.query.trim());
                let mut cell = |ui: &mut egui::Ui, e: &str| {
                    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(CELL), Sense::click());
                    if mine.as_deref() == Some(e) || resp.hovered() {
                        ui.painter().rect_filled(rect, CornerRadius::same(10), pal().selected);
                    }
                    if ui.is_rect_visible(rect) {
                        match crate::macos::emoji_png(e) {
                            Some(png) => {
                                egui::Image::from_bytes(format!("bytes://emoji/{e}.png"), png).paint_at(ui, rect.shrink(7.0));
                            }
                            None => {
                                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, e, egui::FontId::proportional(24.0), pal().text);
                            }
                        }
                    }
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        chosen = Some(e.to_string());
                    }
                };
                let header = |ui: &mut egui::Ui, title: &str| {
                    ui.add_space(4.0);
                    ui.label(sf(title, 13.0, 600).color(pal().muted))
                };
                let mut active = 0usize;
                let scroll = ScrollArea::vertical().id_salt("reaction_picker_scroll").max_height(340.0).auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing = Vec2::splat(1.0);
                    if query.is_empty() {
                        header(ui, "Your reactions");
                        ui.horizontal_wrapped(|ui| {
                            for e in QUICK_REACTIONS {
                                cell(ui, e);
                            }
                        });
                    }
                    let top = ui.clip_rect().top();
                    for (gi, group) in groups.iter().enumerate() {
                        let list: Vec<&str> = group
                            .emojis()
                            .filter(|e| query.is_empty() || fold_name(e.name()).contains(&query) || e.shortcodes().any(|c| c.contains(query.as_str())))
                            .map(|e| e.as_str())
                            .collect();
                        if list.is_empty() {
                            continue;
                        }
                        let h = header(ui, emoji_group_label(*group).0);
                        if h.rect.top() <= top + 24.0 {
                            active = gi;
                        }
                        if picker.jump_to == Some(gi) {
                            ui.scroll_to_rect(h.rect, Some(Align::TOP));
                            picker.jump_to = None;
                        }
                        ui.horizontal_wrapped(|ui| {
                            for e in list {
                                cell(ui, e);
                            }
                        });
                    }
                });
                let _ = scroll;
                if query.is_empty() {
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        ui.add_space(((W - 9.0 * 36.0) / 2.0).max(0.0));
                        for (gi, group) in groups.iter().enumerate() {
                            let color = if gi == active { pal().blue } else { pal().muted };
                            if symbol_button(ui, emoji_group_label(*group).1, color).on_hover_text(emoji_group_label(*group).0).clicked() {
                                picker.jump_to = Some(gi);
                            }
                        }
                    });
                }
            });
        });
        // Clicking outside closes it.
        let pointer = ctx.input(|i| i.pointer.interact_pos());
        if ctx.input(|i| i.pointer.any_pressed()) && pointer.is_some_and(|p| !area.response.rect.contains(p)) {
            close = true;
        }
        if let Some(e) = chosen {
            let emoji = if mine.as_deref() == Some(e.as_str()) { String::new() } else { e };
            cmds.push(Command::React { thread: picker.thread, message_id: picker.message_id.clone(), emoji });
            close = true;
        }
        if close {
            self.reaction_picker = None;
        }
    }

    /// The sticker picker (web style): the tray's packs as tabs, the selected
    /// pack's stickers in a grid; a click sends the sticker. Data comes from
    /// the web client's GraphQL queries (`Session::sticker_packs`).
    fn sticker_panel(&mut self, ctx: &egui::Context, bottom_left: Pos2, thread: i64, cmds: &mut Vec<Command>) {
        const W: f32 = 360.0;
        const CELL: f32 = 84.0;
        let (packs, loading_tray) = {
            let st = self.shared.stickers.lock().unwrap();
            (st.packs.clone(), st.loading.contains(""))
        };
        if packs.is_none() && !loading_tray {
            cmds.push(Command::LoadStickerPacks);
        }
        let packs = packs.unwrap_or_default();
        if self.sticker_pack.as_ref().is_none_or(|p| !packs.iter().any(|x| &x.id == p)) {
            self.sticker_pack = packs.first().map(|p| p.id.clone());
        }
        let (stickers, loading_pack) = match &self.sticker_pack {
            Some(id) => {
                let st = self.shared.stickers.lock().unwrap();
                (st.pack_stickers.get(id).cloned(), st.loading.contains(id))
            }
            None => (None, false),
        };
        if let (Some(id), None, false) = (&self.sticker_pack, &stickers, loading_pack) {
            cmds.push(Command::LoadStickerPack { pack: id.clone() });
        }
        let mut close = ctx.input(|i| i.key_pressed(Key::Escape));
        // The panel is its own borderless, transparent window with native
        // Liquid Glass behind it (`macos::glass_window`): an egui popup can't
        // be glass, since the glass sits behind egui and the messages under
        // the popup would cover it. It sits where the popup was, above the
        // composer's left edge.
        let size = Vec2::new(W + 20.0, 440.0);
        let local = Rect::from_min_size(Pos2::new(bottom_left.x, bottom_left.y - size.y), size);
        let origin = ctx.input(|i| i.viewport().inner_rect.map(|r| r.min)).unwrap_or_default();
        let builder = egui::ViewportBuilder::default()
            .with_title(crate::macos::STICKER_WINDOW)
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(false)
            .with_taskbar(false)
            .with_position(origin + local.min.to_vec2())
            .with_inner_size(size);
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("sticker_panel"), builder, |pctx, _| {
            if pctx.input(|i| i.key_pressed(Key::Escape)) {
                close = true;
            }
            crate::macos::glass_window(crate::macos::STICKER_WINDOW, 16.0);
            let frame = Frame::NONE.stroke(Stroke::new(1.0, pal().border)).corner_radius(CornerRadius::same(16)).inner_margin(Margin::same(10));
            egui::CentralPanel::default().frame(frame).show(pctx, |ui| {
                ui.set_width(W);
                if self.sticker_store {
                    self.sticker_store_view(ui, thread, cmds, &mut close);
                    return;
                }
                // Pack tabs: each pack's thumbnail, the selected one underlined
                // (scrolling sideways), then the store button.
                ui.horizontal(|ui| {
                    let store_w = 40.0;
                    let tabs_w = ui.available_width() - store_w;
                    ScrollArea::horizontal().id_salt("sticker_tabs").max_width(tabs_w).scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for p in &packs {
                        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(38.0), Sense::click());
                        let selected = self.sticker_pack.as_deref() == Some(p.id.as_str());
                        if selected || resp.hovered() {
                            ui.painter().rect_filled(rect, CornerRadius::same(8), pal().selected);
                        }
                        if p.thumbnail.starts_with("http") {
                            egui::Image::new(p.thumbnail.as_str()).paint_at(ui, rect.shrink(4.0));
                        }
                        if selected {
                            ui.painter().line_segment([rect.left_bottom() + Vec2::new(8.0, 1.0), rect.right_bottom() + Vec2::new(-8.0, 1.0)], Stroke::new(2.0, pal().blue));
                        }
                        if resp.on_hover_text(&p.name).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            self.sticker_pack = Some(p.id.clone());
                        }
                    }
                    });
                    if plus_button(ui).on_hover_text("Sticker Store").clicked() {
                        self.sticker_store = true;
                        self.store_preview = None;
                    }
                });
                ui.separator();
                ScrollArea::vertical().id_salt("sticker_grid").max_height(340.0).auto_shrink([false, false]).show(ui, |ui| {
                    sticker_grid(ui, &stickers, thread, cmds, &mut close);
                });
            });
        });
        // Clicking in the main window (except on the composer row, where the
        // sticker button is) closes it.
        let pointer = ctx.input(|i| i.pointer.interact_pos());
        if ctx.input(|i| i.pointer.any_pressed()) && pointer.is_some_and(|p| !local.contains(p) && p.y < bottom_left.y) {
            close = true;
        }
        if close {
            self.show_stickers = false;
            self.sticker_store = false;
            self.store_preview = None;
        }
    }

    /// The sticker store inside the sticker panel: every pack as a card
    /// (banner, name, artist, preview and add/remove), loading more as you
    /// scroll; a preview shows the pack's stickers, which can be sent.
    fn sticker_store_view(&mut self, ui: &mut egui::Ui, thread: i64, cmds: &mut Vec<Command>, close: &mut bool) {
        let (store, loading, has_more, changing, tray) = {
            let st = self.shared.stickers.lock().unwrap();
            (st.store.clone(), st.loading.contains("store"), st.store_next.is_some(), st.tray_changing.clone(), st.packs.clone().unwrap_or_default())
        };
        if store.is_none() && !loading {
            cmds.push(Command::LoadStickerStore { more: false });
        }
        let in_tray = |id: &str, listed: bool| tray.iter().any(|p| p.id == id) || listed;

        // Header: back (from a preview), title, done.
        ui.horizontal(|ui| {
            ui.set_height(32.0);
            if self.store_preview.is_some() {
                if chevron_button(ui).on_hover_text("Back").clicked() {
                    self.store_preview = None;
                }
            }
            let title = self.store_preview.as_ref().map_or("Sticker Store".to_string(), |(_, name)| name.clone());
            ui.add(egui::Label::new(sf(&title, 16.0, 700)).truncate());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(egui::Button::new(sf("Done", 13.0, 600).color(pal().blue)).frame(false)).clicked() {
                    self.sticker_store = false;
                    self.store_preview = None;
                }
            });
        });
        ui.add_space(4.0);

        if let Some((id, _)) = self.store_preview.clone() {
            let listed = store.as_ref().and_then(|l| l.iter().find(|p| p.id == id)).is_some_and(|p| p.in_tray);
            let added = in_tray(&id, listed);
            let busy = changing.contains(&id);
            let label = if busy { "…" } else if added { "Remove from tray" } else { "Add to tray" };
            let fill = if added { pal().input } else { pal().blue };
            let ink = if added { pal().text } else { Color32::WHITE };
            let b = egui::Button::new(sf(label, 13.0, 600).color(ink)).fill(fill).corner_radius(CornerRadius::same(16)).min_size(Vec2::new(ui.available_width(), 32.0));
            if ui.add_enabled(!busy, b).clicked() {
                cmds.push(Command::StickerPackTray { pack: id.clone(), add: !added });
            }
            ui.add_space(6.0);
            let (stickers, loading_pack) = {
                let st = self.shared.stickers.lock().unwrap();
                (st.pack_stickers.get(&id).cloned(), st.loading.contains(&id))
            };
            if stickers.is_none() && !loading_pack {
                cmds.push(Command::LoadStickerPack { pack: id.clone() });
            }
            ScrollArea::vertical().id_salt(("store_pack", &id)).max_height(320.0).auto_shrink([false, false]).show(ui, |ui| {
                sticker_grid(ui, &stickers, thread, cmds, close);
            });
            return;
        }

        ScrollArea::vertical().id_salt("sticker_store").max_height(360.0).auto_shrink([false, false]).show(ui, |ui| {
            let Some(list) = &store else {
                ui.add_space(40.0);
                ui.vertical_centered(|ui| ui.spinner());
                return;
            };
            if list.is_empty() && !loading {
                ui.add_space(40.0);
                ui.vertical_centered(|ui| ui.label(sf("The sticker store couldn't load", 13.0, 500).color(pal().muted)));
                return;
            }
            let w = ui.available_width();
            for p in list {
                Frame::default().fill(pal().quote).corner_radius(CornerRadius::same(14)).inner_margin(Margin::same(8)).show(ui, |ui| {
                    ui.set_width(w - 16.0);
                    if p.preview.starts_with("http") {
                        let (rect, resp) = ui.allocate_exact_size(Vec2::new(w - 16.0, 96.0), Sense::click());
                        if ui.is_rect_visible(rect) {
                            media_image(ui.ctx(), &p.preview).fit_to_exact_size(rect.size()).maintain_aspect_ratio(true).paint_at(ui, rect);
                        }
                        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            self.store_preview = Some((p.id.clone(), p.name.clone()));
                        }
                    }
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.set_width(w - 16.0 - 84.0);
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.add(egui::Label::new(sf(&p.name, 13.0, 600)).truncate());
                            ui.add(egui::Label::new(sf(&p.artist, 11.0, 400).color(pal().muted)).truncate());
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let added = in_tray(&p.id, p.in_tray);
                            let busy = changing.contains(&p.id);
                            if circle_action(ui, if added { "checkmark" } else { "plus" }, added, busy)
                                .on_hover_text(if added { "Remove from tray" } else { "Add to tray" })
                                .clicked()
                                && !busy
                            {
                                cmds.push(Command::StickerPackTray { pack: p.id.clone(), add: !added });
                            }
                            if circle_action(ui, "eye", true, false).on_hover_text("Preview").clicked() {
                                self.store_preview = Some((p.id.clone(), p.name.clone()));
                            }
                        });
                    });
                });
                ui.add_space(6.0);
            }
            if loading {
                ui.vertical_centered(|ui| ui.spinner());
            } else if has_more {
                // Load the next page when the end of the list scrolls into view.
                let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 1.0), Sense::hover());
                if ui.is_rect_visible(rect) {
                    cmds.push(Command::LoadStickerStore { more: true });
                }
            }
        });
    }

    fn message_menu(&mut self, ctx: &egui::Context, store: &Store, thread: &Thread, m: &Message, mine: bool, cmds: &mut Vec<Command>) {
        use crate::macos::MenuEntry;
        let mut entries = Vec::new();
        // Encrypted chats can only send plain text so far: no reactions, replies or edits.
        if !thread.e2ee {
            let my_reaction = m.reactions.get(&store.me);
            let mut row: Vec<(String, bool)> = QUICK_REACTIONS.iter().map(|e| (e.to_string(), my_reaction.is_some_and(|x| x == e))).collect();
            row.push(("+".into(), false));
            entries.push(MenuEntry::Palette(row));
            entries.push(MenuEntry::Separator);
        }
        // What each further entry does, in order after the palette.
        enum Act {
            Reply,
            Copy,
            Edit,
            Unsend,
        }
        let mut acts = Vec::new();
        let mut add = |entries: &mut Vec<MenuEntry>, title: &str, symbol: &'static str, act: Act| {
            entries.push(MenuEntry::Item { title: title.into(), symbol });
            acts.push(act);
        };
        if !thread.e2ee {
            add(&mut entries, "Reply", "arrowshape.turn.up.left", Act::Reply);
        }
        if !m.text.is_empty() {
            add(&mut entries, "Copy", "doc.on.doc", Act::Copy);
        }
        let editable = !thread.e2ee && mine && !m.unsent && !m.text.is_empty() && now_ms() - m.timestamp_ms < 15 * 60_000;
        if editable {
            add(&mut entries, "Edit", "pencil", Act::Edit);
        }
        if !thread.e2ee && mine && !m.unsent {
            entries.push(MenuEntry::Separator);
            add(&mut entries, "Unsend for Everyone", "trash", Act::Unsend);
        }
        let Some(choice) = crate::macos::popup_menu(&entries) else { return };
        let reactions = if thread.e2ee { 0 } else { QUICK_REACTIONS.len() + 1 };
        let react = |e: &str, cmds: &mut Vec<Command>| {
            // Choosing your current reaction again removes it.
            let emoji = if m.reactions.get(&store.me).is_some_and(|x| x == e) { String::new() } else { e.to_string() };
            cmds.push(Command::React { thread: thread.key, message_id: m.id.clone(), emoji });
        };
        if choice < QUICK_REACTIONS.len() && reactions > 0 {
            react(QUICK_REACTIONS[choice], cmds);
            return;
        }
        if choice == QUICK_REACTIONS.len() && reactions > 0 {
            // "+": the web-style picker (search, your reactions, categories).
            self.reaction_picker = Some(ReactionPicker {
                thread: thread.key,
                message_id: m.id.clone(),
                anchor: ctx.input(|i| i.pointer.latest_pos()).unwrap_or_default(),
                query: String::new(),
                jump_to: None,
                opened: true,
            });
            return;
        }
        match acts.get(choice - reactions) {
            Some(Act::Reply) => {
                self.reply_to = Some((m.id.clone(), m.text.clone()));
                self.editing = None;
                self.focus_composer = true;
            }
            Some(Act::Copy) => ctx.copy_text(m.text.clone()),
            Some(Act::Edit) => {
                self.editing = Some(m.id.clone());
                self.reply_to = None;
                self.draft = m.text.clone();
                self.focus_composer = true;
            }
            Some(Act::Unsend) => cmds.push(Command::Unsend { thread: thread.key, message_id: m.id.clone() }),
            None => {}
        }
    }

    fn bubble(&self, ui: &mut egui::Ui, m: &Message, mine: bool, first: bool, last: bool) -> egui::Response {
        let fill = if mine { pal().blue } else { pal().bubble_other };
        let radius = group_corners(mine, first, last);
        let mut parts: Vec<egui::Response> = Vec::new();

        if m.unsent {
            parts.push(
                Frame::default()
                    .stroke(Stroke::new(1.0, pal().border))
                    .corner_radius(radius)
                    .inner_margin(Margin::symmetric(12, 8))
                    .show(ui, |ui| ui.label(sf(if mine { "You unsent a message" } else { "Message unsent" }, 15.0, 400).italics().color(pal().muted)))
                    .response,
            );
            return parts.pop().unwrap();
        }

        for a in &m.attachments {
            parts.push(attachment(ui, a, fill, radius));
        }

        if m.attachments.is_empty() && m.sticker_id.is_some() && m.text.is_empty() {
            parts.push(ui.label(sf("Sticker", 14.0, 400).italics().color(pal().muted)));
        }

        // A message that is only a link, shown as a preview card already:
        // the card says it all, so skip the bare URL under it.
        let bare_link = {
            let t = m.text.trim();
            (t.starts_with("http://") || t.starts_with("https://")) && !t.contains(char::is_whitespace)
        };
        let has_card = m.attachments.iter().any(|a| matches!(a.kind, AttachmentKind::Link) || !a.title.is_empty());
        if !m.text.is_empty() && !(bare_link && has_card) {
            let big_emoji = is_big_emoji(&m.text);
            let r = if big_emoji {
                ui.label(RichText::new(&m.text).size(40.0))
            } else {
                Frame::default()
                    .fill(fill)
                    .corner_radius(radius)
                    .inner_margin(Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        // White only on the blue of own messages; others use the theme's ink.
                        let ink = if mine { Color32::WHITE } else { pal().text };
                        ui.add(egui::Label::new(sf(&m.text, 15.0, 400).color(ink)).wrap());
                        if m.edited {
                            ui.label(sf("Edited", 11.0, 400).color(if mine { Color32::from_white_alpha(170) } else { pal().muted }));
                        }
                    })
                    .response
            };
            parts.push(r);
        }

        if parts.is_empty() {
            parts.push(
                Frame::default()
                    .stroke(Stroke::new(1.0, pal().border))
                    .corner_radius(radius)
                    .inner_margin(Margin::symmetric(12, 8))
                    .show(ui, |ui| ui.label(sf("This message isn't supported yet", 14.0, 400).italics().color(pal().muted)))
                    .response,
            );
        }
        parts.into_iter().reduce(|a, b| a.union(b)).unwrap()
    }

    fn composer(&mut self, ui: &mut egui::Ui, store: &Store, thread: &Thread, cmds: &mut Vec<Command>) {
        let key = thread.key;
        let banner = |ui: &mut egui::Ui, title: String, body: &str| -> bool {
            let mut close = false;
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(sf(&title, 13.0, 600));
                    if !body.is_empty() {
                        ui.add(egui::Label::new(sf(&truncate(body, 90), 12.0, 400).color(pal().muted)).truncate());
                    }
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    close = ui.add(egui::Button::new(sf("✕", 14.0, 600).color(pal().muted)).frame(false)).clicked();
                });
            });
            ui.add_space(6.0);
            close
        };
        if let Some((id, text)) = self.reply_to.clone() {
            let who = store
                .messages
                .get(&key)
                .and_then(|v| v.iter().find(|m| m.id == id))
                .map(|m| if m.sender_id == store.me { "yourself".to_string() } else { store.member_name(key, m.sender_id) })
                .unwrap_or_default();
            if banner(ui, format!("Replying to {who}"), &text) {
                self.reply_to = None;
            }
        }
        if self.editing.is_some() && banner(ui, "Editing message".into(), "") {
            self.editing = None;
            self.draft.clear();
        }
        if self.show_emoji {
            Frame::default().fill(pal().quote).corner_radius(CornerRadius::same(12)).inner_margin(Margin::same(8)).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for e in EMOJIS {
                        if ui.add(egui::Button::new(RichText::new(e).size(22.0)).frame(false)).clicked() {
                            self.draft.push_str(e);
                        }
                    }
                });
            });
            ui.add_space(6.0);
        }

        // Pasted or dropped attachments waiting to be sent: thumbnails with a
        // remove button, above the composer.
        if !self.pending_files.is_empty() {
            let mut remove = None;
            ui.horizontal(|ui| {
                ui.add_space(52.0);
                for (i, path) in self.pending_files.iter().enumerate() {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(64.0), Sense::hover());
                    let is_image = path.extension().and_then(|e| e.to_str()).is_some_and(|e| ["png", "jpg", "jpeg", "gif", "webp", "heic"].contains(&e.to_ascii_lowercase().as_str()));
                    if is_image {
                        egui::Image::new(format!("file://{}", path.display())).corner_radius(CornerRadius::same(10)).paint_at(ui, rect);
                    } else {
                        ui.painter().rect_filled(rect, CornerRadius::same(10), pal().quote);
                        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, truncate(&name, 10), egui::FontId::proportional(11.0), pal().muted);
                    }
                    let x = Rect::from_center_size(rect.right_top() + Vec2::new(-8.0, 8.0), Vec2::splat(18.0));
                    let close = ui.interact(x, ui.scope_id().with(("unattach", i)), Sense::click());
                    ui.painter().circle_filled(x.center(), 9.0, Color32::from_black_alpha(170));
                    let c = x.center();
                    ui.painter().line_segment([c + Vec2::splat(-3.5), c + Vec2::splat(3.5)], Stroke::new(1.4, Color32::WHITE));
                    ui.painter().line_segment([c + Vec2::new(-3.5, 3.5), c + Vec2::new(3.5, -3.5)], Stroke::new(1.4, Color32::WHITE));
                    if close.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        remove = Some(i);
                    }
                }
            });
            if let Some(i) = remove {
                self.pending_files.remove(i);
            }
            ui.add_space(6.0);
        }

        // Laid out by hand from one rect: egui's `horizontal` centers each item on
        // the row height *so far*, which left the buttons and the text off the
        // capsule's center line. Buttons sit on the bottom line's center (where
        // the single line is), so a growing multi-line draft keeps them low,
        // like Messenger.
        let font = egui::FontId::new(15.0 * text_scale(), egui::FontFamily::Name("sf-400".into()));
        let line = ui.fonts_mut(|f| f.row_height(&font));
        let text_h = self.composer_text_h.max(line);
        const PILL_MIN: f32 = 40.0;
        let pill_h = (text_h + 2.0 * ((PILL_MIN - line) / 2.0)).max(PILL_MIN);
        let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), pill_h), Sense::hover());
        let mid_y = row.bottom() - PILL_MIN / 2.0;
        let button = |cx: f32| Rect::from_center_size(Pos2::new(cx, mid_y), Vec2::splat(32.0));

        let clip = Some(button(row.left() + 16.0));
        // Stickers (not in encrypted chats: sending them there isn't supported).
        let sticker = Some(button(row.left() + 16.0 + 36.0));
        let send = button(row.right() - 16.0);
        let pill = Rect::from_min_max(
            Pos2::new(sticker.or(clip).map_or(row.left(), |c| c.right() + 8.0), row.top()),
            Pos2::new(send.left() - 8.0, row.bottom()),
        );
        // The input is a glass capsule (native pane behind it).
        let pill_radius = CornerRadius::same((PILL_MIN / 2.0) as u8);
        ui.painter().rect_filled(pill, pill_radius, pal().input);
        ui.painter().rect_stroke(pill, pill_radius, Stroke::new(1.0, pal().border), egui::StrokeKind::Inside);
        self.glass.push(glass_rect(pill, PILL_MIN / 2.0));

        if let Some(clip) = clip {
            if paperclip_button(ui, clip).on_hover_text("Attach files").clicked() {
                if let Some(paths) = rfd::FileDialog::new().set_title("Send files").pick_files() {
                    cmds.push(Command::SendFiles { thread: key, paths, reply_to: self.reply_to.take().map(|r| r.0) });
                }
            }
        }
        if let Some(rect) = sticker {
            let color = if self.show_stickers { pal().blue } else { pal().blue.gamma_multiply(0.85) };
            if ui.put(rect, |ui: &mut egui::Ui| symbol_button(ui, "face.smiling.inverse", color)).on_hover_text("Choose a sticker").clicked() {
                self.show_stickers = !self.show_stickers;
            }
        }
        if self.show_stickers {
            self.sticker_panel(ui.ctx(), Pos2::new(pill.left() - 40.0, pill.top() - 8.0), key, cmds);
        }
        let smiley = Rect::from_center_size(Pos2::new(pill.right() - 20.0, mid_y), Vec2::splat(32.0));
        if smiley_button(ui, smiley, self.show_emoji).on_hover_text("Choose an emoji").clicked() {
            self.show_emoji = !self.show_emoji;
        }

        // The text area: centered in the capsule, between the left padding and the smiley.
        let text_rect = Rect::from_min_max(
            Pos2::new(pill.left() + 16.0, pill.center().y - text_h / 2.0),
            Pos2::new(smiley.left() - 4.0, pill.center().y + text_h / 2.0),
        );
        // @mentions (groups): a picker above the composer while the draft ends
        // in "@query". Keys are handled before the text field sees them, so
        // Enter/Tab pick a member instead of sending.
        let text_id = ui.make_persistent_id(("composer_text", key));
        let candidates: Vec<(i64, String)> = match mention_query(&self.draft) {
            Some((_, query)) if thread.is_group() => {
                let query = fold_name(query);
                thread
                    .participants
                    .iter()
                    .filter(|p| **p != store.me)
                    .map(|p| (*p, store.member_name(key, *p)))
                    .filter(|(_, name)| fold_name(name).contains(&query))
                    .take(6)
                    .collect()
            }
            _ => Vec::new(),
        };
        let mut picked: Option<(i64, String)> = None;
        if !candidates.is_empty() {
            self.mention_sel = self.mention_sel.min(candidates.len() - 1);
            if ui.memory(|m| m.has_focus(text_id)) {
                ui.input_mut(|i| {
                    if i.consume_key(egui::Modifiers::NONE, Key::ArrowDown) {
                        self.mention_sel = (self.mention_sel + 1) % candidates.len();
                    }
                    if i.consume_key(egui::Modifiers::NONE, Key::ArrowUp) {
                        self.mention_sel = (self.mention_sel + candidates.len() - 1) % candidates.len();
                    }
                    if i.consume_key(egui::Modifiers::NONE, Key::Enter) || i.consume_key(egui::Modifiers::NONE, Key::Tab) {
                        picked = Some(candidates[self.mention_sel].clone());
                    }
                });
            }
            egui::Area::new(ui.scope_id().with("mention_picker"))
                .order(egui::Order::Foreground)
                .pivot(egui::Align2::LEFT_BOTTOM)
                .fixed_pos(Pos2::new(pill.left() + 8.0, pill.top() - 6.0))
                .show(ui.ctx(), |ui| {
                    Frame::default().fill(pal().popup).stroke(Stroke::new(1.0, pal().border)).corner_radius(CornerRadius::same(12)).inner_margin(Margin::same(6)).show(ui, |ui| {
                        ui.set_width(260.0);
                        for (i, (id, name)) in candidates.iter().enumerate() {
                            let (row, resp) = ui.allocate_exact_size(Vec2::new(260.0, 34.0), Sense::click());
                            if i == self.mention_sel || resp.hovered() {
                                ui.painter().rect_filled(row, CornerRadius::same(8), pal().selected);
                            }
                            let pic = store.contacts.get(id).map(|c| c.picture.clone()).unwrap_or_default();
                            paint_avatar(ui, Rect::from_center_size(Pos2::new(row.left() + 18.0, row.center().y), Vec2::splat(24.0)), &pic, name);
                            ui.painter().text(Pos2::new(row.left() + 38.0, row.center().y), egui::Align2::LEFT_CENTER, name, egui::FontId::new(14.0, egui::FontFamily::Name("sf-500".into())), pal().text);
                            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                picked = Some((*id, name.clone()));
                            }
                        }
                    });
                });
        } else {
            self.mention_sel = 0;
        }
        if let Some((id, name)) = picked {
            // Replace "@query" with "@Name": Messenger keeps the "@" in the text
            // (received tags read "@Truc Dinh") and the tag covers it.
            if let Some((at, _)) = mention_query(&self.draft) {
                let tag = format!("@{name}");
                self.draft.truncate(at);
                self.draft.push_str(&tag);
                self.draft.push(' ');
                self.mention_picks.push((id, tag));
                if let Some(mut state) = TextEdit::load_state(ui.ctx(), text_id) {
                    let end = egui::text::CCursor::new(self.draft.chars().count());
                    state.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
                    state.store(ui.ctx(), text_id);
                }
                ui.memory_mut(|m| m.request_focus(text_id));
            }
        }

        // Cmd+V with a picture or Finder files on the clipboard attaches them
        // (egui only pastes text; it turns an image-only clipboard into
        // `PasteImage`). Handled before the text field sees the event.
        if ui.memory(|m| m.has_focus(text_id)) {
            let pasted_image = ui.input_mut(|i| {
                let mut found = None;
                i.events.retain(|e| match e {
                    egui::Event::PasteImage(img) => {
                        found = Some(img.clone());
                        false
                    }
                    _ => true,
                });
                found
            });
            if let Some(img) = pasted_image {
                match save_pasted_image(&img) {
                    Ok(path) => self.pending_files.push(path),
                    Err(e) => *self.shared.error.lock().unwrap() = Some(format!("Couldn't paste the image: {e}")),
                }
            }
            if ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Paste(_)))) {
                let files = crate::macos::pasteboard_files();
                if !files.is_empty() {
                    // Files copied in Finder: attach them instead of pasting their names.
                    ui.input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Paste(_))));
                    self.pending_files.extend(files);
                }
            }
        }

        let mut text_ui = ui.new_child(egui::UiBuilder::new().max_rect(text_rect).layout(Layout::top_down(Align::Min)));
        // Tags show in the accent color while composing.
        let tags: Vec<String> = self.mention_picks.iter().map(|(_, t)| t.clone()).collect();
        let tag_font = font.clone();
        let mut layouter = move |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
            let text = buf.as_str();
            let mut job = egui::text::LayoutJob::default();
            let plain = egui::TextFormat::simple(tag_font.clone(), pal().text);
            let tagged = egui::TextFormat::simple(tag_font.clone(), pal().blue);
            let mut pos = 0;
            while pos < text.len() {
                let next = tags.iter().filter_map(|t| text[pos..].find(t.as_str()).map(|i| (pos + i, t.len()))).min();
                match next {
                    Some((start, len)) => {
                        job.append(&text[pos..start], 0.0, plain.clone());
                        job.append(&text[start..start + len], 0.0, tagged.clone());
                        pos = start + len;
                    }
                    None => {
                        job.append(&text[pos..], 0.0, plain.clone());
                        pos = text.len();
                    }
                }
            }
            job.wrap.max_width = wrap_width;
            ui.fonts_mut(|f| f.layout_job(job))
        };
        let resp = text_ui.add(
            TextEdit::multiline(&mut self.draft)
                .id(text_id)
                .layouter(&mut layouter)
                .hint_text(sf("Aa", 15.0, 400).color(pal().muted))
                .font(font)
                .frame(Frame::NONE)
                .desired_rows(1)
                .desired_width(text_rect.width())
                .margin(Margin::ZERO)
                .return_key(None),
        );
        // Reply/Edit from the message menu: type right away.
        if std::mem::take(&mut self.focus_composer) {
            resp.request_focus();
        }
        // Height for the next frame's layout (grows with the draft's lines).
        let measured = resp.rect.height().min(160.0);
        if (measured - self.composer_text_h).abs() > 0.5 {
            self.composer_text_h = measured;
            ui.ctx().request_repaint();
        }

        // Typing indicator: refresh every 3s while there's text; clear when emptied.
        if resp.changed() && !thread.e2ee {
            let typing = !self.draft.trim().is_empty();
            let stale = self.typing_sent.is_none_or(|(k, t)| k != key || t.elapsed().as_secs() >= 3);
            if typing && stale {
                cmds.push(Command::Typing { thread: key, is_group: thread.is_group(), typing: true });
                self.typing_sent = Some((key, Instant::now()));
            } else if !typing && self.typing_sent.is_some() {
                cmds.push(Command::Typing { thread: key, is_group: thread.is_group(), typing: false });
                self.typing_sent = None;
            }
        }

        let enter = resp.has_focus() && ui.input(|i| i.key_pressed(Key::Enter) && !i.modifiers.shift);
        let empty = self.draft.trim().is_empty() && self.pending_files.is_empty();
        // The chat's own quick emoji (set per group, like the web client), else 👍.
        let quick = if thread.emoji.is_empty() { "👍" } else { thread.emoji.as_str() };
        let send_button = egui::Button::new(RichText::new(if empty { quick } else { "➤" }).size(20.0).color(pal().blue)).frame(false);
        let clicked = ui.put(send, send_button).on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
        if (enter || clicked) && !self.pending_files.is_empty() {
            // Attachments first, then any text as its own message.
            let paths = std::mem::take(&mut self.pending_files);
            cmds.push(Command::SendFiles { thread: key, paths, reply_to: None });
        }
        if enter || clicked {
            let text = self.draft.trim().to_string();
            let text = if text.is_empty() && clicked && empty { quick.to_string() } else { text };
            if !text.is_empty() {
                if let Some(id) = self.editing.take() {
                    cmds.push(Command::Edit { message_id: id, text });
                } else {
                    let mentions = locate_mentions(&text, &self.mention_picks);
                    cmds.push(Command::SendText { thread: key, text, reply_to: self.reply_to.take().map(|r| r.0), mentions });
                }
                if self.typing_sent.take().is_some() {
                    cmds.push(Command::Typing { thread: key, is_group: thread.is_group(), typing: false });
                }
            }
            self.draft.clear();
            self.mention_picks.clear();
            resp.request_focus();
        }

        // Drag & drop files onto the window to send them.
        let dropped: Vec<_> = ui.ctx().input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        if !dropped.is_empty() {
            cmds.push(Command::SendFiles { thread: key, paths: dropped, reply_to: self.reply_to.take().map(|r| r.0) });
        }
    }

    fn info_panel(&mut self, ui: &mut egui::Ui, store: &Store, key: i64, cmds: &mut Vec<Command>) {
        let Some(t) = store.threads.get(&key) else { return };
        let title = store.thread_title(t);
        let strip = ui.allocate_space(Vec2::new(ui.available_width(), TITLEBAR)).1;
        titlebar_drag(ui, strip);
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(8.0);
                thread_avatar(ui, store, t, &title, 72.0);
                ui.add_space(6.0);
                ui.label(sf(&title, 17.0, 600));
                ui.add_space(12.0);
            });
            // Round action buttons, centered as a group.
            let actions: Vec<(&str, &str)> = if t.thread_type == 1 {
                vec![("👤", "Profile"), (if t.muted { "🔔" } else { "🔕" }, if t.muted { "Unmute" } else { "Mute" })]
            } else {
                vec![(if t.muted { "🔔" } else { "🔕" }, if t.muted { "Unmute" } else { "Mute" })]
            };
            let group_w = actions.len() as f32 * 72.0;
            ui.horizontal(|ui| {
                ui.add_space(((ui.available_width() - group_w) / 2.0).max(0.0));
                for (icon, label) in actions {
                    if round_action(ui, icon, label) {
                        match label {
                            "Profile" => ui.ctx().open_url(egui::OpenUrl::new_tab(format!("https://www.facebook.com/{key}"))),
                            _ => cmds.push(Command::Mute { thread: key, minutes: if t.muted { None } else { Some(-1) } }),
                        }
                    }
                }
            });
            ui.add_space(16.0);

            if t.is_group() {
                egui::CollapsingHeader::new(sf(&format!("Chat members ({})", t.participants.len()), 15.0, 600)).default_open(true).show(ui, |ui| {
                    for p in &t.participants {
                        ui.horizontal(|ui| {
                            let pic = store.contacts.get(p).map(|c| c.picture.clone()).unwrap_or_default();
                            let name = store.member_name(t.key, *p);
                            avatar(ui, &pic, &name, 36.0);
                            ui.label(sf(&name, 15.0, 500));
                        });
                        ui.add_space(2.0);
                    }
                });
                ui.add_space(6.0);
            }
            egui::CollapsingHeader::new(sf("Media, files and links", 15.0, 600)).default_open(true).show(ui, |ui| {
                let all: Vec<Attachment> = store
                    .messages
                    .get(&key)
                    .map(|v| v.iter().rev().flat_map(|m| m.attachments.iter()).cloned().collect())
                    .unwrap_or_default();
                let media: Vec<_> = all.iter().filter(|a| matches!(a.kind, AttachmentKind::Image | AttachmentKind::Video)).take(30).collect();
                let other: Vec<_> = all.iter().filter(|a| matches!(a.kind, AttachmentKind::File | AttachmentKind::Link | AttachmentKind::Audio)).take(20).collect();
                if media.is_empty() && other.is_empty() {
                    ui.label(sf("Nothing shared yet", 14.0, 400).color(pal().muted));
                }
                let cell = ((ui.available_width() - 8.0) / 3.0).floor();
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::splat(4.0);
                    for a in media {
                        let r = ui.add(egui::Image::new(a.preview_url.as_str()).fit_to_exact_size(Vec2::splat(cell)).corner_radius(CornerRadius::same(6)).sense(Sense::click()));
                        if r.clicked() {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(&a.url));
                        }
                    }
                });
                for a in other {
                    if ui.link(sf(&truncate(&link_title(a), 36), 14.0, 400)).clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(&a.url));
                    }
                }
            });
        });
    }

    fn notify(&mut self, store: &mut Store) {
        let incoming = std::mem::take(&mut store.new_incoming);
        for (thread, mid) in incoming {
            let Some(t) = store.threads.get(&thread) else { continue };
            if t.muted || (self.focused && self.selected == Some(thread)) {
                continue;
            }
            let Some(m) = store.messages.get(&thread).and_then(|v| v.iter().find(|m| m.id == mid)) else { continue };
            let body = if m.text.is_empty() { "Sent an attachment".to_string() } else { m.text.clone() };
            let title = if t.is_group() {
                format!("{} — {}", store.member_name(thread, m.sender_id), store.thread_title(t))
            } else {
                store.thread_title(t)
            };
            crate::macos::notify(&title, &truncate(&body, 200), thread);
        }
    }

    fn update_dock_badge(&self, store: &Store) {
        let unread = store.threads.values().filter(|t| t.unread() && !t.muted).count();
        crate::macos::set_dock_badge(unread);
    }
}

/// Shown instead of the message list for end-to-end encrypted chats.
/// Returns true when "Restore with PIN" was clicked.
fn e2ee_notice(ui: &mut egui::Ui, key: i64) -> bool {
    let mut clicked = false;
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() / 2.0 - 110.0).max(24.0));
        ui.label(RichText::new("🔒").size(44.0).color(pal().muted));
        ui.add_space(8.0);
        ui.label(sf("End-to-end encrypted chat", 17.0, 600));
        ui.add_space(4.0);
        ui.add(egui::Label::new(sf("New messages in this chat appear here as they arrive.\nRestore older history with your Messenger PIN.", 14.0, 400).color(pal().muted)).wrap());
        ui.add_space(14.0);
        let restore = egui::Button::new(sf("Restore with PIN", 15.0, 600).color(Color32::WHITE)).fill(pal().blue).corner_radius(CornerRadius::same(18)).min_size(Vec2::new(0.0, 36.0));
        clicked = ui.add(restore).clicked();
        ui.add_space(6.0);
        if ui.add(egui::Button::new(sf("Open in browser", 13.0, 500).color(pal().blue)).frame(false)).clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab(format!("https://www.facebook.com/messages/e2ee/t/{key}")));
        }
    });
    clicked
}

/// "↩ A replied to B" plus the quoted message, shown above a reply.
fn reply_header(ui: &mut egui::Ui, store: &Store, thread: &Thread, m: &Message, reply_id: &str, mine: bool) {
    let original = store.messages.get(&thread.key).and_then(|v| v.iter().find(|x| x.id == reply_id));
    let sender = if mine { "You".to_string() } else { store.member_name(thread.key, m.sender_id) };
    // The original's author: from the loaded message, else from the reply's own
    // metadata (replyToUserId), so it's named even before history loads.
    let target = match original.map(|o| o.sender_id).or(m.reply_to_user) {
        Some(id) if id == m.sender_id => "themself".to_string(),
        Some(id) if id == store.me => "you".to_string(),
        Some(id) => store.member_name(thread.key, id),
        None => "a message".to_string(),
    };
    let quoted = match original {
        Some(o) if !o.text.is_empty() => o.text.clone(),
        Some(o) if !o.attachments.is_empty() => "Attachment".to_string(),
        _ if !m.reply_snippet.is_empty() => m.reply_snippet.clone(),
        _ => "Original message".to_string(),
    };
    // A drawn reply arrow (an emoji ↩ renders as a colored box), then the text.
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let (rect, _) = ui.allocate_exact_size(Vec2::new(12.0, 12.0), Sense::hover());
        let stroke = Stroke::new(1.3, pal().muted);
        let c = rect.center();
        let pts = vec![c + Vec2::new(4.5, 4.0), c + Vec2::new(4.5, 0.5), c + Vec2::new(3.0, -1.5), c + Vec2::new(-4.0, -1.5)];
        ui.painter().add(egui::Shape::line(pts, stroke));
        ui.painter().line_segment([c + Vec2::new(-4.0, -1.5), c + Vec2::new(-1.0, -4.5)], stroke);
        ui.painter().line_segment([c + Vec2::new(-4.0, -1.5), c + Vec2::new(-1.0, 1.5)], stroke);
        ui.label(sf(&format!("{sender} replied to {target}"), 12.0, 400).color(pal().muted));
    });
    Frame::default().fill(pal().quote).corner_radius(CornerRadius::same(18)).inner_margin(Margin { left: 12, right: 12, top: 8, bottom: 14 }).show(ui, |ui| {
        ui.add(egui::Label::new(sf(&truncate(&quoted, 140), 14.0, 400).color(pal().muted)).wrap());
    });
    ui.add_space(-10.0);
}

fn attachment(ui: &mut egui::Ui, a: &Attachment, fill: Color32, radius: CornerRadius) -> egui::Response {
    match a.kind {
        AttachmentKind::Image | AttachmentKind::Sticker | AttachmentKind::Video => {
            let (w, h) = if a.width > 0.0 && a.height > 0.0 { (a.width, a.height) } else { (240.0, 240.0) };
            let max = if matches!(a.kind, AttachmentKind::Sticker) { 120.0 } else { 300.0 };
            let scale = (max / w).min(max / h).min(1.0).max(0.0);
            let size = Vec2::new((w * scale).max(80.0), (h * scale).max(80.0));
            let image = media_image(ui.ctx(), &a.preview_url).fit_to_exact_size(size).corner_radius(radius).sense(Sense::click());
            let loading = matches!(image.load_for_size(ui.ctx(), size), Ok(egui::load::TexturePoll::Pending { .. }));
            // egui's own spinner fills the whole picture; while it loads show
            // a quiet placeholder with a small spinner instead.
            let r = ui.add(image.show_loading_spinner(false));
            if loading {
                ui.painter().rect_filled(r.rect, radius, pal().quote);
                egui::Spinner::new().size(18.0).color(pal().muted).paint_at(ui, Rect::from_center_size(r.rect.center(), Vec2::splat(18.0)));
            }
            if matches!(a.kind, AttachmentKind::Video) {
                ui.painter().circle_filled(r.rect.center(), 24.0, Color32::from_black_alpha(150));
                ui.painter().text(r.rect.center() + Vec2::new(2.0, 0.0), egui::Align2::CENTER_CENTER, "▶", egui::FontId::proportional(22.0), Color32::WHITE);
            }
            // Opens the full media (or, for shares without a direct link, the
            // preview); videos play in the app's player window. The bubble's own
            // click sense (for its menu) sits on top, so test the pointer
            // directly instead of `r.clicked()`.
            let target = if a.url.is_empty() { &a.preview_url } else { &a.url };
            // (Raw pointer input passes through overlays: ignore it while the
            // image viewer is open.)
            let viewer_open = ui.ctx().data(|d| d.get_temp::<Option<String>>(egui::Id::new(IMAGE_VIEWER))).flatten().is_some();
            let hovered = !viewer_open && ui.rect_contains_pointer(r.rect);
            if hovered && !target.is_empty() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                if ui.input(|i| i.pointer.primary_clicked()) {
                    if matches!(a.kind, AttachmentKind::Video) {
                        crate::macos::play_video(target, &a.title, a.width, a.height);
                    } else if matches!(a.kind, AttachmentKind::Image) {
                        let full = if a.url.starts_with("http") || a.url.starts_with("file://") { a.url.clone() } else { a.preview_url.clone() };
                        ui.ctx().data_mut(|d| d.insert_temp(egui::Id::new(IMAGE_VIEWER), Some(full)));
                        ui.ctx().request_repaint();
                    } else {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(target));
                    }
                }
            }
            r
        }
        // A shared post or reel with a picture: the picture, then title and source.
        AttachmentKind::Link if a.preview_url.starts_with("http") && a.url.is_empty() => {
            // Fit the picture in 280×360 keeping its aspect ratio (portrait reel
            // thumbnails would otherwise be squashed); the card is as wide as it.
            let (w, h) = if a.width > 0.0 && a.height > 0.0 { (a.width, a.height) } else { (280.0, 160.0) };
            let scale = (280.0 / w).min(360.0 / h);
            let size = Vec2::new(w * scale, h * scale);
            let r = Frame::default()
                .fill(pal().bubble_other)
                .corner_radius(radius)
                .show(ui, |ui| {
                    ui.set_width(size.x);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let top = CornerRadius { nw: radius.nw, ne: radius.ne, sw: 0, se: 0 };
                    ui.add(egui::Image::new(a.preview_url.as_str()).fit_to_exact_size(size).corner_radius(top));
                    Frame::default().inner_margin(Margin { left: 12, right: 12, top: 8, bottom: 10 }).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        if !a.title.is_empty() {
                            ui.add(egui::Label::new(sf(&truncate(&a.title, 110), 14.0, 600).color(pal().text)).wrap());
                        }
                        if !a.subtitle.is_empty() {
                            ui.add(egui::Label::new(sf(&a.subtitle, 12.0, 400).color(pal().muted)).truncate());
                        }
                    });
                })
                .response
                .interact(Sense::click());
            if r.clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(&a.preview_url));
            }
            r.on_hover_cursor(egui::CursorIcon::PointingHand)
        }
        AttachmentKind::Audio | AttachmentKind::File | AttachmentKind::Link => {
            let icon = match a.kind {
                AttachmentKind::Audio => "🎤",
                AttachmentKind::Link => "🔗",
                _ => "📄",
            };
            let r = Frame::default()
                .fill(if matches!(a.kind, AttachmentKind::Link) { pal().bubble_other } else { fill })
                .corner_radius(radius)
                .inner_margin(Margin::symmetric(12, 10))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    let has_preview = a.preview_url.starts_with("http") && a.preview_url != a.url;
                    if matches!(a.kind, AttachmentKind::Link) && has_preview {
                        ui.add(egui::Image::new(a.preview_url.as_str()).max_size(Vec2::new(280.0, 160.0)).corner_radius(CornerRadius::same(10)));
                    }
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(icon).size(16.0).color(pal().text));
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            let ink = if fill == pal().blue && !matches!(a.kind, AttachmentKind::Link) { Color32::WHITE } else { pal().text };
                            ui.add(egui::Label::new(sf(&truncate(&link_title(a), 80), 14.0, 600).color(ink)).wrap());
                            if let Some(host) = url::Url::parse(&a.url).ok().and_then(|u| u.host_str().map(str::to_string)) {
                                ui.label(sf(&host, 12.0, 400).color(pal().muted));
                            }
                        });
                    });
                })
                .response
                .interact(Sense::click());
            if r.clicked() && !a.url.is_empty() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(&a.url));
            }
            r.on_hover_cursor(egui::CursorIcon::PointingHand)
        }
    }
}

/// A readable label for a link/file attachment (never a raw path).
fn link_title(a: &Attachment) -> String {
    let t = a.title.trim();
    if !t.is_empty() && !t.starts_with('/') && !t.starts_with("http") {
        return t.to_string();
    }
    match a.kind {
        AttachmentKind::Link => "Link".into(),
        AttachmentKind::Audio => "Voice message".into(),
        _ => "Attachment".into(),
    }
}

/// Messenger-style grouped bubble corners: the sender-side corners shrink
/// where a bubble joins the previous/next one from the same sender.
fn group_corners(mine: bool, first: bool, last: bool) -> CornerRadius {
    let (big, small) = (18u8, 4u8);
    let top = if first { big } else { small };
    let bottom = if last { big } else { small };
    if mine {
        CornerRadius { nw: big, sw: big, ne: top, se: bottom }
    } else {
        CornerRadius { nw: top, sw: bottom, ne: big, se: big }
    }
}

/// Make `rect` drag the window (it sits under the transparent titlebar);
/// double-click toggles maximize like a native titlebar.
fn titlebar_drag(ui: &mut egui::Ui, rect: Rect) {
    let r = ui.interact(rect, ui.scope_id().with("titlebar_drag"), Sense::click_and_drag());
    // Move the window natively from the press: egui's drag only starts after
    // the pointer has moved a few points, so the window lagged behind it.
    if r.hovered() && ui.input(|i| i.pointer.primary_pressed()) && crate::macos::titlebar_mouse_down() {
        return;
    }
    if r.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    if r.double_clicked() {
        let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
    }
}

/// Attach-files button: a drawn paperclip (tilted, three strokes joined by
/// round bends), in the accent color.
fn paperclip_button(ui: &mut egui::Ui, rect: Rect) -> egui::Response {
    let resp = ui.interact(rect, ui.scope_id().with("attach"), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let color = if resp.hovered() { pal().blue } else { pal().blue.gamma_multiply(0.85) };
    let arc = |cx: f32, cy: f32, r: f32, from: f32, to: f32| -> Vec<Pos2> {
        (0..=10).map(|i| {
            let a = (from + (to - from) * i as f32 / 10.0).to_radians();
            Pos2::new(cx + r * a.cos(), cy + r * a.sin())
        }).collect()
    };
    // Upright clip in local coordinates (y down), then tilted 40°.
    let mut pts = vec![Pos2::new(-4.0, 6.5), Pos2::new(-4.0, -5.0)];
    pts.extend(arc(-0.75, -5.0, 3.25, 180.0, 360.0));
    pts.push(Pos2::new(2.5, 7.0));
    pts.extend(arc(0.5, 7.0, 2.0, 0.0, 180.0));
    pts.push(Pos2::new(-1.5, -3.0));
    let (sin, cos) = 40f32.to_radians().sin_cos();
    let c = rect.center();
    let pts = pts.into_iter().map(|p| c + Vec2::new(p.x * cos - p.y * sin, p.x * sin + p.y * cos)).collect();
    ui.painter().add(egui::Shape::line(pts, Stroke::new(1.7, color)));
    resp
}

/// The emoji-picker toggle inside the composer: a drawn line smiley (not an
/// emoji), so it doesn't read as a second emoji next to the 👍 button.
fn smiley_button(ui: &mut egui::Ui, rect: Rect, open: bool) -> egui::Response {
    let resp = ui.interact(rect, ui.scope_id().with("emoji_toggle"), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let color = if open || resp.hovered() { pal().blue } else { pal().muted };
    let stroke = Stroke::new(1.6, color);
    let c = rect.center();
    let p = ui.painter();
    p.circle_stroke(c, 9.0, stroke);
    p.circle_filled(c + Vec2::new(-3.2, -2.4), 1.3, color);
    p.circle_filled(c + Vec2::new(3.2, -2.4), 1.3, color);
    let smile: Vec<Pos2> = (0..=8)
        .map(|i| {
            let a = std::f32::consts::PI * (0.2 + 0.6 * i as f32 / 8.0);
            c + Vec2::new(4.8 * a.cos(), 4.8 * a.sin())
        })
        .collect();
    p.add(egui::Shape::line(smile, stroke));
    resp
}

/// A toolbar button showing an SF Symbol (rendered by AppKit, see
/// `macos::sf_symbol_png`), 36 pt hit area with a soft hover fill. Falls back
/// to the symbol name's first letter if the symbol can't be rendered.
fn symbol_button(ui: &mut egui::Ui, symbol: &str, color: Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if resp.hovered() {
        ui.painter().circle_filled(rect.center(), 17.0, pal().quote);
    }
    match crate::macos::sf_symbol_png(symbol, 17.0, [color.r(), color.g(), color.b()]) {
        Some(png) => {
            // Fit inside 22 pt keeping the symbol's aspect ratio (paint_at would
            // stretch it); the PNG header holds width and height at bytes 16..24.
            let dim = |i: usize| png.get(i..i + 4).map_or(1.0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f32);
            let (w, h) = (dim(16), dim(20));
            let scale = 22.0 / w.max(h);
            let target = Rect::from_center_size(rect.center(), Vec2::new(w * scale, h * scale));
            egui::Image::from_bytes(format!("bytes://sf/{symbol}-{:?}.png", color.to_array()), png).paint_at(ui, target);
        }
        None => {
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, &symbol[..1], egui::FontId::proportional(16.0), color);
        }
    }
    resp
}

fn icon_button(ui: &mut egui::Ui, icon: &str, color: Color32) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(icon).size(20.0).color(color))
            .frame(false)
            .min_size(Vec2::splat(36.0)),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn round_action(ui: &mut egui::Ui, icon: &str, label: &str) -> bool {
    let mut clicked = false;
    ui.allocate_ui_with_layout(Vec2::new(64.0, 64.0), Layout::top_down(Align::Center), |ui| {
        let (rect, r) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::click());
        ui.painter().circle_filled(rect.center(), 18.0, if r.hovered() { pal().border } else { pal().input });
        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, icon, egui::FontId::proportional(16.0), pal().text);
        ui.label(sf(label, 12.0, 400));
        clicked = r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
    });
    clicked
}

/// macOS-style search field: a 36 pt glass capsule with a drawn magnifier,
/// a clear button once there's text, and a blue ring while focused.
fn pill_search(ui: &mut egui::Ui, text: &mut String) {
    const H: f32 = 36.0;
    let id = ui.make_persistent_id("search_field");
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), H), Sense::hover());
    let focused = ui.memory(|m| m.has_focus(id));
    let radius = CornerRadius::same((H / 2.0) as u8);
    ui.painter().rect_filled(rect, radius, pal().input);
    let ring = if focused { Stroke::new(1.5, pal().blue.gamma_multiply(0.8)) } else { Stroke::new(1.0, pal().border) };
    ui.painter().rect_stroke(rect, radius, ring, egui::StrokeKind::Inside);

    // Magnifier: a ring plus a short handle.
    let c = Pos2::new(rect.left() + 17.0, rect.center().y - 1.0);
    let icon = Stroke::new(1.6, pal().muted);
    ui.painter().circle_stroke(c, 5.0, icon);
    ui.painter().line_segment([c + Vec2::splat(3.7), c + Vec2::splat(7.2)], icon);

    let clear_w = if text.is_empty() { 12.0 } else { 34.0 };
    let field = Rect::from_min_max(Pos2::new(rect.left() + 32.0, rect.top()), Pos2::new(rect.right() - clear_w, rect.bottom()));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(field).layout(Layout::left_to_right(Align::Center)));
    let resp = child.add(
        TextEdit::singleline(text)
            .id(id)
            .hint_text(sf("Search", 15.0, 400).color(pal().muted))
            .font(egui::FontId::new(15.0 * text_scale(), egui::FontFamily::Name("sf-400".into())))
            .frame(Frame::NONE)
            .margin(Margin::ZERO)
            .desired_width(f32::INFINITY),
    );
    if resp.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
        text.clear();
    }

    if !text.is_empty() {
        let center = Pos2::new(rect.right() - 18.0, rect.center().y);
        let hit = Rect::from_center_size(center, Vec2::splat(20.0));
        let clear = ui.interact(hit, id.with("clear"), Sense::click());
        let fill = if clear.hovered() { pal().muted } else { pal().muted.gamma_multiply(0.7) };
        ui.painter().circle_filled(center, 7.5, fill);
        let x = Stroke::new(1.5, Color32::from_rgb(30, 30, 32));
        ui.painter().line_segment([center + Vec2::new(-2.8, -2.8), center + Vec2::new(2.8, 2.8)], x);
        ui.painter().line_segment([center + Vec2::new(-2.8, 2.8), center + Vec2::new(2.8, -2.8)], x);
        if clear.clicked() {
            text.clear();
            resp.request_focus();
        }
    }
}

/// Cmd + / Cmd = grows text, Cmd - shrinks it, Cmd 0 resets (like Messenger
/// and browsers). Only text scales; layout and avatars keep their size.
fn handle_text_size_keys(ctx: &egui::Context) {
    use egui::Modifiers;
    let (bigger, smaller, reset) = ctx.input_mut(|i| {
        (
            i.consume_key(Modifiers::COMMAND, Key::Plus) || i.consume_key(Modifiers::COMMAND, Key::Equals),
            i.consume_key(Modifiers::COMMAND, Key::Minus),
            i.consume_key(Modifiers::COMMAND, Key::Num0),
        )
    });
    if !(bigger || smaller || reset) {
        return;
    }
    let current = text_scale();
    let idx = TEXT_SCALE_STEPS.iter().position(|s| (s - current).abs() < 0.01).unwrap_or(2);
    let next = if reset {
        1.0
    } else if bigger {
        TEXT_SCALE_STEPS[(idx + 1).min(TEXT_SCALE_STEPS.len() - 1)]
    } else {
        TEXT_SCALE_STEPS[idx.saturating_sub(1)]
    };
    set_text_scale(ctx, next);
}

/// Text in SF Pro (the font Messenger web renders with on macOS) at a CSS-style weight.
fn sf(text: &str, size: f32, weight: u16) -> RichText {
    RichText::new(text).size(size * text_scale()).family(egui::FontFamily::Name(format!("sf-{weight}").into()))
}

/// Register SF Pro at the weights Messenger uses (400/500/600/700) via the
/// variable font's `wght` axis. 400 also becomes the default proportional font.
fn install_fonts(ctx: &egui::Context) {
    use egui::epaint::text::VariationCoords;
    let Ok(bytes) = std::fs::read("/System/Library/Fonts/SFNS.ttf") else { return };
    let mut defs = egui::FontDefinitions::default();
    // egui shrinks its emoji fallback fonts (to 0.81 / 0.90); next to SF Pro
    // they then look small, so draw them at full size.
    for (name, scale) in [("NotoEmoji-Regular", 1.0), ("emoji-icon-font", 1.05)] {
        if let Some(data) = defs.font_data.get_mut(name) {
            let mut d = (**data).clone();
            d.tweak.scale = scale;
            *data = Arc::new(d);
        }
    }
    let fallbacks = defs.families.get(&egui::FontFamily::Proportional).cloned().unwrap_or_default();
    for weight in [400u16, 500, 600, 700] {
        let name = format!("sf-{weight}");
        let mut data = egui::FontData::from_owned(bytes.clone());
        data.tweak.coords = VariationCoords::new([(b"wght", weight as f32)]);
        defs.font_data.insert(name.clone(), Arc::new(data));
        let family: Vec<String> = std::iter::once(name.clone()).chain(fallbacks.iter().cloned()).collect();
        if weight == 400 {
            defs.families.insert(egui::FontFamily::Proportional, family.clone());
        }
        defs.families.insert(egui::FontFamily::Name(name.into()), family);
    }
    ctx.set_fonts(defs);
}

/// Messenger's typing indicator: a small bubble with three dots that rise and
/// brighten one after another.
fn typing_bubble(ui: &mut egui::Ui) {
    const SIZE: Vec2 = Vec2::new(46.0, 26.0);
    let (rect, _) = ui.allocate_exact_size(SIZE, Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(13), pal().bubble_other);
    let t = ui.input(|i| i.time) as f32;
    for i in 0..3 {
        // Each dot peaks a third of a cycle after the previous one.
        let phase = (t * 5.0 - i as f32 * 0.9).sin().max(0.0);
        let center = Pos2::new(rect.center().x + (i as f32 - 1.0) * 9.0, rect.center().y - phase * 3.0);
        let color = pal().muted.gamma_multiply(0.45 + 0.55 * phase);
        ui.painter().circle_filled(center, 3.0, color);
    }
    ui.ctx().request_repaint(); // keep animating while it's shown
}

#[derive(Clone, Copy, PartialEq)]
enum SendStatus {
    Sending,
    Sent,
    Delivered,
}

/// The small status mark under our newest message: an empty circle while
/// sending, a circle with a check once the server has it, a filled circle with
/// a check once a recipient's device got it.
fn status_row(ui: &mut egui::Ui, status: SendStatus) {
    const R: f32 = 6.5;
    ui.add_space(2.0);
    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
        ui.add_space(16.0);
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(R * 2.0 + 1.0), Sense::hover());
        let c = rect.center();
        let p = ui.painter();
        let stroke = Stroke::new(1.2, pal().muted);
        let check = |p: &egui::Painter, color: Color32| {
            let s = Stroke::new(1.4, color);
            p.line_segment([c + Vec2::new(-3.0, 0.2), c + Vec2::new(-0.9, 2.4)], s);
            p.line_segment([c + Vec2::new(-0.9, 2.4), c + Vec2::new(3.2, -2.2)], s);
        };
        match status {
            SendStatus::Sending => {
                p.circle_stroke(c, R, stroke);
            }
            SendStatus::Sent => {
                p.circle_stroke(c, R, stroke);
                check(p, pal().muted);
            }
            SendStatus::Delivered => {
                p.circle_filled(c, R, pal().muted);
                check(p, pal().bg.to_opaque());
            }
        }
        resp.on_hover_text(match status {
            SendStatus::Sending => "Sending",
            SendStatus::Sent => "Sent",
            SendStatus::Delivered => "Delivered",
        });
    });
    ui.add_space(4.0);
}

/// For each message, the people (other than us) whose read watermark stops at
/// it: the newest message they've seen. Readers whose newest seen message is
/// their own aren't listed (like Messenger).
fn readers_by_message(store: &Store, thread: &Thread, messages: &[Message]) -> HashMap<String, Vec<(i64, i64)>> {
    let mut out: HashMap<String, Vec<(i64, i64)>> = HashMap::new();
    let Some(receipts) = store.read_receipts.get(&thread.key) else { return out };
    for (&reader, &watermark) in receipts {
        if reader == store.me || watermark <= 0 {
            continue;
        }
        let Some(m) = messages.iter().rev().find(|m| !m.pending && !m.is_admin && m.timestamp_ms <= watermark) else { continue };
        if m.sender_id != reader {
            out.entry(m.id.clone()).or_default().push((reader, watermark));
        }
    }
    for list in out.values_mut() {
        list.sort_by_key(|(_, ts)| std::cmp::Reverse(*ts));
    }
    out
}

/// Small avatars of the people who stopped reading at this message; hover for
/// names and times.
fn seen_by_row(ui: &mut egui::Ui, store: &Store, thread: i64, readers: &[(i64, i64)]) {
    const SIZE: f32 = 18.0;
    const MAX: usize = 8;
    ui.add_space(2.0);
    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
        ui.add_space(16.0);
        ui.spacing_mut().item_spacing.x = 3.0;
        if readers.len() > MAX {
            ui.label(sf(&format!("+{}", readers.len() - MAX), 11.0, 500).color(pal().muted));
        }
        let start = ui.cursor().min;
        for (id, _) in readers.iter().take(MAX) {
            let name = store.member_name(thread, *id);
            let picture = store.contacts.get(id).map(|c| c.picture.clone()).unwrap_or_default();
            avatar(ui, &picture, &name, SIZE);
        }
        let area = Rect::from_two_pos(start, ui.min_rect().left_bottom());
        let names = readers
            .iter()
            .map(|(id, ts)| format!("{} · {}", store.member_name(thread, *id), long_time(*ts)))
            .collect::<Vec<_>>()
            .join("\n");
        ui.interact(area, ui.scope_id().with("seen_by"), Sense::hover()).on_hover_text(format!("Seen by\n{names}"));
    });
    ui.add_space(4.0);
}

fn avatar(ui: &mut egui::Ui, url: &str, name: &str, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    paint_avatar(ui, rect, url, name);
}

/// A chat's avatar. A group without its own picture shows two members,
/// overlapping (Messenger's layout): one up right, one down left in front,
/// cut out from the back one by a ring of the background color.
fn thread_avatar(ui: &mut egui::Ui, store: &Store, t: &Thread, title: &str, size: f32) {
    let pair = store.group_pictures(t);
    let [(back_url, back_name), (front_url, front_name)] = pair.as_slice() else {
        avatar(ui, &store.thread_picture(t), title, size);
        return;
    };
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let small = size * 0.68;
    let back = Rect::from_min_size(Pos2::new(rect.right() - small, rect.top()), Vec2::splat(small));
    let front = Rect::from_min_size(Pos2::new(rect.left(), rect.bottom() - small), Vec2::splat(small));
    paint_avatar(ui, back, back_url, back_name);
    let ring = if LIGHT_THEME.load(Ordering::Relaxed) { Color32::from_rgb(236, 238, 242) } else { Color32::from_rgb(24, 32, 50) };
    ui.painter().circle_filled(front.center(), small / 2.0 + size * 0.04, ring);
    paint_avatar(ui, front, front_url, front_name);
}

/// A round avatar: the picture once loaded, a grey disc while it loads, and the
/// name's initial if there's no picture or it failed to load (expired or
/// removed profile pictures; egui would draw its red error triangle).
fn paint_avatar(ui: &egui::Ui, rect: Rect, url: &str, name: &str) {
    let initial = |ui: &egui::Ui| {
        ui.painter().circle_filled(rect.center(), rect.width() / 2.0, Color32::from_rgb(70, 72, 76));
        let initial = name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, initial, egui::FontId::proportional(rect.width() * 0.45), Color32::WHITE);
    };
    if url.is_empty() {
        initial(ui);
        return;
    }
    let image = egui::Image::new(url).corner_radius(CornerRadius::same((rect.width() / 2.0) as u8));
    match image.load_for_size(ui.ctx(), rect.size()) {
        Ok(egui::load::TexturePoll::Ready { .. }) => image.paint_at(ui, rect),
        Ok(egui::load::TexturePoll::Pending { .. }) => {
            ui.painter().circle_filled(rect.center(), rect.width() / 2.0, pal().quote);
        }
        Err(_) => initial(ui),
    }
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s }
}

fn short_time(ms: i64) -> String {
    if ms <= 0 {
        return String::new();
    }
    let mins = (now_ms() - ms) / 60_000;
    match mins {
        i64::MIN..=0 => "now".into(),
        1..=59 => format!("{mins}m"),
        60..=1439 => format!("{}h", mins / 60),
        1440..=10079 => format!("{}d", mins / 1440),
        _ => format!("{}w", mins / 10080),
    }
}

fn long_time(ms: i64) -> String {
    let Some(dt) = Local.timestamp_millis_opt(ms).single() else { return String::new() };
    let today = Local::now().date_naive();
    if dt.date_naive() == today {
        dt.format("%H:%M").to_string()
    } else if (today - dt.date_naive()).num_days() < 7 {
        dt.format("%a %H:%M").to_string()
    } else {
        dt.format("%d %b %Y, %H:%M").to_string()
    }
}

/// Start a fresh copy of the app and quit this one.
fn relaunch() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
    std::process::exit(0);
}

#[cfg(test)]
mod mention_tests {
    use super::*;

    #[test]
    fn finds_the_query_being_typed() {
        assert_eq!(mention_query("hi @tr"), Some((3, "tr")));
        assert_eq!(mention_query("@"), Some((0, "")));
        assert_eq!(mention_query("mail a@b"), None, "an @ inside a word isn't a mention");
        assert_eq!(mention_query("no tag"), None);
    }

    #[test]
    fn matching_ignores_vietnamese_diacritics() {
        assert!(fold_name("Trực Dean").contains(&fold_name("truc")));
        assert!(fold_name("Đông").contains("dong"));
    }

    #[test]
    fn mention_offsets_count_utf16_units() {
        let text = "😂 Trực Dean ơi, Loc";
        let m = locate_mentions(text, &[(1, "Trực Dean".into()), (2, "Loc".into()), (3, "Gone".into())]);
        assert_eq!(m.len(), 2, "names edited out of the text are dropped");
        assert_eq!((m[0].id, m[0].offset, m[0].length), (1, 3, 9)); // 😂 is 2 UTF-16 units
        assert_eq!(m[1].offset, text[..text.find("Loc").unwrap()].encode_utf16().count());
    }
}
