//! Borderless, click-through overlay owned by the 64-bit host. D3DMetal
//! therefore presents into a window created by the same process, while the
//! 32-bit game keeps focus, input and its own dialogs.
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{ClientToScreen, ValidateRect};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetForegroundWindow, HTTRANSPARENT, HWND_TOPMOST, IsIconic, IsWindow, IsWindowVisible,
    MA_NOACTIVATE, MSG, PM_REMOVE, PeekMessageW, PostMessageW, RegisterClassW, SWP_NOACTIVATE,
    SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNOACTIVATE, SetWindowPos, ShowWindow, TranslateMessage,
    WM_ERASEBKGND, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_NCHITTEST,
    WM_PAINT, WM_RBUTTONDBLCLK, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
    WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};

/// Game window that receives mouse input the overlay happens to receive.
static INPUT_TARGET: AtomicUsize = AtomicUsize::new(0);

/// Register the window that should receive forwarded mouse input. Must be
/// called before the overlay is shown.
pub fn set_input_target(game: HWND) {
    INPUT_TARGET.store(game.0 as usize, Ordering::Relaxed);
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
    // The overlay covers exactly the game's client area, so client-relative
    // mouse coordinates are already valid for the game window and can be
    // forwarded unchanged. `WM_MOUSEWHEEL`/`WM_MOUSEHWHEEL` carry screen
    // coordinates in `wparam`, which need no translation either.
    if is_mouse_input(msg) {
        let target = INPUT_TARGET.load(Ordering::Relaxed);
        if target != 0 {
            let game = HWND(target as *mut c_void);
            // SAFETY: `game` is a window handle supplied by the producer; this
            // only enqueues a message and never dereferences Rust data.
            if unsafe { PostMessageW(Some(game), msg, wparam, lparam) }.is_ok() {
                return LRESULT(0);
            }
        }
    }
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
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
    pub fn new() -> Result<Self, String> {
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
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT,
                class_name,
                w!(""),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(hinstance),
                None,
            )
        }
        .map_err(|err| format!("CreateWindowExW: {err}"))?;

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

    /// Take the overlay out of mouse hit-testing entirely.
    ///
    /// `WS_EX_TRANSPARENT` plus `WM_NCHITTEST -> HTTRANSPARENT` is enough on
    /// native Windows, but Wine dispatches mouse input straight to the window
    /// under the cursor without running the hit-test retry loop, so a topmost
    /// popup swallows every click while the game keeps the foreground and stays
    /// visible behind it. A *disabled* window is documented to never receive
    /// mouse input and is skipped by `WindowFromPoint`, which both Windows and
    /// Wine honour.
    ///
    /// Called only after the swapchain has been created for `hwnd`, so DXGI
    /// never has to deal with a disabled window during surface creation.
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
