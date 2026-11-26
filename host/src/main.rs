//! 64-bit presentation host for the cnc-ddraw DirectDraw wrapper.
//!
//! The 32-bit `ddraw.dll` owns DirectDraw semantics and publishes complete
//! primary-surface frames through a shared-memory ring. This process owns the
//! wgpu DX12 device and a click-through overlay window (`WS_EX_TRANSPARENT` +
//! `HTTRANSPARENT`, kept enabled) so the game below keeps receiving real mouse
//! input while the overlay only presents. On macOS Wine this is the D3DMetal
//! 64-bit path instead of a 32-bit D3D12 stack.
#![windows_subsystem = "windows"]

mod ipc;
mod logging;
mod overlay;
mod presenter;

use std::ffi::c_void;
use std::sync::atomic::Ordering;

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT, WPARAM, WAIT_OBJECT_0};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_SYNCHRONIZE, SetEvent, WaitForSingleObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, IsWindowEnabled, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetClientRect, GetCursorPos, GetForegroundWindow, GetGUIThreadInfo, GetParent,
    GetWindowRect, GetWindowThreadProcessId, IsWindow, IsWindowVisible, PostMessageW,
    WindowFromPoint,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_RBUTTONDOWN,
    WM_RBUTTONUP, GUITHREADINFO,
};

use ddraw_protocol as protocol;
use overlay::Overlay;
use presenter::Presenter;

/// Describes a window well enough to tell apart "the game", "our overlay" and
/// "something else that is sitting on top of the game".
fn window_label(hwnd: HWND) -> String {
    // SAFETY: every call here only reads the handle. `GetClassNameW` writes at
    // most `class.len()` UTF-16 units into our own buffer and reports the count,
    // which is clamped before slicing.
    unsafe {
        let mut class = [0u16; 64];
        let len = GetClassNameW(hwnd, &mut class).max(0) as usize;
        let class = String::from_utf16_lossy(&class[..len.min(class.len())]);
        let mut pid = 0u32;
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        // A top-level window has no parent, so an error here is expected.
        let parent = GetParent(hwnd).unwrap_or_default();
        let own = GetCurrentProcessId();
        format!(
            "class=\"{class}\" pid={pid}{} parent=0x{:x}",
            if pid == own { " (host)" } else { "" },
            parent.0 as usize
        )
    }
}

/// Diagnostic: records where mouse input actually goes. Polled every loop
/// iteration (two cheap Win32 calls) and only logs when something changes.
#[derive(Default)]
struct InputProbe {
    last_hit: Option<isize>,
    click_down: bool,
}

