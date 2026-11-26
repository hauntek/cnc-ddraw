//! Borderless overlay owned by the 64-bit host. D3DMetal therefore presents into
//! a window created by the same process, while the 32-bit game keeps focus and
//! its own dialogs.
//!
//! The overlay is the mouse input target on purpose: Wine drops clicks aimed at
//! a window that answers `WM_NCHITTEST` with `HTTRANSPARENT`, so the overlay
//! takes the click instead, rescales it to the game's frame size and posts it to
//! the game window - the same approach the original in-process renderer used.
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{ClientToScreen, ValidateRect};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetForegroundWindow, HTCLIENT, HWND_TOPMOST, IsIconic, IsWindow, IsWindowVisible,
    GWLP_HWNDPARENT, MA_NOACTIVATE, MSG, PM_REMOVE, PeekMessageW, PostMessageW, RegisterClassW,
    SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNOACTIVATE, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, TranslateMessage,
    WM_ERASEBKGND, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_NCHITTEST,
    WM_PAINT, WM_RBUTTONDBLCLK, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
    WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

/// Game window that receives forwarded mouse input.
static INPUT_TARGET: AtomicUsize = AtomicUsize::new(0);

/// Frame size the game is drawing (`(width << 16) | height`), used to turn
/// overlay client pixels back into the game's own coordinate space.
static FRAME_SIZE: AtomicU32 = AtomicU32::new(0);

/// Whether the presenter letterboxes the frame (`maintas`) instead of stretching it.
/// Forwarded clicks must use the same viewport the picture is drawn into.
static MAINTAS: AtomicBool = AtomicBool::new(false);

/// Register the window that should receive forwarded mouse input.
pub fn set_input_target(game: HWND) {
    INPUT_TARGET.store(game.0 as usize, Ordering::Relaxed);
}

/// Tell the overlay whether the presenter keeps the frame's aspect ratio.
pub fn set_maintas(enabled: bool) {
    MAINTAS.store(enabled, Ordering::Relaxed);
}

/// Tell the overlay how large the game's frame is, so forwarded clicks carry
/// coordinates in that space rather than overlay pixels.
pub fn set_frame_size(width: u32, height: u32) {
    FRAME_SIZE.store((width.min(0xffff) << 16) | height.min(0xffff), Ordering::Relaxed);
}

fn frame_size() -> Option<(u32, u32)> {
    let packed = FRAME_SIZE.load(Ordering::Relaxed);
    let (width, height) = (packed >> 16, packed & 0xffff);
    (width > 0 && height > 0).then_some((width, height))
}

/// Pixel rectangle the frame occupies inside the overlay's client area.
///
/// This mirrors `presenter::compute_viewport`: a frame that fits is kept 1:1 and centred, so the
/// click at overlay pixel `(vx, vy)` is frame pixel `(0, 0)` with no scale factor in between. Only
/// a window smaller than the frame is downscaled with the aspect ratio kept. Forwarding the raw
/// client area instead made every click in fullscreen land to the right of where the player aimed.
fn viewport(
    frame_w: u32,
    frame_h: u32,
    client_w: u32,
    client_h: u32,
    maintas: bool,
) -> (i64, i64, i64, i64) {
    if !maintas || frame_w == 0 || frame_h == 0 || client_w == 0 || client_h == 0 {
        return (0, 0, i64::from(client_w.max(1)), i64::from(client_h.max(1)));
    }
    if client_w >= frame_w && client_h >= frame_h {
        return (
            ((client_w - frame_w) / 2) as i64,
            ((client_h - frame_h) / 2) as i64,
            i64::from(frame_w),
            i64::from(frame_h),
        );
    }
    let w = (client_h as u64 * frame_w as u64 / frame_h as u64).clamp(1, client_w as u64);
    let h = (client_w as u64 * frame_h as u64 / frame_w as u64).clamp(1, client_h as u64);
    (
        ((client_w as u64 - w) / 2) as i64,
        ((client_h as u64 - h) / 2) as i64,
        w as i64,
        h as i64,
    )
}

/// Convert an overlay client pixel to the game's frame pixel, through the presenter's viewport.
///
/// Clicks in the letterbox bars clamp to the nearest frame edge, so they cannot produce negative
/// or out-of-range coordinates.
fn frame_point(
    px: i32,
    py: i32,
    client_w: u32,
    client_h: u32,
    frame_w: u32,
    frame_h: u32,
    maintas: bool,
) -> (i32, i32) {
    let (vx, vy, vw, vh) = viewport(frame_w, frame_h, client_w, client_h, maintas);
    let rx = (i64::from(px) - vx).clamp(0, vw - 1);
    let ry = (i64::from(py) - vy).clamp(0, vh - 1);
    (
        (rx * i64::from(frame_w) / vw) as i32,
        (ry * i64::from(frame_h) / vh) as i32,
    )
}

