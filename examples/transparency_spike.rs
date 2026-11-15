//! R3 spike: does a frameless tao + wry (WebView2) window give real per-pixel transparency?
//! `cargo run --example transparency_spike` - drag it over something colourful; click "close" or press Esc.

use tao::event::{ElementState, Event, KeyEvent, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::keyboard::KeyCode;
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

const HTML: &str = r#"<!doctype html><html><head><style>
html, body { margin: 0; height: 100%; background: transparent; font: 15px "Segoe UI", sans-serif; }
.wrap { display: grid; grid-template-columns: 1fr 1fr 1fr; height: 100%; }
.c { padding: 14px; color: #fff; -webkit-app-region: drag; }
.a { background: rgba(20, 22, 26, 0.40); }   /* background 40% */
.b { background: rgba(20, 22, 26, 0.00); }   /* background 0% */
.d { background: rgba(20, 22, 26, 1.00); }   /* background 100% */
.box { margin-top: 10px; padding: 8px; border-radius: 8px; background: rgba(255, 122, 26, 0.55); }
.t50 { color: rgba(255, 255, 255, 0.5); }
button { margin-top: 12px; -webkit-app-region: no-drag; }
</style></head><body><div class="wrap">
<div class="c a"><b>A: background 40%</b><div class="box">box 55%</div><p>text 100%</p><p class="t50">text 50%</p>
<button onclick="window.ipc.postMessage('close')">close</button></div>
<div class="c b"><b>B: background 0%</b><div class="box">box 55%</div><p>text 100%</p><p class="t50">text 50%</p></div>
<div class="c d"><b>C: background 100%</b><div class="box">box 55%</div><p>text 100%</p><p class="t50">text 50%</p></div>
</div></body></html>"#;

const BACKDROP: &str = r#"<!doctype html><html><body style="margin:0;height:100vh;
background:repeating-linear-gradient(90deg,#e00 0 40px,#0b0 40px 80px,#00e 80px 120px,#fff 120px 160px)"></body></html>"#;

fn main() -> wry::Result<()> {
    let event_loop = EventLoopBuilder::<()>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    // `--backdrop`: an opaque striped window at (100,100) right behind the spike, for a self-contained screenshot
    let backdrop = std::env::args().any(|a| a == "--backdrop");
    let size = tao::dpi::PhysicalSize::new(780u32, 260u32);
    let pos = tao::dpi::PhysicalPosition::new(100, 100);
    let _bd = if backdrop {
        let w = WindowBuilder::new()
            .with_decorations(false)
            .with_always_on_top(true)
            .with_inner_size(size)
            .with_position(pos)
            .build(&event_loop)
            .expect("backdrop");
        let v = WebViewBuilder::new().with_html(BACKDROP).build(&w)?;
        Some((w, v))
    } else {
        None
    };
    let flag = |f: &str| std::env::args().any(|a| a == f);
    use tao::platform::windows::{WindowBuilderExtWindows, WindowExtWindows};
    let window = WindowBuilder::new()
        .with_visible(!flag("--backdrop-only"))
        .with_title("transparency spike")
        .with_decorations(false)
        .with_transparent(true)
        .with_no_redirection_bitmap(flag("--noredir"))
        .with_always_on_top(true)
        .with_inner_size(size)
        .with_position(pos)
        .build(&event_loop)
        .expect("window");
    if flag("--dwm") {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::Graphics::Dwm::DwmExtendFrameIntoClientArea;
        use windows::Win32::UI::Controls::MARGINS;
        let m = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
        // SAFETY: valid top-level HWND owned by this thread.
        let r = unsafe { DwmExtendFrameIntoClientArea(HWND(window.hwnd() as _), &m) };
        eprintln!("DwmExtendFrameIntoClientArea: {r:?}");
    }
    let mut wb = WebViewBuilder::new().with_transparent(true);
    if flag("--bg") {
        wb = wb.with_background_color((0, 0, 0, 0));
    }
    let _webview = wb
        .with_html(HTML)
        .with_ipc_handler(move |_| {
            let _ = proxy.send_event(());
        })
        .build(&window)?;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(())
            | Event::WindowEvent { event: WindowEvent::CloseRequested, .. }
            | Event::WindowEvent {
                event:
                    WindowEvent::KeyboardInput {
                        event: KeyEvent { physical_key: KeyCode::Escape, state: ElementState::Pressed, .. },
                        ..
                    },
                ..
            } => *control_flow = ControlFlow::Exit,
            _ => {}
        }
    });
}
