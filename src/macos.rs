//! Small AppKit helpers: dock badge and notifications.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, MainThreadMarker};
use objc2_app_kit::NSApplication;
use objc2_foundation::{NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationPresentationOptions,
    UNNotificationRequest, UNNotificationSound, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};

static LAST_BADGE: AtomicUsize = AtomicUsize::new(usize::MAX);

pub fn set_dock_badge(count: usize) {
    if LAST_BADGE.swap(count, Ordering::Relaxed) == count {
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    let tile = app.dockTile();
    let label = (count > 0).then(|| NSString::from_str(&count.to_string()));
    tile.setBadgeLabel(label.as_deref());
}

define_class!(
    // Shows our banners even while the app is frontmost (another chat is open);
    // macOS hides them by default.
    #[unsafe(super(NSObject))]
    struct NotificationDelegate;

    unsafe impl NSObjectProtocol for NotificationDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            handler: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            handler.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::List
                | UNNotificationPresentationOptions::Sound,));
        }
    }
);

/// UNUserNotificationCenter needs an app bundle (it aborts in a bare binary).
fn in_bundle() -> bool {
    std::env::current_exe().is_ok_and(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"))
}

/// The notification center; the first call installs the delegate and asks
/// for permission (macOS prompts the user once).
fn center() -> Option<Retained<UNUserNotificationCenter>> {
    static DELEGATE: OnceLock<SendSync<Retained<NotificationDelegate>>> = OnceLock::new();
    if !in_bundle() {
        return None;
    }
    let center = UNUserNotificationCenter::currentNotificationCenter();
    if DELEGATE.get().is_none() {
        let delegate = DELEGATE.get_or_init(|| SendSync(unsafe { msg_send![NotificationDelegate::alloc(), init] }));
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate.0)));
        let done = RcBlock::new(|_granted: Bool, _error: *mut NSError| {});
        center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound | UNAuthorizationOptions::Badge,
            &done,
        );
    }
    Some(center)
}

/// Ask for notification permission at launch rather than on the first message.
pub fn init_notifications() {
    let _ = center();
}

struct SendSync<T>(T);
// SAFETY: the delegate is immutable and only messaged by the system.
unsafe impl<T> Send for SendSync<T> {}
unsafe impl<T> Sync for SendSync<T> {}

/// Post a notification as Messenger Desktop itself (so it carries the app's
/// own icon). Falls back to AppleScript outside the app bundle.
pub fn notify(title: &str, body: &str, thread: i64) {
    let Some(center) = center() else {
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!("display notification \"{}\" with title \"{}\" sound name \"default\"", esc(body), esc(title));
        let _ = std::process::Command::new("osascript").arg("-e").arg(script).spawn();
        return;
    };
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));
    content.setSound(Some(&UNNotificationSound::defaultSound()));
    // Groups notifications per chat in Notification Center.
    content.setThreadIdentifier(&NSString::from_str(&thread.to_string()));
    let id = NSString::from_str(&uuid::Uuid::new_v4().to_string());
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&id, &content, None);
    center.addNotificationRequest_withCompletionHandler(&request, None);
}

// ---- Liquid Glass ----
//
// egui draws everything into one layer, so the glass is native AppKit views
// placed *behind* that (transparent) layer: a behind-window blur for the whole
// window, plus one NSGlassEffectView per floating surface (sidebars, composer),
// positioned from the rects egui reports each frame.

use std::cell::RefCell;

use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSGlassEffectView, NSGlassEffectViewStyle, NSVisualEffectBlendingMode,
    NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView, NSWindow, NSWindowOrderingMode,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

/// A glass surface in egui points (top-left origin): x, y, width, height, corner radius.
pub type GlassRect = [f32; 5];

#[derive(Default)]
struct Glass {
    window: Option<Retained<NSWindow>>,
    background: Option<Retained<NSVisualEffectView>>,
    /// The content tint over the whole window, between the blur and the panes.
    tint: Option<Retained<objc2_app_kit::NSBox>>,
    tint_rgba: [f64; 4],
    panes: Vec<Retained<NSGlassEffectView>>,
    shown: Vec<GlassRect>,
    host_height: f64,
    /// Where macOS put the traffic-light buttons, before we nudged them.
    button_origins: Vec<NSPoint>,
    /// Light appearance: the glass panes get a light tint instead of a dark one.
    light: bool,
}

