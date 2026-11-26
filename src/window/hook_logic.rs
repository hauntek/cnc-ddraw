//! Pure Rust decision logic for Win32 API hooks, `WH_GETMESSAGE`, and host WndProc subclassing.
#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BITSPIXEL, HORZRES, LOGPIXELSX, LOGPIXELSY, VERTRES};
use windows::Win32::UI::WindowsAndMessaging::{
    MSG, SET_WINDOW_POS_FLAGS, SM_CXSCREEN, SM_CXFULLSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN,
    SM_CYFULLSCREEN, SM_CYVIRTUALSCREEN, WM_ACTIVATE, WM_ACTIVATEAPP, WM_KILLFOCUS,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_MOVE,
    WM_NCACTIVATE, WM_RBUTTONDOWN, WM_SIZE, WM_SYSCHAR, WM_SYSKEYDOWN, WM_TIMER,
    WM_WINDOWPOSCHANGED,
};

use crate::config;
use crate::win32::geometry::{
    client_to_screen, clip_cursor, cursor_pos, map_rect, screen_to_client, set_cursor_pos,
    window_from_point, window_rect,
};
use crate::win32::hwnd::{
    bring_window_to_top, desktop_window, kill_timer, move_window, set_window_pos,
};

use super::input::{
    client_to_logical, hit_test_real_child, is_mouse_message, is_related_dialog_window,
    logical_to_client,
};
use super::overlay::ensure_present_on_top_if_active;
use super::{
    bound_hwnd, is_present_hwnd, is_scaling_active, logical_mode, FLUSH_TIMER_ID,
    WM_APP_FLUSH,
};

pub(super) fn virtual_window_rect(hwnd: HWND, _include_dialogs: bool) -> Option<RECT> {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    let (bound, w, h, _) = logical_mode()?;
    if hwnd != bound && hwnd != desktop_window() {
        // Dialogs and their controls are real windows laid out in real pixels: the game positions
        // their real children from `GetWindowRect`, so handing it virtual pixels here would put
        // its idea of the dialog in a different space than the controls it is talking to.
        return None;
    }
    // Sample what the game believes its window size is: if it ever sees the
    // real 1280x960 here, its own cursor-to-surface maths would scale a
    // second time on top of the hook below.
    if CALLS.fetch_add(1, Ordering::Relaxed) % 120 == 0 {
        crate::compat::log_msg(&format!(
            "clientrect: hwnd=0x{:x} returning {}x{} real={:?}",
            hwnd.0 as usize,
            w,
            h,
            window_rect(bound).ok().map(|r| (r.right - r.left, r.bottom - r.top))
        ));
    }
    Some(RECT {
        left: 0,
        top: 0,
        right: w as i32,
        bottom: h as i32,
    })
}

pub(super) fn virtual_system_metrics(index: i32) -> Option<i32> {
    let (_, w, h, _) = logical_mode()?;
    if index == SM_CXSCREEN.0 || index == SM_CXFULLSCREEN.0 || index == SM_CXVIRTUALSCREEN.0 {
        return Some(w as i32);
    }
    if index == SM_CYSCREEN.0 || index == SM_CYFULLSCREEN.0 || index == SM_CYVIRTUALSCREEN.0 {
        return Some(h as i32);
    }
    None
}

pub(super) fn virtual_client_to_screen(hwnd: HWND, _pt: &mut POINT) -> Option<bool> {
    let (bound, _, _, _) = logical_mode()?;
    // In the game's pretend world the window sits at the screen origin, so screen and client
    // space coincide for it; everything else (real dialogs and their controls) stays real.
    (hwnd == bound).then_some(true)
}

pub(super) fn virtual_screen_to_client(hwnd: HWND, _pt: &mut POINT) -> Option<bool> {
    let (bound, _, _, _) = logical_mode()?;
    (hwnd == bound).then_some(true)
}

pub(super) fn virtual_cursor_pos() -> Option<POINT> {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    let mut raw = cursor_pos().ok()?;
    let physical = raw;
    if let Some((bound, w, h, _)) = logical_mode() {
        if screen_to_client(bound, &mut raw) {
            let (lx, ly) = client_to_logical(bound, raw.x, raw.y, w, h);
            raw = POINT { x: lx, y: ly };
        }
    }
    // Sample what the game's own polling sees, so it can be compared with the
    // coordinates the click message carries.
    if CALLS.fetch_add(1, Ordering::Relaxed) % 60 == 0 {
        crate::compat::log_msg(&format!(
            "cursorpos: screen=({},{}) returned=({},{}) logical={:?}",
            physical.x,
            physical.y,
            raw.x,
            raw.y,
            logical_mode().map(|(_, w, h, _)| (w, h))
        ));
    }
    Some(raw)
}

