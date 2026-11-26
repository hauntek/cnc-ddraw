//! Logical <-> physical coordinate conversion, Win32 dialog hit-testing, and overlay mouse routing.
#![forbid(unsafe_code)]

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    HTCLIENT, HTTRANSPARENT, MA_NOACTIVATE, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDBLCLK, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCHITTEST, WM_RBUTTONDBLCLK, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WS_DISABLED,
};

use crate::config;
use crate::win32::current_process_id;
use crate::win32::geometry::{client_rect, client_to_screen, screen_to_client, window_rect};
use crate::win32::hwnd::{
    children, class_name, def_window_proc_w, enum_thread_windows, is_child, is_visible, owner,
    parent, post_message, send_message_w, style, window_process_id, window_thread_id,
};

use super::overlay::keep_present_on_top;
use super::{bound_hwnd, bound_thread_id, compute_viewport, is_present_hwnd, logical_mode};

const SS_NOTIFY: u32 = 0x0100;

pub(crate) fn client_to_logical(hwnd: HWND, px: i32, py: i32, lw: u32, lh: u32) -> (i32, i32) {
    let client = client_rect(hwnd).unwrap_or_default();
    if client.right <= 0 || client.bottom <= 0 {
        return (px.clamp(0, lw as i32 - 1), py.clamp(0, lh as i32 - 1));
    }
    let (vx, vy, vw, vh) = compute_viewport(
        lw,
        lh,
        client.right as u32,
        client.bottom as u32,
        config::get().maintas,
    );
    let rx = (px - vx as i32).clamp(0, vw as i32 - 1) as i64;
    let ry = (py - vy as i32).clamp(0, vh as i32 - 1) as i64;
    let lx = ((rx * lw as i64) / vw.max(1) as i64).clamp(0, lw as i64 - 1) as i32;
    let ly = ((ry * lh as i64) / vh.max(1) as i64).clamp(0, lh as i64 - 1) as i32;
    (lx, ly)
}

pub(crate) fn logical_to_client(hwnd: HWND, lx: i32, ly: i32, lw: u32, lh: u32) -> (i32, i32) {
    let client = client_rect(hwnd).unwrap_or_default();
    if client.right <= 0 || client.bottom <= 0 {
        return (lx, ly);
    }
    let (vx, vy, vw, vh) = compute_viewport(
        lw,
        lh,
        client.right as u32,
        client.bottom as u32,
        config::get().maintas,
    );
    let cx = vx as i32 + ((lx.clamp(0, lw as i32 - 1) as i64 * vw as i64) / lw as i64) as i32;
    let cy = vy as i32 + ((ly.clamp(0, lh as i32 - 1) as i64 * vh as i64) / lh as i64) as i32;
    (cx, cy)
}

fn window_rect_in_bound(bound: HWND, hwnd: HWND) -> Option<RECT> {
    let mut rc = window_rect(hwnd).ok()?;
    let mut origin = POINT { x: 0, y: 0 };
    if client_to_screen(bound, &mut origin) {
        rc.left -= origin.x;
        rc.right -= origin.x;
        rc.top -= origin.y;
        rc.bottom -= origin.y;
    }
    Some(rc)
}

pub(crate) fn client_origin_in_bound(bound: HWND, hwnd: HWND) -> (i32, i32) {
    let mut target_origin = POINT { x: 0, y: 0 };
    let mut bound_origin = POINT { x: 0, y: 0 };
    if client_to_screen(hwnd, &mut target_origin) && client_to_screen(bound, &mut bound_origin) {
        (
            target_origin.x - bound_origin.x,
            target_origin.y - bound_origin.y,
        )
    } else if let Some(rc) = window_rect_in_bound(bound, hwnd) {
        (rc.left, rc.top)
    } else {
        (0, 0)
    }
}

fn is_non_interactive_static(hwnd: HWND) -> bool {
    let Some(name) = class_name(hwnd) else {
        return false;
    };
    if name.eq_ignore_ascii_case("Static") {
        return (style(hwnd) as u32) & SS_NOTIFY == 0;
    }
    false
}

fn is_on_game_thread(hwnd: HWND) -> bool {
    let tid = bound_thread_id();
    if tid == 0 || hwnd.is_invalid() {
        return false;
    }
    window_thread_id(hwnd) == tid
}

pub(crate) fn is_same_process_window(hwnd: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    let pid = window_process_id(hwnd);
    pid != 0 && pid == current_process_id()
}