fn tint_color([r, g, b, a]: [f64; 4]) -> Retained<objc2_app_kit::NSColor> {
    objc2_app_kit::NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, a)
}

/// Glass tint per appearance: keeps text contrast on bright or dark wallpapers.
fn glass_tint(light: bool) -> Retained<objc2_app_kit::NSColor> {
    if light {
        objc2_app_kit::NSColor::colorWithWhite_alpha(1.0, 0.1)
    } else {
        // Navy, matching the dark blue palette.
        objc2_app_kit::NSColor::colorWithSRGBRed_green_blue_alpha(0.04, 0.08, 0.17, 0.12)
    }
}

/// Set the app's appearance: `Some(light)` forces Aqua or Dark Aqua, `None`
/// follows the system. `light` is the effective one, for the glass tint.
/// Main thread only.
/// Whether macOS itself is in light mode (System Settings › Appearance),
/// whatever appearance this app forces on its windows.
pub fn system_is_light() -> bool {
    let defaults = objc2_foundation::NSUserDefaults::standardUserDefaults();
    let style = defaults.stringForKey(&NSString::from_str("AppleInterfaceStyle"));
    !style.is_some_and(|s| s.to_string().eq_ignore_ascii_case("dark"))
}

pub fn set_appearance(forced: Option<bool>, light: bool, tint: [f64; 4]) {
    use objc2_app_kit::{NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua};
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    let appearance = forced.and_then(|l| NSAppearance::appearanceNamed(unsafe { if l { NSAppearanceNameAqua } else { NSAppearanceNameDarkAqua } }));
    app.setAppearance(appearance.as_deref());
    GLASS.with_borrow_mut(|g| {
        g.light = light;
        g.tint_rgba = tint;
        if let Some(view) = &g.tint {
            view.setFillColor(&tint_color(tint));
        }
        for pane in &g.panes {
            pane.setTintColor(Some(&glass_tint(light)));
        }
    });
}

thread_local! {
    static GLASS: RefCell<Glass> = RefCell::new(Glass::default());
}

/// Move the traffic lights 6 pt in and down so they sit inside the left glass
/// pane (inset 8 pt, 22 pt corners), as macOS 26 does for glass sidebars,
/// instead of straddling its rounded corner. Re-applied whenever macOS lays
/// them out again (resize, full screen).
fn nudge_window_buttons(window: &NSWindow, origins: &mut Vec<NSPoint>) {
    use objc2_app_kit::NSWindowButton;
    const SHIFT: f64 = 6.0;
    let buttons: Vec<_> = [NSWindowButton::CloseButton, NSWindowButton::MiniaturizeButton, NSWindowButton::ZoomButton]
        .into_iter()
        .filter_map(|b| window.standardWindowButton(b))
        .collect();
    if origins.len() != buttons.len() {
        *origins = buttons.iter().map(|b| b.frame().origin).collect();
    }
    for (button, original) in buttons.iter().zip(origins.iter_mut()) {
        let flipped = unsafe { button.superview() }.is_some_and(|v| v.isFlipped());
        let dy = if flipped { SHIFT } else { -SHIFT };
        let target = NSPoint::new(original.x + SHIFT, original.y + dy);
        let now = button.frame().origin;
        if now != target {
            // A position other than ours is a fresh layout by macOS: adopt it as the base.
            if now != *original {
                *original = now;
            }
            button.setFrameOrigin(NSPoint::new(original.x + SHIFT, original.y + dy));
        }
    }
}