pub(super) fn set_virtual_cursor_pos(x: i32, y: i32) -> bool {
    if let Some((bound, w, h, _)) = logical_mode() {
        let (cx, cy) = logical_to_client(bound, x, y, w, h);
        let mut pt = POINT { x: cx, y: cy };
        if client_to_screen(bound, &mut pt) {
            return set_cursor_pos(pt.x, pt.y);
        }
    }
    set_cursor_pos(x, y)
}

pub(super) fn clip_virtual_cursor(rect: Option<&RECT>) -> bool {
    if let (Some((bound, w, h, _)), Some(r)) = (logical_mode(), rect) {
        let (l, t) = logical_to_client(bound, r.left, r.top, w, h);
        let (ri, b) = logical_to_client(
            bound,
            r.right.saturating_sub(1),
            r.bottom.saturating_sub(1),
            w,
            h,
        );
        let mapped = map_rect(
            Some(bound),
            None,
            RECT {
                left: l,
                top: t,
                right: ri + 1,
                bottom: b + 1,
            },
        );
        return clip_cursor(Some(&mapped));
    }
    clip_cursor(rect)
}

pub(super) fn virtual_clip_cursor() -> Option<RECT> {
    let (_, w, h, _) = logical_mode()?;
    Some(RECT {
        left: 0,
        top: 0,
        right: w as i32,
        bottom: h as i32,
    })
}

pub(super) fn virtual_window_from_point(point: POINT) -> HWND {
    if let Some((bound, w, h, _)) = logical_mode() {
        // The game asks in its own (logical) space; the windows it may hit are real, so map the
        // query into real client pixels before hit-testing the game's own window tree.
        let (px, py) = logical_to_client(bound, point.x, point.y, w, h);
        return hit_test_real_child(bound, px, py).unwrap_or(bound);
    }
    window_from_point(point)
}

/// Position of the message currently being retrieved, in the game's pretend screen space.
///
/// `GetMessagePos` returns real screen pixels, which is one of the few channels through which a
/// windowed game can still observe the true window geometry; virtualizing it keeps every mouse
/// position the game can read in the same 800x600 space as `GetCursorPos` and the message lParam.
pub(super) fn virtual_message_pos() -> Option<(i32, i32)> {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    let (bound, w, h, _) = logical_mode()?;
    let mut pt = cursor_pos().ok()?;
    if !screen_to_client(bound, &mut pt) {
        return None;
    }
    let (lx, ly) = client_to_logical(bound, pt.x, pt.y, w, h);
    if CALLS.fetch_add(1, Ordering::Relaxed) % 240 == 0 {
        crate::compat::log_msg(&format!("getmsgpos: returning=({},{})", lx, ly));
    }
    Some((lx, ly))
}

pub(super) fn handle_set_window_pos(
    hwnd: HWND,
    insert_after: HWND,
    x: i32,
    y: i32,
    cx: i32,
    cy: i32,
    flags: u32,
) -> bool {
    if is_scaling_active() && bound_hwnd() == Some(hwnd) {
        return true;
    }
    let ok = set_window_pos(
        hwnd,
        Some(insert_after),
        x,
        y,
        cx,
        cy,
        SET_WINDOW_POS_FLAGS(flags),
    )
    .is_ok();
    if let Some((bound, _, _, _)) = logical_mode() {
        if !is_present_hwnd(hwnd) && is_related_dialog_window(bound, hwnd) {
            ensure_present_on_top_if_active(bound);
        }
    }
    ok
}

pub(super) fn handle_bring_window_to_top(hwnd: HWND) -> bool {
    let ok = bring_window_to_top(hwnd);
    if let Some((bound, _, _, _)) = logical_mode() {
        if !is_present_hwnd(hwnd) && (hwnd == bound || is_related_dialog_window(bound, hwnd)) {
            ensure_present_on_top_if_active(bound);
        }
    }
    ok
}

pub(super) fn handle_move_window(
    hwnd: HWND,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    repaint: bool,
) -> bool {
    if is_scaling_active() && bound_hwnd() == Some(hwnd) {
        return true;
    }
    move_window(hwnd, x, y, width, height, repaint)
}

