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

use windows::Win32::Foundation::{CloseHandle, HWND, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, SetEvent, WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::IsWindow;

use ddraw_protocol as protocol;
use overlay::Overlay;
use presenter::Presenter;

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
    let mut presenter = Presenter::new(
        overlay.hwnd(),
        overlay.hinstance(),
        &args.backend,
        args.maintas,
    )?;
    logging::log_msg("host ready");

    let mut last_seq = 0u32;
    let mut have_frame = false;

    loop {
        Overlay::pump_messages();

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
                overlay.show_at(x, y, width, height);
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
