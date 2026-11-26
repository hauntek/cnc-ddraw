//! Pure Rust decision logic for Win32 API hooks, `WH_GETMESSAGE`, and host WndProc subclassing.
#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BITSPIXEL, HORZRES, LOGPIXELSX, LOGPIXELSY, VERTRES};
use windows::Win32::UI::WindowsAndMessaging::{
    MSG, SET_WINDOW_POS_FLAGS, SM_CXSCREEN, SM_CYSCREEN, WM_ACTIVATE, WM_ACTIVATEAPP, WM_KILLFOCUS,
    WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_MOUSEWHEEL, WM_MOVE, WM_NCACTIVATE, WM_RBUTTONDOWN, WM_SIZE,
    WM_SYSCHAR, WM_SYSKEYDOWN, WM_TIMER, WM_WINDOWPOSCHANGED,
};

use crate::config;
use crate::win32::geometry::{
    client_to_screen, clip_cursor, cursor_pos, get_capture, map_rect, screen_to_client,
    set_cursor_pos, window_from_point, window_rect,
};
use crate::win32::hwnd::{
    bring_window_to_top, desktop_window, kill_timer, move_window, set_window_pos,
};

use super::input::{
    client_origin_in_bound, client_to_logical, hit_test_logical_child, is_mouse_message,
    is_related_dialog_window, logical_to_client,
};
use super::overlay::ensure_present_on_top_if_active;
use super::{
    bound_hwnd, is_present_hwnd, is_scaling_active, logical_mode, FLUSH_TIMER_ID,
    WM_APP_FLUSH,
};

pub(super) fn virtual_window_rect(hwnd: HWND, include_dialogs: bool) -> Option<RECT> {
    let (bound, w, h, _) = logical_mode()?;
    if hwnd == bound || hwnd == desktop_window() {
        return Some(RECT {
            left: 0,
            top: 0,
            right: w as i32,
            bottom: h as i32,
        });
    }
    if include_dialogs && is_related_dialog_window(bound, hwnd) {
        return Some(map_rect(None, Some(bound), window_rect(hwnd).ok()?));
    }
    None
}

pub(super) fn virtual_system_metrics(index: i32) -> Option<i32> {
    let (_, w, h, _) = logical_mode()?;
    if index == SM_CXSCREEN.0 {
        return Some(w as i32);
    }
    if index == SM_CYSCREEN.0 {
        return Some(h as i32);
    }
    None
}

pub(super) fn virtual_client_to_screen(hwnd: HWND, pt: &mut POINT) -> Option<bool> {
    let (bound, _, _, _) = logical_mode()?;
    if hwnd == bound {
        return Some(true);
    }
    if is_related_dialog_window(bound, hwnd) {
        return Some(client_to_screen(hwnd, pt) && screen_to_client(bound, pt));
    }
    None
}

pub(super) fn virtual_screen_to_client(hwnd: HWND, pt: &mut POINT) -> Option<bool> {
    let (bound, _, _, _) = logical_mode()?;
    if hwnd == bound {
        return Some(true);
    }
    if is_related_dialog_window(bound, hwnd) {
        return Some(client_to_screen(bound, pt) && screen_to_client(hwnd, pt));
    }
    None
}

pub(super) fn virtual_cursor_pos() -> Option<POINT> {
    let mut raw = cursor_pos().ok()?;
    if let Some((bound, w, h, _)) = logical_mode() {
        if screen_to_client(bound, &mut raw) {
            let (lx, ly) = client_to_logical(bound, raw.x, raw.y, w, h);
            raw = POINT { x: lx, y: ly };
        }
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
    if let Some((bound, _, _, _)) = logical_mode() {
        return hit_test_logical_child(bound, point.x, point.y).unwrap_or(bound);
    }
    window_from_point(point)
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
    let Some((bound, w, h, _)) = logical_mode() else {
        return;
    };
    let is_bound = msg.hwnd == bound;
    let is_dialog = is_related_dialog_window(bound, msg.hwnd);
    if !is_bound && !is_dialog {
        return;
    }
    let Ok(mut cursor) = cursor_pos() else {
        return;
    };
    if !screen_to_client(bound, &mut cursor) {
        return;
    }
    let (lx, ly) = client_to_logical(bound, cursor.x, cursor.y, w, h);
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
    let captured = get_capture();
    let target = if captured == msg.hwnd || is_bound {
        msg.hwnd
    } else {
        hit_test_logical_child(bound, lx, ly).unwrap_or(bound)
    };
    msg.hwnd = target;
    if msg.message == WM_MOUSEWHEEL || target == bound {
        msg.lParam = LPARAM(((ly as u16 as u32) << 16 | (lx as u16 as u32)) as isize);
    } else {
        let (tx, ty) = client_origin_in_bound(bound, target);
        let cx = lx - tx;
        let cy = ly - ty;
        msg.lParam = LPARAM(((cy as u16 as u32) << 16 | (cx as u16 as u32)) as isize);
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