fn is_mouse_input(msg: u32) -> bool {
    matches!(
        msg,
        WM_MOUSEMOVE
            | WM_LBUTTONDOWN
            | WM_LBUTTONUP
            | WM_LBUTTONDBLCLK
            | WM_RBUTTONDOWN
            | WM_RBUTTONUP
            | WM_RBUTTONDBLCLK
            | WM_MBUTTONDOWN
            | WM_MBUTTONUP
            | WM_XBUTTONDOWN
            | WM_XBUTTONUP
            | WM_MOUSEWHEEL
            | WM_MOUSEHWHEEL
    )
}

unsafe extern "system" fn overlay_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // The overlay is the input target (see `WM_NCHITTEST`), so every mouse
    // message arrives here and has to be re-posted to the game with the
    // coordinates the game expects. That is what the original in-process
    // renderer did: it converted to its logical resolution and posted the
    // message on, instead of trying to be click-through.
    if is_mouse_input(msg) {
        let target = INPUT_TARGET.load(Ordering::Relaxed);
        if target != 0 {
            let game = HWND(target as *mut c_void);
            // Wheel messages carry screen coordinates in `wparam`, which need no
            // translation; every other mouse message packs client pixels in
            // `lparam` and must be rescaled to the game's frame size.
            let forwarded = if msg == WM_MOUSEWHEEL || msg == WM_MOUSEHWHEEL {
                lparam
            } else {
                let raw = lparam.0 as u32;
                let px = (raw & 0xffff) as i16 as i32;
                let py = ((raw >> 16) & 0xffff) as i16 as i32;
                // SAFETY: `hwnd` is the live overlay window owned by this process.
                let mut client = RECT::default();
                let _ = unsafe { GetClientRect(hwnd, &mut client) };
                let client_w = (client.right - client.left).max(1) as u32;
                let client_h = (client.bottom - client.top).max(1) as u32;
                let (lx, ly) = match frame_size() {
                    Some((frame_w, frame_h)) => frame_point(
                        px,
                        py,
                        client_w,
                        client_h,
                        frame_w,
                        frame_h,
                        MAINTAS.load(Ordering::Relaxed),
                    ),
                    None => (px, py),
                };
                if msg == WM_LBUTTONDOWN {
                    crate::logging::log_msg(&format!(
                        "overlay: left click overlay=({px},{py}) game=({lx},{ly}) frame={:?}",
                        frame_size()
                    ));
                }
                LPARAM((((ly as u16 as u32) << 16) | (lx as u16 as u32)) as isize)
            };
            // SAFETY: `game` is a window handle supplied by the producer; this
            // only enqueues a message and never dereferences Rust data.
            if unsafe { PostMessageW(Some(game), msg, wparam, forwarded) }.is_ok() {
                return LRESULT(0);
            }
        }
    }
    match msg {
        // Always claim the mouse. Answering `HTTRANSPARENT` here makes Wine drop
        // the click (the window is skipped by `WindowFromPoint` yet still owns the
        // input), so the overlay has to take the click and forward it instead.
        WM_NCHITTEST => LRESULT(HTCLIENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let _ = ValidateRect(Some(hwnd), None);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub struct Overlay {
    hwnd: HWND,
    hinstance: HINSTANCE,
    visible: bool,
    last_rect: Option<(i32, i32, i32, i32)>,
}

impl Overlay {
    /// Creates the overlay. `owner` should be the game window: an owned window never gets a
    /// taskbar button, and it also stays above its owner in the z-order.
    pub fn new(owner: Option<HWND>) -> Result<Self, String> {
        // SAFETY: GetModuleHandleW(None) returns the current module and takes
        // no borrowed pointers.
        let module =
            unsafe { GetModuleHandleW(None) }.map_err(|err| format!("GetModuleHandleW: {err}"))?;
        let hinstance = HINSTANCE(module.0);
        let class_name = w!("CncDdrawHostPresent");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(overlay_wndproc),
            hInstance: hinstance,
            lpszClassName: class_name,
            ..Default::default()
        };
        // SAFETY: `wc` is fully initialized and the class string is static.
        // A duplicate registration returns 0, which is harmless here.
        unsafe {
            let _ = RegisterClassW(&wc);
        }
        // SAFETY: All pointers are either static strings or null; the window is
        // created hidden (no WS_VISIBLE) and shown later with SW_SHOWNOACTIVATE.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                class_name,
                w!(""),
                WS_POPUP,
                0,
                0,
                1,
                1,
                owner,
                None,
                Some(hinstance),
                None,
            )
        }
        .map_err(|err| format!("CreateWindowExW: {err}"))?;
        // Belt and braces: `WS_EX_TOOLWINDOW` alone does not always keep a window out of the
        // taskbar under Wine, but an owned window never gets a button. Re-assert the owner here
        // in case the creation-time parent was ignored.
        if let Some(owner) = owner {
            // SAFETY: `hwnd` is the live overlay window and the owner is a valid window handle.
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, owner.0 as isize);
            }
        }
        crate::logging::log_msg(&format!(
            "overlay: hwnd=0x{:x} owner={} toolwindow=yes",
            hwnd.0 as usize,
            owner.map_or("none".to_string(), |o| format!("0x{:x}", o.0 as usize))
        ));

        Ok(Self {
            hwnd,
            hinstance,
            visible: false,
            last_rect: None,
        })
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    pub fn hinstance(&self) -> HINSTANCE {
        self.hinstance
    }

    /// Disable the overlay window, which takes it out of input routing.
    ///
    /// This is **not** the default and loses every click under Wine: a disabled
    /// window is still returned by `WindowFromPoint`, so it stays the input
    /// target, but it can no longer receive mouse messages. The overlay normally
    /// claims the mouse (`HTCLIENT`) and forwards it to the game.
    ///
    /// Kept for the case where a stack routes input to the overlay even though
    /// it is transparent; called after the swapchain exists so DXGI never has to
    /// deal with a disabled window during surface creation.
    pub fn make_click_through(&mut self) {
        // SAFETY: `self.hwnd` is a live window owned by this process.
        let was_enabled = unsafe { EnableWindow(self.hwnd, false) }.as_bool();
        crate::logging::log_msg(&format!(
            "overlay: click-through enabled (window was {}abled)",
            if was_enabled { "en" } else { "dis" }
        ));
    }

    pub fn hide(&mut self) {
        if self.visible {
            // SAFETY: `self.hwnd` is the live overlay window owned by this process.
            let _ = unsafe { ShowWindow(self.hwnd, SW_HIDE) };
            self.visible = false;
        }
        self.last_rect = None;
    }

    pub fn show_at(&mut self, x: i32, y: i32, width: i32, height: i32) {
        if width <= 0 || height <= 0 {
            self.hide();
            return;
        }
        let rect = (x, y, width, height);
        if self.visible && self.last_rect == Some(rect) {
            return;
        }
        // SAFETY: `self.hwnd` is live; HWND_TOPMOST / SWP_* do not borrow Rust data.
        let _ = unsafe {
            SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
        };
        if !self.visible {
            // SAFETY: Showing the overlay must not steal focus from the game.
            let _ = unsafe { ShowWindow(self.hwnd, SW_SHOWNOACTIVATE) };
            self.visible = true;
        }
        self.last_rect = Some(rect);
    }

    pub fn pump_messages() {
        let mut msg = MSG::default();
        // SAFETY: `msg` is a valid MSG and this runs on the host UI thread.
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            // SAFETY: `msg` was populated by PeekMessageW.
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        // SAFETY: The host owns this window; destroy it before process exit.
        // `Presenter` is declared before `Overlay` in `App`, so the wgpu surface
        // is dropped before this runs.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// True only when the game's main window currently owns the foreground. If a
/// game dialog owns it, the overlay is hidden so the dialog remains visible.
pub fn game_visible(game: HWND) -> bool {
    // SAFETY: All calls take scalar/window handles and do not borrow Rust data.
    unsafe {
        IsWindow(Some(game)).as_bool()
            && IsWindowVisible(game).as_bool()
            && !IsIconic(game).as_bool()
            && GetForegroundWindow() == game
    }
}

/// Position of the game's client area in desktop coordinates.
pub fn client_screen_rect(game: HWND) -> Option<(i32, i32, i32, i32)> {
    // SAFETY: `rect` and `origin` are valid writable stack values.
    unsafe {
        if !IsWindow(Some(game)).as_bool() {
            return None;
        }
        let mut rect = RECT::default();
        GetClientRect(game, &mut rect).ok()?;
        if rect.right <= 0 || rect.bottom <= 0 {
            return None;
        }
        let mut origin = POINT { x: 0, y: 0 };
        if !ClientToScreen(game, &mut origin).as_bool() {
            return None;
        }
        Some((origin.x, origin.y, rect.right, rect.bottom))
    }
}

#[cfg(test)]
mod tests {
    use super::frame_point;

    #[test]
    fn forwarded_click_follows_the_native_centred_viewport() {
        // An 800x600 frame in a 1920x1080 client stays 1:1 and is centred at (560, 240).
        assert_eq!(frame_point(560, 240, 1920, 1080, 800, 600, true), (0, 0));
        assert_eq!(
            frame_point(1359, 839, 1920, 1080, 800, 600, true),
            (799, 599)
        );
        assert_eq!(
            frame_point(959, 539, 1920, 1080, 800, 600, true),
            (399, 299)
        );
        // Clicks in the black bars clamp to the frame edge instead of going out of range.
        assert_eq!(frame_point(0, 540, 1920, 1080, 800, 600, true), (0, 300));
        assert_eq!(
            frame_point(1919, 540, 1920, 1080, 800, 600, true),
            (799, 300)
        );
        // Without `maintas` the whole client area is stretched onto the frame.
        assert_eq!(frame_point(0, 0, 1920, 1080, 800, 600, false), (0, 0));
        assert_eq!(
            frame_point(1919, 1079, 1920, 1080, 800, 600, false),
            (799, 599)
        );
    }

    #[test]
    fn forwarded_click_matches_the_presenter_when_the_window_is_upscaled() {
        // A 1920x1080 frame in a slightly smaller 1863x1022 window: 1816x1022 at x=23.
        assert_eq!(frame_point(23, 0, 1863, 1022, 1920, 1080, true), (0, 0));
        assert_eq!(
            frame_point(931, 511, 1863, 1022, 1920, 1080, true),
            (960, 539)
        );
    }
}