/// Client size the game should believe in, for window-geometry messages it parses directly.
pub(super) fn virtual_client_size() -> Option<(u32, u32)> {
    let (_, w, h, _) = logical_mode()?;
    Some((w, h))
}

pub(super) fn virtual_device_caps(index: i32) -> Option<i32> {
    if bound_hwnd().is_some() && (index == LOGPIXELSX.0 as i32 || index == LOGPIXELSY.0 as i32) {
        return Some(96);
    }
    let (_, w, h, bpp) = logical_mode()?;
    if index == BITSPIXEL.0 as i32 && bpp > 0 {
        return Some(bpp as i32);
    }
    if index == HORZRES.0 as i32 {
        return Some(w as i32);
    }
    if index == VERTRES.0 as i32 {
        return Some(h as i32);
    }
    None
}

pub(super) fn rewrite_getmsg(msg: &mut MSG) {
    let Some((bound, _, _, _)) = logical_mode() else {
        return;
    };
    if msg.hwnd != bound {
        // Real dialogs and their controls live in real pixels; the window manager already routed
        // this message by the real cursor position, so leave both target and coordinates alone.
        return;
    }
    let Some((lx, ly)) = virtual_message_pos() else {
        return;
    };
    let was_click = msg.message == WM_LBUTTONDOWN;
    let original = msg.pt;
    msg.pt = POINT { x: lx, y: ly };
    if !is_mouse_message(msg.message) {
        return;
    }
    if matches!(
        msg.message,
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN
    ) {
        ensure_present_on_top_if_active(bound);
    }
    // Mouse messages carry client-relative coordinates for the receiving window, and the game
    // believes this window is 800x600 with the cursor reported in the same space.
    msg.lParam = LPARAM(((ly as u16 as u32) << 16 | (lx as u16 as u32)) as isize);
    if was_click {
        crate::compat::log_msg(&format!(
            "getmsg: from=0x{:x} msg_pt=({},{}) logical=({},{}) target=0x{:x}",
            bound.0 as usize,
            original.x,
            original.y,
            lx,
            ly,
            msg.hwnd.0 as usize
        ));
    }
}

pub(super) enum SubclassAction {
    Return(LRESULT),
    DefWindowProc,
    Forward(LPARAM),
}