/// Place the glass surfaces for this frame. Main thread only (egui's update).
pub fn sync_glass(rects: &[GlassRect]) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    GLASS.with_borrow_mut(|g| {
        if g.window.is_none() {
            let app = NSApplication::sharedApplication(mtm);
            g.window = app.mainWindow().or_else(|| app.windows().firstObject());
        }
        let Some(window) = g.window.clone() else { return };
        nudge_window_buttons(&window, &mut g.button_origins);
        let Some(content) = window.contentView() else { return };
        let Some(host) = (unsafe { content.superview() }) else { return };

        if g.background.is_none() {
            let bg = NSVisualEffectView::new(mtm);
            bg.setMaterial(NSVisualEffectMaterial::UnderWindowBackground);
            bg.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
            bg.setState(NSVisualEffectState::Active);
            bg.setFrame(host.bounds());
            bg.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
            host.addSubview_positioned_relativeTo(&bg, NSWindowOrderingMode::Below, Some(&content));
            g.background = Some(bg);

            // One flat tint over the blur for the whole window (the
            // conversation area and the gaps around the panes alike).
            let tint = objc2_app_kit::NSBox::new(mtm);
            tint.setBoxType(objc2_app_kit::NSBoxType::Custom);
            tint.setTitlePosition(objc2_app_kit::NSTitlePosition::NoTitle);
            tint.setBorderWidth(0.0);
            tint.setFillColor(&tint_color(g.tint_rgba));
            tint.setFrame(host.bounds());
            tint.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
            host.addSubview_positioned_relativeTo(&tint, NSWindowOrderingMode::Below, Some(&content));
            g.tint = Some(tint);
        }
        while g.panes.len() < rects.len() {
            let pane = NSGlassEffectView::new(mtm);
            pane.setStyle(NSGlassEffectViewStyle::Regular);
            pane.setTintColor(Some(&glass_tint(g.light)));
            // Directly below the egui layer, above the background blur.
            host.addSubview_positioned_relativeTo(&pane, NSWindowOrderingMode::Below, Some(&content));
            g.panes.push(pane);
        }

        let host_height = host.bounds().size.height;
        if g.shown.as_slice() == rects && g.host_height == host_height {
            return;
        }
        let flipped = host.isFlipped();
        for (i, pane) in g.panes.iter().enumerate() {
            let Some(&[x, y, w, h, radius]) = rects.get(i) else {
                pane.setHidden(true);
                continue;
            };
            let (x, y, w, h) = (x as f64, y as f64, w as f64, h as f64);
            let origin_y = if flipped { y } else { host_height - y - h };
            pane.setFrame(NSRect::new(NSPoint::new(x, origin_y), NSSize::new(w, h)));
            pane.setCornerRadius(radius as f64);
            pane.setHidden(false);
        }
        g.shown = rects.to_vec();
        g.host_height = host_height;
    });
}

// ---- Native context menus ----
//
// Menus pop up over egui-drawn content, where native glass placed behind egui
// would be hidden, so they're real NSMenus (which macOS draws as Liquid Glass).

use std::cell::Cell;

use objc2::sel;
use objc2_app_kit::{
    NSControlStateValueOn, NSEvent, NSFont, NSFontAttributeName, NSImage, NSMenu, NSMenuItem, NSMenuPresentationStyle,
    NSStringDrawing,
};
use objc2_foundation::NSDictionary;

pub enum MenuEntry {
    /// A row of emoji (reactions); `true` marks the selected one.
    Palette(Vec<(String, bool)>),
    /// A command with an SF Symbol icon.
    Item { title: String, symbol: &'static str },
    /// A titled submenu (choices inside are numbered like the rest).
    Submenu { title: String, symbol: &'static str, entries: Vec<MenuEntry> },
    /// One of a set of options; the chosen one gets a checkmark.
    Choice { title: String, checked: bool },
    /// A small section title (not a choice).
    Header(String),
    Separator,
}

thread_local! {
    static PICKED: Cell<Option<isize>> = const { Cell::new(None) };
}

define_class!(
    #[unsafe(super(NSObject))]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(pick:))]
        fn pick(&self, sender: &NSMenuItem) {
            PICKED.set(Some(sender.tag()));
        }
    }
);

/// An image of `emoji` (menus show palette items by their image). A lone
/// symbol like ❤ (U+2764) defaults to text style, a black glyph, so it gets
/// the emoji variation selector (U+FE0F) to draw in color.
fn emoji_image(emoji: &str) -> Retained<NSImage> {
    emoji_image_sized(emoji, 20.0)
}

fn emoji_image_sized(emoji: &str, point_size: f64) -> Retained<NSImage> {
    let styled = if emoji.chars().count() == 1 { format!("{emoji}\u{FE0F}") } else { emoji.to_string() };
    let text = NSString::from_str(&styled);
    let font = NSFont::systemFontOfSize(point_size);
    let attrs: Retained<NSDictionary<_, objc2::runtime::AnyObject>> =
        NSDictionary::from_slices(&[unsafe { NSFontAttributeName }], &[&*font as &objc2::runtime::AnyObject]);
    let size = unsafe { text.sizeWithAttributes(Some(&attrs)) };
    let draw = RcBlock::new(move |_rect: NSRect| -> Bool {
        unsafe { text.drawAtPoint_withAttributes(NSPoint::new(0.0, 0.0), Some(&attrs)) };
        Bool::YES
    });
    NSImage::imageWithSize_flipped_drawingHandler(size, false, &draw)
}

fn symbol_image(name: &str) -> Option<Retained<NSImage>> {
    NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), None)
}