impl InputProbe {
    fn poll(&mut self, game: HWND, overlay: HWND) {
        // SAFETY: every call below takes scalars/handles and writes into stack
        // values owned by this function.
        unsafe {
            let mut cursor = POINT::default();
            if GetCursorPos(&mut cursor).is_err() {
                return;
            }
            let hit = WindowFromPoint(cursor);
            let hit_id = hit.0 as isize;
            if self.last_hit != Some(hit_id) {
                self.last_hit = Some(hit_id);
                logging::log_msg(&format!(
                    "hit probe: cursor=({},{}) hit=0x{:x} {} game=0x{:x} overlay=0x{:x}",
                    cursor.x,
                    cursor.y,
                    hit.0 as usize,
                    window_label(hit),
                    game.0 as usize,
                    overlay.0 as usize
                ));
            }

            let down = GetAsyncKeyState(i32::from(VK_LBUTTON.0)) < 0;
            if !down {
                self.click_down = false;
                return;
            }
            if self.click_down {
                return;
            }
            self.click_down = true;

            let foreground = GetForegroundWindow();
            let mut client = RECT::default();
            let _ = GetClientRect(game, &mut client);
            let mut origin = POINT { x: 0, y: 0 };
            let _ = ClientToScreen(game, &mut origin);
            let mut overlay_rect = RECT::default();
            let _ = GetWindowRect(overlay, &mut overlay_rect);

            // The game thread's own idea of activation, focus and capture.
            let thread = GetWindowThreadProcessId(game, None);
            let mut gui = GUITHREADINFO {
                cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            let gui_ok = GetGUIThreadInfo(thread, &mut gui).is_ok();

            logging::log_msg(&format!(
                "click probe: cursor=({},{}) hit=0x{:x} {} game=0x{:x} overlay=0x{:x} fg=0x{:x} \
                 enabled={} visible={} client=({},{} {}x{}) overlay_rect=({},{} {}x{}) \
                 active=0x{:x} focus=0x{:x} capture=0x{:x} gui_ok={}",
                cursor.x,
                cursor.y,
                hit.0 as usize,
                window_label(hit),
                game.0 as usize,
                overlay.0 as usize,
                foreground.0 as usize,
                IsWindowEnabled(game).as_bool(),
                IsWindowVisible(game).as_bool(),
                origin.x,
                origin.y,
                client.right - client.left,
                client.bottom - client.top,
                overlay_rect.left,
                overlay_rect.top,
                overlay_rect.right - overlay_rect.left,
                overlay_rect.bottom - overlay_rect.top,
                gui.hwndActive.0 as usize,
                gui.hwndFocus.0 as usize,
                gui.hwndCapture.0 as usize,
                gui_ok,
            ));
        }
    }
}

const MK_LBUTTON: usize = 0x0001;
const MK_RBUTTON: usize = 0x0002;
const MK_MBUTTON: usize = 0x0010;

/// Optional input bridge: samples the real mouse and posts synthetic button
/// messages to the game window.
///
/// Wine does not reliably route mouse input past a topmost window, so the game
/// may never see a click even when the overlay is invisible to
/// `WindowFromPoint`. This path does not depend on routing at all: it reads
/// `GetCursorPos`/`GetAsyncKeyState` and posts `WM_*BUTTON*` with the game's
/// client coordinates. Enable with `CNC_DDRAW_HOST_INPUT_BRIDGE=1`.
#[derive(Default)]
struct InputBridge {
    last_pos: Option<(i32, i32)>,
    left: bool,
    right: bool,
    middle: bool,
}

impl InputBridge {
    fn poll(&mut self, game: HWND) {
        // SAFETY: every call takes scalars/handles and writes into stack values
        // owned by this function.
        unsafe {
            if GetForegroundWindow() != game {
                return;
            }
            let mut cursor = POINT::default();
            if GetCursorPos(&mut cursor).is_err() {
                return;
            }
            let mut client = RECT::default();
            if GetClientRect(game, &mut client).is_err() {
                return;
            }
            let mut origin = POINT { x: 0, y: 0 };
            if !ClientToScreen(game, &mut origin).as_bool() {
                return;
            }
            let x = cursor.x - origin.x;
            let y = cursor.y - origin.y;
            let inside = x >= 0 && y >= 0 && x < client.right && y < client.bottom;

            let left = GetAsyncKeyState(i32::from(VK_LBUTTON.0)) < 0;
            let right = GetAsyncKeyState(i32::from(VK_RBUTTON.0)) < 0;
            let middle = GetAsyncKeyState(i32::from(VK_MBUTTON.0)) < 0;
            let moved = self.last_pos != Some((x, y));
            self.last_pos = Some((x, y));
            let left_down = left && !self.left;
            let left_up = !left && self.left;
            let right_down = right && !self.right;
            let right_up = !right && self.right;
            let middle_down = middle && !self.middle;
            let middle_up = !middle && self.middle;
            self.left = left;
            self.right = right;
            self.middle = middle;
            if !inside {
                return;
            }

            let lparam =
                LPARAM(((((y as u32) & 0xffff) << 16) | ((x as u32) & 0xffff)) as isize);
            let post = |msg: u32, wparam: usize| {
                // SAFETY: `game` is a window handle supplied by the producer;
                // this only enqueues a message and never dereferences data.
                let _ = PostMessageW(Some(game), msg, WPARAM(wparam), lparam);
            };
            let edge = left_down || left_up || right_down || right_up || middle_down || middle_up;
            if moved && !edge {
                post(WM_MOUSEMOVE, 0);
            }
            if left_down {
                post(WM_MOUSEMOVE, MK_LBUTTON);
                post(WM_LBUTTONDOWN, MK_LBUTTON);
                logging::log_msg(&format!("input bridge: left down at ({x},{y})"));
            }
            if left_up {
                post(WM_LBUTTONUP, 0);
                logging::log_msg(&format!("input bridge: left up at ({x},{y})"));
            }
            if right_down {
                post(WM_RBUTTONDOWN, MK_RBUTTON);
            }
            if right_up {
                post(WM_RBUTTONUP, 0);
            }
            if middle_down {
                post(WM_MBUTTONDOWN, MK_MBUTTON);
            }
            if middle_up {
                post(WM_MBUTTONUP, 0);
            }
        }
    }
}

struct Args {
    base: String,
    hwnd: u32,
    pid: u32,
    backend: String,
    maintas: bool,
    vsync: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut base = None;
    let mut hwnd = None;
    let mut pid = None;
    let mut backend = "auto".to_string();
    let mut maintas = true;
    let mut vsync = true;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--base" => base = args.next(),
            "--hwnd" => {
                let value = args.next().ok_or("--hwnd requires a value")?;
                let value = value.trim_start_matches("0x");
                hwnd = Some(
                    u32::from_str_radix(value, 16)
                        .map_err(|_| format!("invalid --hwnd value: {value}"))?,
                );
            }
            "--pid" => {
                let value = args.next().ok_or("--pid requires a value")?;
                pid = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid --pid value: {value}"))?,
                );
            }
            "--backend" => backend = args.next().ok_or("--backend requires a value")?,
            "--maintas" => {
                let value = args.next().ok_or("--maintas requires a value")?;
                maintas = !matches!(value.as_str(), "0" | "false" | "no" | "off");
            }
            "--vsync" => {
                let value = args.next().ok_or("--vsync requires a value")?;
                vsync = !matches!(value.as_str(), "0" | "false" | "no" | "off");
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(Args {
        base: base.ok_or("missing --base")?,
        hwnd: hwnd.ok_or("missing --hwnd")?,
        pid: pid.ok_or("missing --pid")?,
        backend,
        maintas,
        vsync,
    })
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    logging::init();
    logging::log_msg(&format!(
        "starting: base={} hwnd=0x{:x} pid={} backend={} maintas={} vsync={}",
        args.base, args.hwnd, args.pid, args.backend, args.maintas, args.vsync
    ));

    let source = ipc::FrameSource::open(&args.base)?;
    let frame_event = ipc::open_event(&args.base, "_frame")?;
    let presented_event = ipc::open_event(&args.base, "_presented")?;
    let shutdown_event = ipc::open_event(&args.base, "_shutdown")?;

    let game = HWND(args.hwnd as usize as *mut c_void);
    // SAFETY: OpenProcess only takes scalar arguments; a failed open is fine and
    // simply disables parent-exit monitoring.
    let parent = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, args.pid) }.ok();

    // Declaration order matters: `presenter` is declared after `overlay` and
    // therefore dropped before it, so the wgpu surface dies before the HWND.
    // Own the overlay by the game window so it never appears in the taskbar (and stays above it).
    let overlay_owner = (!game.is_invalid()).then_some(game);
    let mut overlay = Overlay::new(overlay_owner)?;
    // Anything the overlay receives that belongs to the game (because Wine
    // routed the click to the topmost window instead of the game below it) is
    // forwarded to the game window.
    crate::overlay::set_input_target(game);
    // Forwarded clicks use the same letterboxed viewport the presenter draws into.
    crate::overlay::set_maintas(args.maintas);

    // The swapchain must live in the process that owns the window: D3DMetal
    // attaches its Metal layer to the HWND's NSView, which belongs to whichever
    // process created the window. Creating a swapchain for the game's HWND from
    // this process therefore crashes inside DXGI, so the overlay window (owned
    // by us) is the only presenter target.
    let mut presenter = Presenter::new(
        overlay.hwnd(),
        overlay.hinstance(),
        &args.backend,
        args.maintas,
        args.vsync,
    )?;
    // Keep the overlay enabled and click-through. `WS_EX_TRANSPARENT` plus
    // `HTTRANSPARENT` make the hit test skip it, which the `hit probe` confirms:
    // the probe reports the game window (or one of its children) under the
    // cursor, so real mouse input reaches the game natively and updates its
    // thread key state. A posted `WM_LBUTTONDOWN` does not, which is why taking
    // the click here and forwarding it made menu buttons dead in fullscreen.
    if std::env::var_os("CNC_DDRAW_HOST_OVERLAY_DISABLE").is_some() {
        overlay.make_click_through();
    } else {
        logging::log_msg("overlay: click-through, mouse input reaches the game");
    }
    logging::log_msg("present target: overlay");
    logging::log_msg("host ready");

    let mut last_seq = 0u32;
    let mut have_frame = false;
    // Present only when the picture would actually change. Presenting every loop
    // iteration would redraw the same texture at the display refresh rate while
    // the game produces far fewer frames, and each of those presents blocks on
    // the vsync interval inside `Present`.
    let mut presented_seq: Option<u32> = None;
    let mut presented_rect: Option<(i32, i32, i32, i32)> = None;
    // Escape hatch: present on every loop iteration again, in case a driver needs
    // the swapchain poked continuously to keep the window contents alive.
    let present_always = std::env::var_os("CNC_DDRAW_HOST_PRESENT_ALWAYS").is_some();
    let mut probe = InputProbe::default();

    // `CNC_DDRAW_HOST_TIMING=1` reports where the host actually spends time.
    let timing = std::env::var_os("CNC_DDRAW_HOST_TIMING").is_some();
    let mut window_start = std::time::Instant::now();
    let mut window_loops = 0u64;
    let mut window_uploads = 0u64;
    let mut window_presents = 0u64;
    let mut upload_ns = 0u64;
    let mut present_ns = 0u64;
    let mut bridge = if std::env::var_os("CNC_DDRAW_HOST_INPUT_BRIDGE").is_some() {
        logging::log_msg("input bridge: enabled");
        Some(InputBridge::default())
    } else {
        None
    };

    loop {
        Overlay::pump_messages();
        probe.poll(game, overlay.hwnd());
        if let Some(bridge) = bridge.as_mut() {
            bridge.poll(game);
        }

        // SAFETY: Both handles are valid and owned until the end of this
        // function; zero timeout makes these non-blocking probes.
        let shutdown = unsafe { WaitForSingleObject(shutdown_event, 0) } == WAIT_OBJECT_0;
        let parent_gone = parent
            .is_some_and(|handle| unsafe { WaitForSingleObject(handle, 0) } == WAIT_OBJECT_0);
        if shutdown || parent_gone {
            break;
        }
        // SAFETY: `game` is a handle value supplied by the producer; IsWindow
        // validates it and does not dereference it.
        if !unsafe { IsWindow(Some(game)) }.as_bool() {
            break;
        }

        let header = source.header();
        let seq = header.seq.load(Ordering::Acquire);
        if seq != last_seq {
            let slot = source.slot(seq as usize % protocol::SLOT_COUNT);
            let started = std::time::Instant::now();
            let result = presenter.upload(header, slot);
            upload_ns += started.elapsed().as_nanos() as u64;
            window_uploads += 1;
            match result {
                Ok(()) => {
                    last_seq = seq;
                    have_frame = true;
                    // The overlay forwards mouse input to the game, so it has to
                    // know the frame size to rescale click coordinates.
                    crate::overlay::set_frame_size(header.width, header.height);
                }
                Err(err) => logging::log_msg(&format!("frame upload rejected: {err}")),
            }
        }

        if have_frame && overlay::game_visible(game) {
            if let Some(rect) = overlay::client_screen_rect(game) {
                let (x, y, width, height) = rect;
                overlay.show_at(x, y, width, height);
                // A new frame, a moved or resized overlay, or a swapchain that
                // was just reconfigured all require a fresh present; anything
                // else would redraw the exact same picture.
                let changed = present_always
                    || presented_seq != Some(last_seq)
                    || presented_rect != Some(rect);
                if changed {
                    let started = std::time::Instant::now();
                    let result = presenter.present(width as u32, height as u32);
                    present_ns += started.elapsed().as_nanos() as u64;
                    window_presents += 1;
                    match result {
                        Ok(()) => {
                            presented_seq = Some(last_seq);
                            presented_rect = Some(rect);
                            // SAFETY: The event handle is valid for the process lifetime.
                            unsafe {
                                let _ = SetEvent(presented_event);
                            }
                        }
                        Err(err) => logging::log_msg(&format!("present failed: {err}")),
                    }
                }
            } else {
                overlay.hide();
                presented_rect = None;
            }
        } else {
            overlay.hide();
            presented_rect = None;
        }

        window_loops += 1;
        if timing && window_start.elapsed().as_secs_f64() >= 2.0 {
            let secs = window_start.elapsed().as_secs_f64();
            let per = |total: u64, count: u64| {
                if count == 0 {
                    0.0
                } else {
                    total as f64 / count as f64 / 1_000_000.0
                }
            };
            logging::log_msg(&format!(
                "timing: {:.1} loops/s upload {:.1}/s {:.2}ms present {:.1}/s {:.2}ms",
                window_loops as f64 / secs,
                window_uploads as f64 / secs,
                per(upload_ns, window_uploads),
                window_presents as f64 / secs,
                per(present_ns, window_presents),
            ));
            window_start = std::time::Instant::now();
            window_loops = 0;
            window_uploads = 0;
            window_presents = 0;
            upload_ns = 0;
            present_ns = 0;
        }

        // Wait for a new frame, but wake up regularly to follow the game
        // window (move, resize, minimize, Alt+Tab).
        // SAFETY: `frame_event` is a valid auto-reset event handle.
        unsafe {
            let _ = WaitForSingleObject(frame_event, 8);
        }
    }

    logging::log_msg("shutdown");
    if let Some(parent) = parent {
        // SAFETY: `parent` was opened by this function.
        unsafe {
            let _ = CloseHandle(parent);
        }
    }
    // SAFETY: All four events were opened by this function.
    unsafe {
        let _ = CloseHandle(frame_event);
        let _ = CloseHandle(presented_event);
        let _ = CloseHandle(shutdown_event);
    }
    Ok(())
}

fn main() {
    if let Err(err) = run() {
        logging::init();
        logging::log_msg(&format!("fatal: {err}"));
        std::process::exit(1);
    }
}
