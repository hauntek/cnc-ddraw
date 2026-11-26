//! Borderless, click-through overlay owned by the 64-bit host. D3DMetal
//! therefore presents into a window created by the same process, while the
//! 32-bit game keeps focus, input and its own dialogs.
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{ClientToScreen, ValidateRect};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetForegroundWindow, HTTRANSPARENT, HWND_TOPMOST, IsIconic, IsWindow, IsWindowVisible,
    MA_NOACTIVATE, MSG, PM_REMOVE, PeekMessageW, RegisterClassW, SWP_NOACTIVATE, SWP_SHOWWINDOW,
    SW_HIDE, SW_SHOWNOACTIVATE, SetWindowPos, ShowWindow, TranslateMessage, WM_ERASEBKGND,
    WM_MOUSEACTIVATE, WM_NCHITTEST, WM_PAINT, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TRANSPARENT, WS_POPUP,
};

unsafe extern "system" fn overlay_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
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
