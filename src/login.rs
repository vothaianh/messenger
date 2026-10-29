//! Login window: an embedded WebKit view on facebook.com. The user signs in
//! themselves (2FA and checkpoints work as usual); once the `c_user`/`xs`
//! session cookies appear we store the cookie header in the Keychain and exit.
//! The app never sees or stores the password.

use std::time::{Duration, Instant};

use anyhow::Result;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

use crate::session::{Cookies, USER_AGENT};

pub fn run() -> Result<()> {
    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("Log in — Messenger Desktop")
        .with_inner_size(tao::dpi::LogicalSize::new(480.0, 720.0))
        .build(&event_loop)?;
    let webview = WebViewBuilder::new()
        .with_url("https://www.facebook.com/login/?next=https%3A%2F%2Fwww.facebook.com%2Fmessages%2F")
        .with_user_agent(USER_AGENT)
        .build(&window)?;

    let mut last_check = Instant::now();
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(500));
        match event {
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => std::process::exit(1),
            Event::NewEvents(_) if last_check.elapsed() >= Duration::from_millis(500) => {
                last_check = Instant::now();
                // Not `cookies_for_url`: it requires an exact host match, and
                // Facebook sets its session cookies on `.facebook.com`.
                let Ok(cookies) = webview.cookies() else { return };
                let header = cookies
                    .iter()
                    .filter(|c| c.domain().is_some_and(|d| d.trim_start_matches('.') == "facebook.com" || d.ends_with(".facebook.com")))
                    .map(|c| format!("{}={}", c.name(), c.value()))
                    .collect::<Vec<_>>()
                    .join("; ");
                let cookies = Cookies(header);
                if cookies.is_logged_in() {
                    if let Err(e) = cookies.save() {
                        eprintln!("{e:#}");
                        std::process::exit(1);
                    }
                    std::process::exit(0);
                }
            }
            _ => {}
        }
    });
}