fn menu_item(mtm: MainThreadMarker, target: &MenuTarget, title: &str, tag: isize) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), &NSString::from_str(title), Some(sel!(pick:)), &NSString::new())
    };
    unsafe { item.setTarget(Some(target)) };
    item.setTag(tag);
    item
}

/// Add `entries` to `menu`, numbering choices from `next` in order.
fn add_entries(mtm: MainThreadMarker, target: &MenuTarget, menu: &NSMenu, entries: &[MenuEntry], next: &mut isize) {
    for entry in entries {
        match entry {
            MenuEntry::Palette(emoji) => {
                let palette = NSMenu::new(mtm);
                palette.setPresentationStyle(NSMenuPresentationStyle::Palette);
                for (e, selected) in emoji {
                    let i = menu_item(mtm, target, e, *next);
                    *next += 1;
                    // "+" (more emoji) is an SF Symbol, the rest are emoji.
                    let image = if e == "+" { symbol_image("plus.circle") } else { Some(emoji_image(e)) };
                    i.setImage(image.as_deref());
                    if *selected {
                        i.setState(NSControlStateValueOn);
                    }
                    palette.addItem(&i);
                }
                let holder = NSMenuItem::new(mtm);
                holder.setSubmenu(Some(&palette));
                menu.addItem(&holder);
            }
            MenuEntry::Item { title, symbol } => {
                let i = menu_item(mtm, target, title, *next);
                *next += 1;
                i.setImage(symbol_image(symbol).as_deref());
                menu.addItem(&i);
            }
            MenuEntry::Submenu { title, symbol, entries } => {
                let sub = NSMenu::new(mtm);
                add_entries(mtm, target, &sub, entries, next);
                let holder = NSMenuItem::new(mtm);
                holder.setTitle(&NSString::from_str(title));
                holder.setImage(symbol_image(symbol).as_deref());
                holder.setSubmenu(Some(&sub));
                menu.addItem(&holder);
            }
            MenuEntry::Choice { title, checked } => {
                let i = menu_item(mtm, target, title, *next);
                *next += 1;
                if *checked {
                    i.setState(NSControlStateValueOn);
                }
                menu.addItem(&i);
            }
            MenuEntry::Header(title) => menu.addItem(&NSMenuItem::sectionHeaderWithTitle(&NSString::from_str(title), mtm)),
            MenuEntry::Separator => menu.addItem(&NSMenuItem::separatorItem(mtm)),
        }
    }
}

/// Show a native context menu at the mouse pointer and wait for a choice.
/// Choices are numbered in menu order (depth first, palette emoji one by
/// one); returns the chosen number.
pub fn popup_menu(entries: &[MenuEntry]) -> Option<usize> {
    let mtm = MainThreadMarker::new()?;
    let target: Retained<MenuTarget> = unsafe { msg_send![MenuTarget::alloc(), init] };
    let menu = NSMenu::new(mtm);
    add_entries(mtm, &target, &menu, entries, &mut 0);
    PICKED.set(None);
    menu.popUpMenuPositioningItem_atLocation_inView(None, NSEvent::mouseLocation(), None);
    PICKED.take().map(|t| t as usize)
}

// ---- SF Symbols for egui ----

use std::collections::HashMap;

use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSColor, NSDeviceRGBColorSpace, NSFontWeightSemibold, NSGraphicsContext,
    NSImageSymbolConfiguration,
};
use objc2_foundation::NSArray;

thread_local! {
    static SYMBOLS: RefCell<HashMap<String, Option<Vec<u8>>>> = RefCell::new(HashMap::new());
}

/// A PNG of the SF Symbol `name` at `point_size` (semibold) in `rgb`, rendered
/// at 3x for crisp display. Cached; `None` if the symbol doesn't exist.
/// Main thread only (AppKit drawing).
/// Multi-layer ".fill" symbols with an inner glyph (info.circle.fill) get the
/// glyph in white and the shape in `rgb`, like the web client's icons.
pub fn sf_symbol_png(name: &str, point_size: f64, rgb: [u8; 3]) -> Option<Vec<u8>> {
    let key = format!("{name}@{point_size}#{rgb:?}");
    if let Some(hit) = SYMBOLS.with_borrow(|m| m.get(&key).cloned()) {
        return hit;
    }
    let png = render_symbol(name, point_size, rgb);
    SYMBOLS.with_borrow_mut(|m| m.insert(key, png.clone()));
    png
}

