//! 64-bit presentation host for the cnc-ddraw DirectDraw wrapper.
//!
//! The 32-bit `ddraw.dll` owns DirectDraw semantics and publishes complete
//! primary-surface frames through a shared-memory ring. This process owns the
//! wgpu DX12 device and a click-through overlay window, so on macOS Wine the
//! D3DMetal 64-bit path is used instead of a 32-bit D3D12 stack.
#![windows_subsystem = "windows"]

mod ipc;
mod logging;
mod overlay;
mod presenter;

use std::ffi::c_void;
use std::sync::atomic::Ordering;

use windows::Win32::Foundation::{CloseHandle, HWND, POINT, RECT, WAIT_OBJECT_0};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, SetEvent, WaitForSingleObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, IsWindowEnabled, VK_LBUTTON};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetCursorPos, GetForegroundWindow, GetGUIThreadInfo, GetWindowRect,
    GetWindowThreadProcessId, IsWindow, IsWindowVisible, WindowFromPoint, GUITHREADINFO,
};

use ddraw_protocol as protocol;
use overlay::Overlay;
use presenter::Presenter;

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
                    "hit probe: cursor=({},{}) hit=0x{:x} game=0x{:x} overlay=0x{:x}",
                    cursor.x, cursor.y, hit.0 as usize, game.0 as usize, overlay.0 as usize
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
                "click probe: cursor=({},{}) hit=0x{:x} game=0x{:x} overlay=0x{:x} fg=0x{:x} \
                 enabled={} visible={} client=({},{} {}x{}) overlay_rect=({},{} {}x{}) \
                 active=0x{:x} focus=0x{:x} capture=0x{:x} gui_ok={}",
                cursor.x,
                cursor.y,
                hit.0 as usize,
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

struct Args {
    base: String,
    hwnd: u32,
    pid: u32,
    backend: String,
    maintas: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut base = None;
    let mut hwnd = None;
    let mut pid = None;
    let mut backend = "auto".to_string();
    let mut maintas = true;

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
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(Args {
        base: base.ok_or("missing --base")?,
        hwnd: hwnd.ok_or("missing --hwnd")?,
        pid: pid.ok_or("missing --pid")?,
        backend,
        maintas,
    })
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    logging::init();
    logging::log_msg(&format!(
        "starting: base={} hwnd=0x{:x} pid={} backend={} maintas={}",
        args.base, args.hwnd, args.pid, args.backend, args.maintas
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
    let mut overlay = Overlay::new()?;
    // Anything the overlay receives that belongs to the game (because Wine
    // routed the click to the topmost window instead of the game below it) is
    // forwarded to the game window.
    crate::overlay::set_input_target(game);

    // Present into the game's own window instead of the overlay. This leaves no
    // window above the game at all, so the game's thread receives real mouse
    // input (and its `GetKeyState` sees the buttons); the overlay keeps D3DMetal
    // in one process but Wine does not route clicks through it.
    let mut present_into_game = std::env::var_os("CNC_DDRAW_HOST_GAMEWINDOW").is_some();
    let target = if present_into_game { game } else { overlay.hwnd() };
    let mut presenter = match Presenter::new(target, overlay.hinstance(), &args.backend, args.maintas)
    {
        Ok(presenter) => presenter,
        Err(err) if present_into_game => {
            // Cross-process swapchains are supported by DXGI, but keep the
            // picture alive if this Wine/D3DMetal build refuses one.
            logging::log_msg(&format!(
                "game-window surface unavailable ({err}); presenting into the overlay instead"
            ));
            present_into_game = false;
            Presenter::new(
                overlay.hwnd(),
                overlay.hinstance(),
                &args.backend,
                args.maintas,
            )?
        }
        Err(err) => return Err(err),
    };
    // After the swapchain exists, make sure the overlay can never swallow a
    // click meant for the game window underneath it. Set
    // `CNC_DDRAW_HOST_OVERLAY_INPUT=1` to keep the old interactive overlay for
    // A/B testing input problems.
    if std::env::var_os("CNC_DDRAW_HOST_OVERLAY_INPUT").is_none() {
        overlay.make_click_through();
    } else {
        logging::log_msg("overlay: left interactive (CNC_DDRAW_HOST_OVERLAY_INPUT)");
    }
    logging::log_msg(&format!(
        "present target: {}",
        if present_into_game {
            "game window"
        } else {
            "overlay"
        }
    ));
    logging::log_msg("host ready");

    let mut last_seq = 0u32;
    let mut have_frame = false;
    let mut probe = InputProbe::default();

    loop {
        Overlay::pump_messages();
        probe.poll(game, overlay.hwnd());

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
            match presenter.upload(header, slot) {
                Ok(()) => {
                    last_seq = seq;
                    have_frame = true;
                }
                Err(err) => logging::log_msg(&format!("frame upload rejected: {err}")),
            }
        }

        if have_frame && overlay::game_visible(game) {
            if let Some((x, y, width, height)) = overlay::client_screen_rect(game) {
                if !present_into_game {
                    overlay.show_at(x, y, width, height);
                }
                match presenter.present(width as u32, height as u32) {
                    Ok(()) => {
                        // SAFETY: The event handle is valid for the process lifetime.
                        unsafe {
                            let _ = SetEvent(presented_event);
                        }
                    }
                    Err(err) => logging::log_msg(&format!("present failed: {err}")),
                }
            } else {
                overlay.hide();
            }
        } else {
            overlay.hide();
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