pub(super) fn handle_subclass_message(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    activate_passed: &AtomicBool,
    activateapp_passed: &AtomicBool,
) -> SubclassAction {
    if msg == WM_APP_FLUSH {
        crate::draw::flush_pending_present(false);
        return SubclassAction::Return(LRESULT(0));
    }
    if msg == WM_TIMER && wparam.0 == FLUSH_TIMER_ID {
        kill_timer(hwnd, FLUSH_TIMER_ID);
        crate::draw::flush_pending_present(true);
        return SubclassAction::Return(LRESULT(0));
    }
    let toggle_vk = config::get().toggle_fullscreen_vk;
    if toggle_vk != 0 {
        let alt_down = (lparam.0 as u32) & (1 << 29) != 0;
        let was_down = (lparam.0 as u32) & (1 << 30) != 0;
        if msg == WM_SYSKEYDOWN && wparam.0 as u32 == toggle_vk && alt_down {
            if !was_down {
                crate::draw::toggle_fullscreen_hotkey();
            }
            return SubclassAction::Return(LRESULT(0));
        }
        if msg == WM_SYSCHAR && wparam.0 as u32 == toggle_vk && alt_down {
            // Suppress default Windows Alt+Enter system beep.
            return SubclassAction::Return(LRESULT(0));
        }
    }
    let shield_focus = is_scaling_active()
        && (config::get().noactivateapp || !crate::draw::is_fullscreen_active());
    if shield_focus && matches!(msg, WM_KILLFOCUS | WM_NCACTIVATE) {
        return SubclassAction::DefWindowProc;
    }
    if msg == WM_ACTIVATE && shield_focus {
        let active = (wparam.0 & 0xffff) != 0;
        if !activate_passed.swap(true, Ordering::Relaxed) && active {
            // Allow the initial activation through once so the game initializes its focus flag.
        } else {
            return SubclassAction::Return(LRESULT(0));
        }
    }
    if msg == WM_ACTIVATEAPP {
        if wparam.0 == 0 {
            let _ = clip_cursor(None);
        } else {
            crate::draw::on_host_window_changed();
        }
        if shield_focus {
            if !activateapp_passed.swap(true, Ordering::Relaxed) && wparam.0 != 0 {
                // Allow initial activation once.
            } else {
                return SubclassAction::Return(LRESULT(0));
            }
        }
    }
    if matches!(msg, WM_MOVE | WM_SIZE | WM_WINDOWPOSCHANGED) && bound_hwnd() == Some(hwnd) {
        crate::draw::on_host_window_changed();
        // `WM_MOVE` carries the real client-area origin in screen pixels. The game's window is
        // pretended to sit at the screen origin - that is what makes `ScreenToClient` and
        // `GetCursorPos` agree - so report (0, 0) like `GetWindowRect` does. A game that subtracts
        // this origin from the cursor position would otherwise shift every click by the real
        // window offset.
        if msg == WM_MOVE && logical_mode().is_some() {
            let raw = lparam.0 as u32;
            static MOVED: AtomicBool = AtomicBool::new(false);
            if !MOVED.swap(true, Ordering::Relaxed) {
                crate::compat::log_msg(&format!(
                    "move: hwnd=0x{:x} real=({},{}) returning (0,0)",
                    hwnd.0 as usize,
                    (raw & 0xffff) as i16 as i32,
                    ((raw >> 16) & 0xffff) as i16 as i32
                ));
            }
            return SubclassAction::Forward(LPARAM(0));
        }
        // `WM_SIZE` is the last channel through which the game can still learn the real client
        // size: `lParam` carries 1280x960 for a window that `GetClientRect` reports as 800x600.
        // Engines cache exactly this as their view/window size and then scale mouse positions by
        // it, which lands clicks at the wrong place. Report the surface size instead.
        if msg == WM_SIZE {
            let raw = lparam.0 as u32;
            let cx = raw & 0xffff;
            let cy = (raw >> 16) & 0xffff;
            match logical_mode() {
                Some((bound, w, h, _))
                    if hwnd == bound && cx != 0 && cy != 0 && (cx != w || cy != h) =>
                {
                    crate::compat::log_msg(&format!(
                        "size: hwnd=0x{:x} real={}x{} returning {}x{}",
                        hwnd.0 as usize,
                        cx,
                        cy,
                        w,
                        h
                    ));
                    return SubclassAction::Forward(LPARAM(
                        (((h & 0xffff) << 16) | (w & 0xffff)) as isize
                    ));
                }
                _ => {
                    // One-shot: a size the game sees before the surface size is known can pin its
                    // mouse maths to the real client size for the rest of the run.
                    static UNVIRTUALIZED: AtomicBool = AtomicBool::new(false);
                    if cx != 0 && cy != 0 && !UNVIRTUALIZED.swap(true, Ordering::Relaxed) {
                        crate::compat::log_msg(&format!(
                            "size: hwnd=0x{:x} unvirtualized real={}x{} logical={:?}",
                            hwnd.0 as usize,
                            cx,
                            cy,
                            logical_mode().map(|(_, w, h, _)| (w, h))
                        ));
                    }
                }
            }
        }
    }
    if msg == WM_LBUTTONDOWN || msg == WM_LBUTTONUP {
        let raw = lparam.0 as u32;
        crate::compat::log_msg(&format!(
            "subclass: hwnd=0x{:x} msg=0x{:x} lparam=({},{}) wparam=0x{:x}",
            hwnd.0 as usize,
            msg,
            (raw & 0xffff) as i16 as i32,
            ((raw >> 16) & 0xffff) as i16 as i32,
            wparam.0
        ));
    }
    if msg == WM_MOUSEMOVE {
        static MOVES: AtomicUsize = AtomicUsize::new(0);
        if MOVES.fetch_add(1, Ordering::Relaxed) % 120 == 0 {
            let raw = lparam.0 as u32;
            crate::compat::log_msg(&format!(
                "subclass: hwnd=0x{:x} move=({},{})",
                hwnd.0 as usize,
                (raw & 0xffff) as i16 as i32,
                ((raw >> 16) & 0xffff) as i16 as i32
            ));
        }
    }
    // No coordinate remapping here: every mouse message is posted to the thread
    // queue, so it already went through the `WH_GETMESSAGE` hook, which rewrote
    // the coordinates from window pixels to the game's logical resolution. Doing
    // it a second time in the window procedure scaled the coordinates twice
    // whenever no in-process presenter overlay existed - which is exactly the
    // `presenter = host` case - so clicks landed at 1/scale of where the player
    // actually aimed (a 1280x960 window turned a 800x600-logical click into
    // 0.625x of it) while the polled cursor stayed correct.
    SubclassAction::Forward(lparam)
}