fn render_symbol(name: &str, point_size: f64, rgb: [u8; 3]) -> Option<Vec<u8>> {
    MainThreadMarker::new()?;
    let base = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), None)?;
    let color = NSColor::colorWithSRGBRed_green_blue_alpha(rgb[0] as f64 / 255.0, rgb[1] as f64 / 255.0, rgb[2] as f64 / 255.0, 1.0);
    let size_cfg = NSImageSymbolConfiguration::configurationWithPointSize_weight(point_size, unsafe { NSFontWeightSemibold });
    // Palette order is primary (the inner glyph) then secondary (the shape).
    let palette = if name.contains("circle.fill") { vec![NSColor::whiteColor(), color] } else { vec![color] };
    let color_cfg = NSImageSymbolConfiguration::configurationWithPaletteColors(&NSArray::from_retained_slice(&palette));
    let image = base.imageWithSymbolConfiguration(&size_cfg.configurationByApplyingConfiguration(&color_cfg))?;
    image_png(&image)
}

/// A color PNG of `emoji` (Apple Color Emoji, which egui can't draw), cached.
/// Main thread only.
pub fn emoji_png(emoji: &str) -> Option<Vec<u8>> {
    let key = format!("emoji:{emoji}");
    if let Some(hit) = SYMBOLS.with_borrow(|m| m.get(&key).cloned()) {
        return hit;
    }
    let png = MainThreadMarker::new().and_then(|_| image_png(&emoji_image_sized(emoji, 28.0)));
    SYMBOLS.with_borrow_mut(|m| m.insert(key, png.clone()));
    png
}

/// Rasterize an NSImage at 3x into PNG bytes.
fn image_png(image: &NSImage) -> Option<Vec<u8>> {
    let size = image.size();
    const SCALE: f64 = 3.0;
    let (w, h) = ((size.width * SCALE).ceil() as isize, (size.height * SCALE).ceil() as isize);
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            w,
            h,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            0,
            0,
        )
    }?;
    rep.setSize(size);
    let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&ctx));
    image.drawInRect(NSRect::new(NSPoint::new(0.0, 0.0), size));
    NSGraphicsContext::restoreGraphicsState_class();
    let data = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?;
    Some(data.to_vec())
}

// ---- Clipboard files ----

/// Files on the clipboard (copied in Finder), as paths. Empty when the
/// clipboard holds no file URLs.
pub fn pasteboard_files() -> Vec<std::path::PathBuf> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeFileURL};
    let board = NSPasteboard::generalPasteboard();
    let Some(items) = board.pasteboardItems() else { return Vec::new() };
    items
        .iter()
        .filter_map(|item| item.stringForType(unsafe { NSPasteboardTypeFileURL }))
        .filter_map(|url| url::Url::parse(&url.to_string()).ok()?.to_file_path().ok())
        .filter(|p| p.is_file())
        .collect()
}

// ---- Video player ----
//
// Videos play in a native window with AVKit's player (streaming the link;
// local files for encrypted chats). One at a time: opening another replaces
// it, and closing the window stops playback.

use objc2_av_foundation::AVPlayer;
use objc2_av_kit::AVPlayerView;

thread_local! {
    static VIDEO: RefCell<Option<(Retained<NSWindow>, Retained<AVPlayer>)>> = const { RefCell::new(None) };
}