pub(crate) fn is_related_dialog_window(bound: HWND, hwnd: HWND) -> bool {
    if hwnd == bound || hwnd.is_invalid() || is_present_hwnd(hwnd) {
        return false;
    }
    if is_on_game_thread(hwnd) || is_child(bound, hwnd) {
        return true;
    }
    let mut cur = hwnd;
    for _ in 0..8 {
        if let Some(own) = owner(cur) {
            if own == bound {
                return true;
            }
            if own != cur {
                cur = own;
                continue;
            }
        }
        if let Some(par) = parent(cur) {
            if par == bound {
                return true;
            }
            if par != cur {
                cur = par;
                continue;
            }
        }
        break;
    }
    false
}

fn hit_test_tree(bound: HWND, root: HWND, px: i32, py: i32) -> Option<HWND> {
    if !is_visible(root) || (style(root) as u32) & WS_DISABLED.0 != 0 {
        return None;
    }
    let rc = window_rect_in_bound(bound, root)?;
    if px < rc.left || px >= rc.right || py < rc.top || py >= rc.bottom {
        return None;
    }
    // Descend top-to-bottom through child controls first so controls win over dialog background.
    for child in children(root) {
        if let Some(hit) = hit_test_tree(bound, child, px, py) {
            return Some(hit);
        }
    }
    if is_non_interactive_static(root) {
        return None;
    }
    Some(root)
}

/// Hit-tests the game's own window tree at a point given in *real* client pixels of `bound`.
///
/// Callers that work in the game's logical space must map through `logical_to_client` first:
/// dialogs and their controls are real windows, so mixing the two spaces silently routes input to
/// the wrong window (and offsets it by the scale factor) at every non-native resolution.
pub(crate) fn hit_test_real_child(bound: HWND, px: i32, py: i32) -> Option<HWND> {
    let tid = bound_thread_id();
    if tid != 0 {
        let mut popups = Vec::new();
        enum_thread_windows(tid, |hwnd| {
            if hwnd != bound && !is_present_hwnd(hwnd) && is_visible(hwnd) {
                popups.push(hwnd);
            }
            true
        });
        for dlg in popups {
            if let Some(hit) = hit_test_tree(bound, dlg, px, py) {
                return Some(hit);
            }
        }
    }
    for child in children(bound) {
        if let Some(hit) = hit_test_tree(bound, child, px, py) {
            return Some(hit);
        }
    }
    None
}

pub(crate) fn is_mouse_message(msg: u32) -> bool {
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
            | WM_MBUTTONDBLCLK
            | WM_MOUSEWHEEL
    )
}

/// Window procedure for the owned wgpu presentation overlay window.
/// Routes mouse input through the viewport to the matching game dialog control (or main HWND)
/// without stealing window focus.
pub(crate) fn present_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_NCHITTEST => {
            return if logical_mode().is_some() {
                LRESULT(HTCLIENT as isize)
            } else {
                LRESULT(HTTRANSPARENT as isize)
            };
        }
        WM_SETCURSOR => {
            if let Some(bound) = bound_hwnd() {
                return send_message_w(bound, WM_SETCURSOR, WPARAM(bound.0 as usize), lparam);
            }
        }
        _ if is_mouse_message(msg) => {
            if let Some((bound, w, h, _)) = logical_mode() {
                if matches!(msg, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN) {
                    keep_present_on_top(bound, hwnd);
                }
                let raw = lparam.0 as u32;
                let mut px = (raw & 0xffff) as i16 as i32;
                let mut py = ((raw >> 16) & 0xffff) as i16 as i32;
                if msg == WM_MOUSEWHEEL {
                    let mut pt = POINT { x: px, y: py };
                    if screen_to_client(bound, &mut pt) {
                        px = pt.x;
                        py = pt.y;
                    }
                }
                // The overlay client area covers the game window's real client area one-to-one,
                // so real children are found by their real geometry; only the game's own window
                // needs the coordinate space it believes in.
                let target = hit_test_real_child(bound, px, py).unwrap_or(bound);
                let target_lparam = if target == bound {
                    let (lx, ly) = client_to_logical(bound, px, py, w, h);
                    LPARAM(((ly as u16 as u32) << 16 | (lx as u16 as u32)) as isize)
                } else {
                    let (tx, ty) = client_origin_in_bound(bound, target);
                    let cx = px - tx;
                    let cy = py - ty;
                    LPARAM(((cy as u16 as u32) << 16 | (cx as u16 as u32)) as isize)
                };
                let _ = post_message(target, msg, wparam, target_lparam);
                return LRESULT(0);
            }
        }
        _ => {}
    }
    def_window_proc_w(hwnd, msg, wparam, lparam)
}