/// Play `url` (https link, `file://` URL or path) in the player window, sized
/// to the video's `width`×`height` (0 when unknown). Main thread only.
pub fn play_video(url: &str, title: &str, width: f32, height: f32) {
    use objc2_app_kit::{NSBackingStoreType, NSWindowStyleMask};
    use objc2_foundation::NSURL;
    let Some(mtm) = MainThreadMarker::new() else { return };
    let ns_url = if let Some(path) = url.strip_prefix("file://") {
        NSURL::fileURLWithPath(&NSString::from_str(path))
    } else if url.starts_with('/') {
        NSURL::fileURLWithPath(&NSString::from_str(url))
    } else {
        let Some(u) = NSURL::URLWithString(&NSString::from_str(url)) else { return };
        u
    };
    stop_video();

    // Fit the video in 960×640 (and at least 320 wide), keeping its shape.
    let (w, h) = if width > 0.0 && height > 0.0 { (width as f64, height as f64) } else { (640.0, 360.0) };
    let scale = (960.0 / w).min(640.0 / h);
    let (mut w, mut h) = (w * scale, h * scale);
    if w < 320.0 {
        h *= 320.0 / w;
        w = 320.0;
    }

    let player = unsafe { AVPlayer::playerWithURL(&ns_url, mtm) };
    let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Resizable | NSWindowStyleMask::Miniaturizable;
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h));
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(mtm.alloc(), rect, style, NSBackingStoreType::Buffered, false)
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str(if title.is_empty() { "Video" } else { title }));
    window.setContentAspectRatio(NSSize::new(w, h));
    let view = unsafe { AVPlayerView::initWithFrame(mtm.alloc(), rect) };
    unsafe { view.setPlayer(Some(&player)) };
    window.setContentView(Some(&view));
    // Over the main window's center.
    let app = NSApplication::sharedApplication(mtm);
    if let Some(main) = app.mainWindow() {
        let f = main.frame();
        let frame = window.frame();
        window.setFrameOrigin(NSPoint::new(
            f.origin.x + (f.size.width - frame.size.width) / 2.0,
            f.origin.y + (f.size.height - frame.size.height) / 2.0,
        ));
    } else {
        window.center();
    }
    window.makeKeyAndOrderFront(None);
    unsafe { player.play() };
    VIDEO.with_borrow_mut(|v| *v = Some((window, player)));
}

fn stop_video() {
    VIDEO.with_borrow_mut(|v| {
        if let Some((window, player)) = v.take() {
            unsafe { player.pause() };
            window.close();
        }
    });
}

/// Stop playback once the user closed the player window (called every frame).
pub fn reap_video() {
    let closed = VIDEO.with_borrow(|v| v.as_ref().is_some_and(|(window, _)| !window.isVisible() && !window.isMiniaturized()));
    if closed {
        stop_video();
    }
}

// ---- Glass windows ----
//
// Floating panels that must be real Liquid Glass over egui content (the
// sticker panel) are their own borderless, transparent egui viewports; this
// puts a glass pane behind the egui view and attaches the window to the main
// one, so it moves with it and stays above it.

/// Title of the sticker panel's window.
pub const STICKER_WINDOW: &str = "Messenger Desktop Stickers";
/// Title of the PIN dialog's window.
pub const PIN_WINDOW: &str = "Messenger Desktop PIN";

thread_local! {
    static GLASS_WINDOWS: RefCell<HashMap<String, (Retained<NSWindow>, Retained<NSGlassEffectView>)>> = RefCell::new(HashMap::new());
}

/// Give the window titled `title` a glass background with `radius` corners
/// (once per window; call every frame). Main thread only.
pub fn glass_window(title: &str, radius: f64) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    let Some(window) = app.windows().iter().find(|w| w.title().to_string() == title) else { return };
    let light = GLASS.with_borrow(|g| g.light);
    GLASS_WINDOWS.with_borrow_mut(|map| {
        if let Some((known, pane)) = map.get(title) {
            if std::ptr::eq(&**known, &*window) {
                pane.setTintColor(Some(&glass_tint(light)));
                return;
            }
        }
        let Some(content) = window.contentView() else { return };
        let Some(host) = (unsafe { content.superview() }) else { return };
        window.setOpaque(false);
        window.setBackgroundColor(Some(&objc2_app_kit::NSColor::clearColor()));
        window.setHasShadow(true);
        let pane = NSGlassEffectView::new(mtm);
        pane.setStyle(NSGlassEffectViewStyle::Regular);
        pane.setCornerRadius(radius);
        pane.setTintColor(Some(&glass_tint(light)));
        pane.setFrame(host.bounds());
        pane.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
        host.addSubview_positioned_relativeTo(&pane, NSWindowOrderingMode::Below, Some(&content));
        if let Some(main) = GLASS.with_borrow(|g| g.window.clone()) {
            unsafe { main.addChildWindow_ordered(&window, NSWindowOrderingMode::Above) };
        }
        map.insert(title.to_string(), (window, pane));
    });
}

// ---- Hidden web engine ----
//
// A WKWebView that's never shown, running facebook.com/messages with the
// login window's session (the default, persistent WebKit data store), so the
// app can use the web client's own code where reimplementing it isn't
// practical (restoring encrypted-chat history with the PIN). Its window sits
// off-screen but "visible", so WebKit doesn't throttle the page.

use objc2_web_kit::{WKContentWorld, WKWebView, WKWebViewConfiguration};

#[derive(Default)]
struct HiddenWeb {
    window: Option<Retained<NSWindow>>,
    view: Option<Retained<WKWebView>>,
    next_id: u64,
    results: HashMap<u64, Result<String, String>>,
}

thread_local! {
    static WEB: RefCell<HiddenWeb> = RefCell::new(HiddenWeb::default());
}

/// Open (once) the hidden page at `url`. Main thread only.
pub fn web_open(url: &str, user_agent: &str) {
    use objc2_app_kit::{NSBackingStoreType, NSWindowCollectionBehavior, NSWindowStyleMask};
    use objc2_foundation::{NSURLRequest, NSURL};
    let Some(mtm) = MainThreadMarker::new() else { return };
    WEB.with_borrow_mut(|w| {
        if w.view.is_some() {
            return;
        }
        let rect = NSRect::new(NSPoint::new(-30000.0, -30000.0), NSSize::new(1280.0, 800.0));
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(mtm.alloc(), rect, NSWindowStyleMask::Borderless, NSBackingStoreType::Buffered, false)
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setExcludedFromWindowsMenu(true);
        window.setIgnoresMouseEvents(true);
        window.setCollectionBehavior(NSWindowCollectionBehavior::Transient | NSWindowCollectionBehavior::IgnoresCycle);
        let config = unsafe { WKWebViewConfiguration::new(mtm) };
        let view = unsafe { WKWebView::initWithFrame_configuration(mtm.alloc(), NSRect::new(NSPoint::new(0.0, 0.0), rect.size), &config) };
        unsafe { view.setCustomUserAgent(Some(&NSString::from_str(user_agent))) };
        window.setContentView(Some(&view));
        window.orderFrontRegardless();
        if let Some(u) = NSURL::URLWithString(&NSString::from_str(url)) {
            let request = NSURLRequest::requestWithURL(&u);
            unsafe { view.loadRequest(&request) };
        }
        w.window = Some(window);
        w.view = Some(view);
    });
}

/// Whether the hidden page finished loading.
pub fn web_loaded() -> bool {
    WEB.with_borrow(|w| w.view.as_ref().is_some_and(|v| unsafe { !v.isLoading() }))
}

/// Run `body` as an async function in the page (it may `await` and must
/// `return` a string, e.g. `JSON.stringify(...)`). Returns an id for
/// `web_result`. Main thread only.
pub fn web_call(body: &str) -> Option<u64> {
    web_call_with(body, &[])
}

/// `web_call` with string arguments, visible to `body` as local variables of
/// those names. Values are passed as data, never pasted into the code (so a
/// PIN can't break out of a string literal).
pub fn web_call_with(body: &str, args: &[(&str, &str)]) -> Option<u64> {
    let mtm = MainThreadMarker::new()?;
    let keys: Vec<Retained<NSString>> = args.iter().map(|(k, _)| NSString::from_str(k)).collect();
    let values: Vec<Retained<objc2::runtime::AnyObject>> = args.iter().map(|(_, v)| Retained::into_super(Retained::into_super(NSString::from_str(v)))).collect();
    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let arguments = (!args.is_empty()).then(|| objc2_foundation::NSDictionary::from_retained_objects(&key_refs, &values));
    WEB.with_borrow_mut(|w| {
        let view = w.view.clone()?;
        w.next_id += 1;
        let id = w.next_id;
        let handler = RcBlock::new(move |result: *mut objc2::runtime::AnyObject, error: *mut NSError| {
            let outcome = if !error.is_null() {
                let e = unsafe { &*error };
                Err(e.localizedDescription().to_string())
            } else if result.is_null() {
                Ok(String::new())
            } else {
                let obj = unsafe { &*result };
                match obj.downcast_ref::<NSString>() {
                    Some(s) => Ok(s.to_string()),
                    None => Ok(format!("{obj:?}")),
                }
            };
            WEB.with_borrow_mut(|w| {
                w.results.insert(id, outcome);
            });
        });
        let world = unsafe { WKContentWorld::pageWorld(mtm) };
        unsafe { view.callAsyncJavaScript_arguments_inFrame_inContentWorld_completionHandler(&NSString::from_str(body), arguments.as_deref(), None, &world, Some(&handler)) };
        Some(id)
    })
}

/// The result of `web_call(id)` once it's done (taken).
pub fn web_result(id: u64) -> Option<Result<String, String>> {
    WEB.with_borrow_mut(|w| w.results.remove(&id))
}

/// Tear the hidden page down.
pub fn web_close() {
    WEB.with_borrow_mut(|w| {
        if let Some(window) = w.window.take() {
            window.close();
        }
        w.view = None;
        w.results.clear();
    });
}

// ---- In-window login ----
//
// The Facebook login page as a WKWebView inside the main window (not a
// separate window). It uses the default WebKit data store, so the session it
// creates is the one the hidden web engine sees too. The user types into
// Facebook's own page; the app only reads the resulting cookies.

thread_local! {
    static LOGIN: RefCell<Option<Retained<WKWebView>>> = const { RefCell::new(None) };
    static LOGIN_COOKIES: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Show the login page in the main window at `rect` (egui points, top-left
/// origin: x, y, width, height). Creates it once, then only moves it.
pub fn login_view(url: &str, user_agent: &str, rect: [f32; 4]) {
    use objc2_foundation::{NSURLRequest, NSURL};
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    let Some(window) = app.mainWindow().or_else(|| app.windows().firstObject()) else { return };
    let Some(content) = window.contentView() else { return };
    let height = content.bounds().size.height;
    let y = if content.isFlipped() { rect[1] as f64 } else { height - (rect[1] + rect[3]) as f64 };
    let frame = NSRect::new(NSPoint::new(rect[0] as f64, y), NSSize::new(rect[2] as f64, rect[3] as f64));
    LOGIN.with_borrow_mut(|slot| {
        if let Some(view) = slot.as_ref() {
            if view.frame() != frame {
                view.setFrame(frame);
            }
            return;
        }
        let config = unsafe { WKWebViewConfiguration::new(mtm) };
        let view = unsafe { WKWebView::initWithFrame_configuration(mtm.alloc(), frame, &config) };
        unsafe { view.setCustomUserAgent(Some(&NSString::from_str(user_agent))) };
        content.addSubview(&view);
        if let Some(u) = NSURL::URLWithString(&NSString::from_str(url)) {
            unsafe { view.loadRequest(&NSURLRequest::requestWithURL(&u)) };
        }
        window.makeFirstResponder(Some(&view));
        *slot = Some(view);
    });
}

/// Ask WebKit for the facebook.com cookies (as a `Cookie` header); the answer
/// arrives in `login_cookies` on a later frame.
pub fn request_login_cookies() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let store = unsafe { objc2_web_kit::WKWebsiteDataStore::defaultDataStore(mtm).httpCookieStore() };
    let handler = RcBlock::new(|cookies: std::ptr::NonNull<objc2_foundation::NSArray<objc2_foundation::NSHTTPCookie>>| {
        let cookies = unsafe { cookies.as_ref() };
        let header = cookies
            .iter()
            .filter(|c| {
                let d = c.domain().to_string();
                d.trim_start_matches('.') == "facebook.com" || d.ends_with(".facebook.com")
            })
            .map(|c| format!("{}={}", c.name(), c.value()))
            .collect::<Vec<_>>()
            .join("; ");
        LOGIN_COOKIES.with_borrow_mut(|c| *c = Some(header));
    });
    unsafe { store.getAllCookies(&handler) };
}

/// The cookies fetched by `request_login_cookies` (taken).
pub fn login_cookies() -> Option<String> {
    LOGIN_COOKIES.with_borrow_mut(Option::take)
}

/// Remove the login page from the window.
pub fn login_view_close() {
    LOGIN.with_borrow_mut(|slot| {
        if let Some(view) = slot.take() {
            view.removeFromSuperview();
        }
    });
}

/// Log out of the web side too: delete all WebKit website data (cookies and
/// the web client's storage), then run `then`. Otherwise the login page would
/// sign straight back in with the old session.
pub fn clear_web_data(then: impl Fn() + 'static) {
    let Some(mtm) = MainThreadMarker::new() else { return then() };
    unsafe {
        let store = objc2_web_kit::WKWebsiteDataStore::defaultDataStore(mtm);
        let types = objc2_web_kit::WKWebsiteDataStore::allWebsiteDataTypes(mtm);
        store.removeDataOfTypes_modifiedSince_completionHandler(&types, &objc2_foundation::NSDate::distantPast(), &RcBlock::new(then));
    }
}
