//! COM DirectDraw 1/2/4/7 implementation and top-level DirectDraw exports.
//! All display/presentation state lives in `display` (`#![forbid(unsafe_code)]`).
mod display;

pub use display::{
    flush_pending_present, is_fullscreen_active, on_host_window_changed, toggle_fullscreen_hotkey,
    DisplayShared,
};

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::{
    implement, s, w, Error, IUnknown, Interface, OutRef, Ref, Result, BOOL, GUID, HRESULT,
};
use windows::Win32::Foundation::{
    E_FAIL, E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, E_POINTER, HANDLE, HWND, SIZE, S_OK,
};
use windows::Win32::Graphics::DirectDraw::*;
use windows::Win32::Graphics::Gdi::{HDC, HMONITOR, PALETTEENTRY};

use crate::ffi::{descriptor, ptr as ffi_ptr};
use crate::renderer::Renderer;
use crate::win32::geometry::monitor_refresh_hz;
use crate::win32::AtomicHwnd;
use crate::window::{self, HostWindow, PresentationMode};
use crate::{clipper, compat, config, palette, surface};

use display::{
    build_driver_caps, fill_device_identifier, fill_device_identifier2, make_hwnd,
    mode_description, set_active_display, WindowTransitionGuard, DEFAULT_MODE,
    VIRTUAL_VIDMEM_BYTES,
};

const ALREADY_INITIALIZED: HRESULT = HRESULT(0x8876_0005_u32 as i32);
const DDERR_CURRENTLYNOTAVAIL: HRESULT = HRESULT(0x8876_0028_u32 as i32);
const DDERR_INVALIDOBJECT: HRESULT = HRESULT(0x8876_0082_u32 as i32);
const DDERR_NOTFOUND: HRESULT = HRESULT(0x8876_00ff_u32 as i32);
const DDERR_VERTICALBLANKINPROGRESS: HRESULT = HRESULT(0x8876_0219_u32 as i32);

#[implement(IDirectDraw, IDirectDraw2, IDirectDraw4, IDirectDraw7, Agile = false)]
struct Draw {
    display: Arc<DisplayShared>,
    hwnd: AtomicHwnd,
    bind_id: AtomicUsize,
    exclusive: AtomicBool,
    manage_window: AtomicBool,
    changing_window: AtomicBool,
    vblank_epoch: Instant,
    cached_hz: AtomicU32,
    initialized: AtomicBool,
}

impl Draw {
    fn new_with_init(initialized: bool) -> Self {
        let (width, height, bpp) = DEFAULT_MODE;
        Self {
            display: DisplayShared::new(width, height, bpp),
            hwnd: AtomicHwnd::new(),
            bind_id: AtomicUsize::new(0),
            exclusive: AtomicBool::new(false),
            manage_window: AtomicBool::new(false),
            changing_window: AtomicBool::new(false),
            vblank_epoch: Instant::now(),
            cached_hz: AtomicU32::new(0),
            initialized: AtomicBool::new(initialized),
        }
    }

    fn create_interface(initialized: bool) -> Result<IDirectDraw> {
        let draw = Self::new_with_init(initialized);
        let display = draw.display.clone();
        let dd: IDirectDraw = draw.into();
        display.lock().owner_draw = dd.downgrade()?;
        Ok(dd)
    }

    fn current_hwnd(&self) -> HWND {
        self.hwnd.load().unwrap_or_default()
    }
}

pub fn create_uninitialized_draw() -> Result<IDirectDraw> {
    Draw::create_interface(false)
}

impl Drop for Draw {
    fn drop(&mut self) {
        let id = self.bind_id.load(Ordering::Acquire);
        let owns_hooks = window::is_active_owner(id);
        {
            let mut display = self.display.lock();
            if let Some(saved) = display.set_host(None) {
                if owns_hooks {
                    let _ = saved.restore();
                }
            }
        }
        if id != 0 {
            window::unbind_window(id);
        }
    }
}

impl Draw_Impl {
    fn refresh_cached_hz(&self) -> u32 {
        let resolved = monitor_refresh_hz(self.current_hwnd())
            .filter(|hz| (24..=500).contains(hz))
            .unwrap_or(60);
        self.cached_hz.store(resolved, Ordering::Relaxed);
        resolved
    }

    fn monitor_hz(&self) -> u32 {
        let cached = self.cached_hz.load(Ordering::Relaxed);
        if cached != 0 {
            cached
        } else {
            self.refresh_cached_hz()
        }
    }

    /// Returns `(phase_us, active_end_us, frame_us, display_height)`.
    fn frame_timing(&self) -> (u64, u64, u64, u32) {
        let hz = self.monitor_hz() as u64;
        let frame_us = (1_000_000 / hz).max(1_000);
        let active_end_us = frame_us * 92 / 100;
        let elapsed_us = self.vblank_epoch.elapsed().as_micros() as u64;
        let phase_us = elapsed_us % frame_us;
        let height = self.display.height();
        (phase_us, active_end_us, frame_us, height)
    }

    /// # Safety
    /// The caller provides a sized descriptor and any initial `DDSD_LPSURFACE` pixels.
    unsafe fn create_surface_v2<S>(
        &self,
        desc: *mut DDSURFACEDESC2,
        out: OutRef<S>,
        outer: Ref<IUnknown>,
    ) -> Result<()>
    where
        S: Interface + windows_core::Type<S, Default = Option<S>>,
    {
        ffi_ptr::put_interface(out, || {
            ffi_ptr::no_aggregation(outer)?;
            // SAFETY: Caller provides a valid `DDSURFACEDESC2` and optional `DDSD_LPSURFACE` image.
            let desc = unsafe { descriptor::read(desc)? };
            // SAFETY: `desc` came directly from this COM call with caller-backed `lpSurface`.
            let surf = unsafe { surface::create_surface(self.display.clone(), &desc)? };
            surf.cast::<S>()
        })
    }

    fn duplicate_surface_impl<S: Interface>(&self, src: Ref<S>) -> Result<S> {
        let src_v1 = src.ok()?.cast::<IDirectDrawSurface>()?;
        let dup = surface::duplicate_surface(self.display.clone(), &src_v1)?;
        dup.cast::<S>()
    }

    fn enum_surfaces_impl<S: Interface>(
        &self,
        flags: u32,
        filter: Option<DDSURFACEDESC>,
        mut emit: impl FnMut(S, &mut DDSURFACEDESC) -> i32,
    ) -> Result<()> {
        let allowed = (DDENUMSURFACES_ALL
            | DDENUMSURFACES_MATCH
            | DDENUMSURFACES_NOMATCH
            | DDENUMSURFACES_CANBECREATED
            | DDENUMSURFACES_DOESEXIST) as u32;
        if flags & !allowed != 0 {
            return Err(E_INVALIDARG.into());
        }
        let all = flags & DDENUMSURFACES_ALL as u32 != 0;
        let matched = flags & DDENUMSURFACES_MATCH as u32 != 0;
        let nomatch = flags & DDENUMSURFACES_NOMATCH as u32 != 0;
        let can_create = flags & DDENUMSURFACES_CANBECREATED as u32 != 0;
        let exists = flags & DDENUMSURFACES_DOESEXIST as u32 != 0;
        if (all as u8 + matched as u8 + nomatch as u8) != 1
            || (can_create as u8 + exists as u8) != 1
        {
            return Err(E_INVALIDARG.into());
        }
        if (matched || nomatch) && filter.is_none() {
            return Err(E_INVALIDARG.into());
        }
        if can_create {
            return Ok(());
        }
        let snapshot = self.display.live_surfaces();
        for surf in snapshot {
            let keep = if all {
                true
            } else {
                let is_match = surface::matches_desc(&surf, filter.as_ref().unwrap())?;
                if matched {
                    is_match
                } else {
                    !is_match
                }
            };
            if !keep {
                continue;
            }
            let mut desc = surface::surface_desc(&surf)?;
            let typed = surf.cast::<S>()?;
            if emit(typed, &mut desc) == DDENUMRET_CANCEL as i32 {
                break;
            }
        }
        Ok(())
    }

    fn enum_display_modes_impl(
        &self,
        flags: u32,
        filter: DDSURFACEDESC,
        mut emit: impl FnMut(&mut DDSURFACEDESC) -> i32,
    ) -> Result<()> {
        if flags & !((DDEDM_REFRESHRATES | DDEDM_STANDARDVGAMODES) as u32) != 0 {
            return Err(E_INVALIDARG.into());
        }
        if filter.dwFlags
            & !((DDSD_WIDTH | DDSD_HEIGHT | DDSD_PIXELFORMAT | DDSD_REFRESHRATE) as u32)
            != 0
        {
            return Err(E_INVALIDARG.into());
        }
        let (cur_w, cur_h, _) = self.display.mode();
        let current_size = (cur_w, cur_h);
        let refresh = self.monitor_hz();
        let include_refresh = flags & DDEDM_REFRESHRATES as u32 != 0;
        let sizes = [
            (320, 200),
            (320, 240),
            (640, 400),
            (640, 480),
            (800, 600),
            (1024, 768),
            (1280, 720),
            (1280, 960),
            (1280, 1024),
            (1600, 1200),
            (1920, 1080),
        ];
        let additional = (!sizes.contains(&current_size)).then_some(current_size);
        for (width, height) in sizes.into_iter().chain(additional) {
            if filter.dwFlags & DDSD_WIDTH as u32 != 0 && filter.dwWidth != width
                || filter.dwFlags & DDSD_HEIGHT as u32 != 0 && filter.dwHeight != height
            {
                continue;
            }
            for bpp in [8, 16, 32] {
                let mut mode = mode_description(width, height, bpp)?;
                if include_refresh {
                    mode.dwFlags |= DDSD_REFRESHRATE as u32;
                    mode.Anonymous2.dwRefreshRate = refresh;
                }
                if let Some(wanted) = descriptor::refresh_rate(&filter) {
                    if wanted != 0 && wanted != refresh {
                        continue;
                    }
                }
                if filter.dwFlags & DDSD_PIXELFORMAT as u32 != 0 {
                    let requested = &filter.ddpfPixelFormat;
                    let format = &mode.ddpfPixelFormat;
                    let (requested_bpp, masks) = descriptor::rgb_layout(requested);
                    if (requested.dwFlags != 0 && requested.dwFlags != format.dwFlags)
                        || (requested_bpp != 0 && requested_bpp != bpp)
                        || (masks != [0; 3] && masks != descriptor::rgb_layout(format).1)
                    {
                        continue;
                    }
                }
                if emit(&mut mode) == DDENUMRET_CANCEL as i32 {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// # Safety
    /// Each non-null output must be writable for one DWORD.
    unsafe fn get_available_vid_mem_impl(&self, total: *mut u32, free: *mut u32) -> Result<()> {
        if total.is_null() && free.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let used = self
            .display
            .allocated_bytes()
            .min(VIRTUAL_VIDMEM_BYTES as usize) as u32;
        // Write sequentially so that clients can use the same output for both.
        for (ptr, value) in [
            (total, VIRTUAL_VIDMEM_BYTES),
            (free, VIRTUAL_VIDMEM_BYTES - used),
        ] {
            if !ptr.is_null() {
                // SAFETY: Caller guarantees each non-null pointer is writable for one u32.
                unsafe { ptr.write(value) };
            }
        }
        Ok(())
    }

    fn get_surface_from_dc_impl<S: Interface>(&self, hdc: HDC) -> Result<S> {
        let surface = self.display.find_surface_by_dc(hdc).ok_or(DDERR_NOTFOUND)?;
        surface.cast::<S>()
    }
}

#[allow(non_snake_case)]
impl IDirectDraw_Impl for Draw_Impl {
    fn Compact(&self) -> Result<()> {
        Ok(())
    }

    fn CreateClipper(
        &self,
        flags: u32,
        out: OutRef<IDirectDrawClipper>,
        outer: Ref<IUnknown>,
    ) -> Result<()> {
        ffi_ptr::put_interface(out, || {
            ffi_ptr::no_aggregation(outer)?;
            clipper::create_clipper(flags, Some(self.display.clone()))
        })
    }

    fn CreatePalette(
        &self,
        flags: u32,
        entries: *mut PALETTEENTRY,
        out: OutRef<IDirectDrawPalette>,
        outer: Ref<IUnknown>,
    ) -> Result<()> {
        ffi_ptr::put_interface(out, || {
            ffi_ptr::no_aggregation(outer)?;
            // SAFETY: An 8-bit DirectDraw palette supplies 256 initial entries.
            unsafe { palette::create_palette(flags, entries, self.display.clone()) }
        })
    }

    fn CreateSurface(
        &self,
        desc: *mut DDSURFACEDESC,
        out: OutRef<IDirectDrawSurface>,
        outer: Ref<IUnknown>,
    ) -> Result<()> {
        ffi_ptr::put_interface(out, || {
            ffi_ptr::no_aggregation(outer)?;
            // SAFETY: The caller supplies a sized descriptor and any initial `DDSD_LPSURFACE` pixels.
            unsafe { surface::create_surface(self.display.clone(), &descriptor::read(desc)?) }
        })
    }

    fn DuplicateSurface(&self, src: Ref<IDirectDrawSurface>) -> Result<IDirectDrawSurface> {
        self.duplicate_surface_impl(src)
    }

    fn EnumDisplayModes(
        &self,
        flags: u32,
        filter: *mut DDSURFACEDESC,
        context: *mut c_void,
        callback: LPDDENUMMODESCALLBACK,
    ) -> Result<()> {
        let callback = callback.ok_or(E_INVALIDARG)?;
        let filter = if filter.is_null() {
            DDSURFACEDESC::default()
        } else {
            // SAFETY: Non-null filter satisfies the `DDSURFACEDESC` COM input contract.
            unsafe { descriptor::read(filter)? }
        };
        // SAFETY: Invoking caller-supplied enumeration callback with our initialized `mode` and `context`.
        self.enum_display_modes_impl(flags, filter, |mode| unsafe { callback(mode, context).0 })
    }

    fn EnumSurfaces(
        &self,
        flags: u32,
        desc: *mut DDSURFACEDESC,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK,
    ) -> Result<()> {
        let callback = callback.ok_or(E_INVALIDARG)?;
        let filter = if desc.is_null() {
            None
        } else {
            // SAFETY: Non-null `desc` satisfies the `DDSURFACEDESC` COM input contract.
            Some(unsafe { descriptor::read(desc)? })
        };
        self.enum_surfaces_impl::<IDirectDrawSurface>(flags, filter, |surf, d| {
            let owned = ManuallyDrop::new(surf);
            // SAFETY: DirectDraw transfers ownership of one surface ref to the enum callback.
            unsafe { callback((&*owned).into(), d, context).0 }
        })
    }

    fn FlipToGDISurface(&self) -> Result<()> {
        self.display.request_present_now()
    }

    fn GetCaps(&self, driver: *mut DDCAPS_DX7, software: *mut DDCAPS_DX7) -> Result<()> {
        if driver.is_null() && software.is_null() {
            return Err(E_INVALIDARG.into());
        }
        // SAFETY: Snapshot both size headers while they still contain the caller's input.
        let d_size = unsafe { descriptor::read_caps_size(driver)? };
        // SAFETY: Snapshot software caps size header before mutating either buffer.
        let s_size = unsafe { descriptor::read_caps_size(software)? };
        for (output, size) in [(driver, d_size), (software, s_size)] {
            if let Some(size) = size {
                let caps = build_driver_caps(size);
                // SAFETY: `output` was validated for `size` bytes matching a legacy DDCAPS prefix.
                unsafe { descriptor::write_caps_prefix(output, &caps, size) };
            }
        }
        Ok(())
    }

    fn GetDisplayMode(&self, desc: *mut DDSURFACEDESC) -> Result<()> {
        // SAFETY: Caller passes a sized `DDSURFACEDESC` output buffer.
        let out = unsafe { descriptor::output(desc)? };
        let (width, height, bpp) = self.display.mode();
        out.write(mode_description(width, height, bpp)?);
        Ok(())
    }

    fn GetFourCCCodes(&self, count: *mut u32, _: *mut u32) -> Result<()> {
        // SAFETY: Caller passes a writable `u32` pointer for the returned code count.
        unsafe { ffi_ptr::write_out(count, 0, E_POINTER) }
    }

    fn GetGDISurface(&self) -> Result<IDirectDrawSurface> {
        self.display
            .primary_surface()
            .ok_or_else(|| Error::from(DDERR_NOTFOUND))
    }

    fn GetMonitorFrequency(&self, output: *mut u32) -> Result<()> {
        // SAFETY: Caller passes a writable `u32` pointer for the monitor frequency.
        let slot = unsafe { ffi_ptr::out_slot(output, E_POINTER)? };
        let hz = monitor_refresh_hz(self.current_hwnd()).ok_or(E_FAIL)?;
        slot.write(hz);
        Ok(())
    }

    fn GetScanLine(&self, line: *mut u32) -> Result<()> {
        // SAFETY: Caller passes a writable `u32` pointer for the scan line.
        let out = unsafe { ffi_ptr::out_slot(line, E_INVALIDARG)? };
        let (phase_us, active_end_us, _, height) = self.frame_timing();
        if phase_us >= active_end_us {
            out.write(0);
            return Err(DDERR_VERTICALBLANKINPROGRESS.into());
        }
        out.write(((phase_us * height as u64) / active_end_us.max(1)) as u32);
        Ok(())
    }

    fn GetVerticalBlankStatus(&self, status: *mut BOOL) -> Result<()> {
        // SAFETY: Caller passes a writable `BOOL` pointer for vertical blank status.
        let out = unsafe { ffi_ptr::out_slot(status, E_INVALIDARG)? };
        let (phase_us, active_end_us, _, _) = self.frame_timing();
        out.write(BOOL::from(phase_us >= active_end_us));
        Ok(())
    }

    fn Initialize(&self, device: *mut GUID) -> Result<()> {
        if self.initialized.load(Ordering::Acquire) {
            return Err(ALREADY_INITIALIZED.into());
        }
        if !valid_device_guid(device) {
            return Err(E_INVALIDARG.into());
        }
        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    fn RestoreDisplayMode(&self) -> Result<()> {
        let _transition = WindowTransitionGuard::acquire(&self.changing_window).ok();
        window::set_scaling_active(self.bind_id.load(Ordering::Acquire), false);
        let mut display = self.display.lock();
        if let Some(saved) = display.set_host(None) {
            let _ = saved.restore();
        }
        if self.display.registry().primary.strong_count() == 0 {
            (display.width, display.height, display.bpp) = DEFAULT_MODE;
            self.display
                .store_mode(DEFAULT_MODE.0, DEFAULT_MODE.1, DEFAULT_MODE.2);
        }
        let _ = self.display.present_locked(display);
        Ok(())
    }

    fn SetCooperativeLevel(&self, hwnd: HWND, flags: u32) -> Result<()> {
        let known_flags = (DDSCL_NORMAL
            | DDSCL_FULLSCREEN
            | DDSCL_EXCLUSIVE
            | DDSCL_ALLOWREBOOT
            | DDSCL_ALLOWMODEX
            | DDSCL_NOWINDOWCHANGES
            | DDSCL_FPUSETUP
            | DDSCL_FPUPRESERVE
            | DDSCL_MULTITHREADED
            | DDSCL_CREATEDEVICEWINDOW
            | DDSCL_SETDEVICEWINDOW
            | DDSCL_SETFOCUSWINDOW) as u32;
        if flags & !known_flags != 0 {
            return Err(E_INVALIDARG.into());
        }
        let normal = flags & DDSCL_NORMAL as u32 != 0;
        let exclusive = flags & DDSCL_EXCLUSIVE as u32 != 0;
        let fullscreen = flags & DDSCL_FULLSCREEN as u32 != 0;
        let no_window_changes = flags & DDSCL_NOWINDOWCHANGES as u32 != 0;
        let window_only = flags
            & (DDSCL_SETFOCUSWINDOW | DDSCL_SETDEVICEWINDOW | DDSCL_CREATEDEVICEWINDOW) as u32
            != 0;
        if (normal && exclusive)
            || (exclusive && !fullscreen)
            || (!normal && !exclusive && !window_only)
        {
            return Err(E_INVALIDARG.into());
        }
        if (hwnd.is_invalid() && exclusive) || (!hwnd.is_invalid() && !window::is_window(hwnd)) {
            return Err(E_INVALIDARG.into());
        }
        // SetWindowPos synchronously calls the game's WndProc, which may re-enter us.
        let _transition = WindowTransitionGuard::acquire(&self.changing_window)?;
        let manage_window = exclusive && !no_window_changes;
        let cfg = config::get();
        // Overlay scaling keeps the game window at the game's own resolution and lets the
        // presenter's overlay window show it scaled up. Every coordinate the game reads then stays
        // native, which is what keeps real Win32 dialogs, their controls and the game's own art in
        // the same space - virtualizing a scaled client cannot do that.
        let windowscale = cfg.windowscale_enabled();
        let mut window_cfg = cfg.clone();
        if !windowscale {
            // Zero means "use the surface size" in `apply_windowed`, and the window is only the
            // backing surface for the game's own drawing - the overlay is what the player sees.
            window_cfg.width = 0;
            window_cfg.height = 0;
            window_cfg.border = false;
            window_cfg.resizable = false;
        }
        let change_hwnd = self.current_hwnd() != hwnd;
        {
            let display = self.display.lock();
            if !change_hwnd
                && display.presentation.host.is_some() == manage_window
                && self.exclusive.load(Ordering::Acquire) == exclusive
            {
                return Ok(());
            }
        }
        let mut display = self.display.lock();
        if let Some(saved) = display.set_host(None) {
            saved.restore()?;
        }
        self.exclusive.store(exclusive, Ordering::Release);
        self.manage_window.store(manage_window, Ordering::Release);
        // Bind *before* the host block resizes the window: that resize sends `WM_SIZE` /
        // `WM_WINDOWPOSCHANGED` carrying the real client size, and while the primary surface size
        // is still unknown there is no virtual size to report - only the subclass installed here
        // can keep the real geometry away from the game.
        if change_hwnd {
            display.presentation.bound_hwnd = make_hwnd(hwnd);
            // Renderer and 64-bit host are created lazily on the first present.
            // This keeps SetCooperativeLevel from initialising a 32-bit GPU
            // device on macOS, where the 64-bit host owns D3DMetal.
            display.presentation.renderer = None;
            display.presentation.sink = None;
            display.presentation.host_failed = false;
            display.consecutive_present_failures = 0;
            self.hwnd.store((!hwnd.is_invalid()).then_some(hwnd));
            self.cached_hz.store(0, Ordering::Relaxed);
            let prev_id = self.bind_id.load(Ordering::Acquire);
            if hwnd.is_invalid() {
                window::unbind_window(prev_id);
                self.bind_id.store(0, Ordering::Release);
            } else {
                compat::apply_single_cpu_affinity();
                let id = window::bind_window(hwnd, exclusive && windowscale);
                self.bind_id.store(id, Ordering::Release);
                set_active_display(&self.display);
            }
        } else {
            window::set_scaling_active(self.bind_id.load(Ordering::Acquire), exclusive);
        }
        if manage_window {
            let initial_mode = if windowscale && !cfg.windowed {
                PresentationMode::BorderlessFullscreen
            } else {
                PresentationMode::Windowed
            };
            // Overlay scaling keeps the window at the size the game created it with, so the game
            // never observes a client size it did not ask for.
            let (mut logical_w, mut logical_h) = (display.width, display.height);
            if !windowscale {
                if let Ok(rc) = crate::win32::geometry::client_rect(hwnd) {
                    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
                    if w > 0 && h > 0 {
                        logical_w = w as u32;
                        logical_h = h as u32;
                    }
                }
            }
            let host = HostWindow::enter(
                hwnd,
                initial_mode,
                logical_w,
                logical_h,
                &window_cfg,
            )?;
            display.set_host(Some(host));
        }
        let _ = self.display.present_locked(display);
        Ok(())
    }

    fn SetDisplayMode(&self, width: u32, height: u32, bpp: u32) -> Result<()> {
        if !(1..=8192).contains(&width)
            || !(1..=8192).contains(&height)
            || ![8, 16, 32].contains(&bpp)
        {
            return Err(E_INVALIDARG.into());
        }
        let hwnd = self.current_hwnd();
        let cfg = config::get();
        let _transition = WindowTransitionGuard::acquire(&self.changing_window)?;
        let mut display = self.display.lock();
        // Publish the logical size *before* the window is resized: the resize below sends
        // `WM_SIZE`/`WM_WINDOWPOSCHANGED` with the real client size, and a game that caches those
        // would keep scaling its mouse maths by the real window size forever after.
        let windowscale = cfg.windowscale_enabled();
        let mut window_cfg = cfg.clone();
        if !windowscale {
            // Zero means "use the surface size" in `apply_windowed`, and the window is only the
            // backing surface for the game's own drawing - the overlay is what the player sees.
            window_cfg.width = 0;
            window_cfg.height = 0;
            window_cfg.border = false;
            window_cfg.resizable = false;
        }
        let scaling =
            self.exclusive.load(Ordering::Acquire) && !hwnd.is_invalid() && windowscale;
        if scaling {
            // `set_scaling_active` clears the mode flag, so arm scaling first and re-arm the mode.
            window::set_scaling_active(self.bind_id.load(Ordering::Acquire), true);
        }
        window::set_mode(width, height, bpp, true);
        if self.exclusive.load(Ordering::Acquire) && !hwnd.is_invalid() {
            if self.manage_window.load(Ordering::Acquire) && display.presentation.host.is_none() {
                let mode = if windowscale && !cfg.windowed {
                    PresentationMode::BorderlessFullscreen
                } else {
                    PresentationMode::Windowed
                };
                let host = HostWindow::enter(hwnd, mode, width, height, &window_cfg)?;
                display.set_host(Some(host));
            } else if let Some(host) = display.presentation.host.as_mut() {
                host.on_logical_mode_changed(width, height, &window_cfg);
            }
            if scaling {
                window::set_scaling_active(self.bind_id.load(Ordering::Acquire), true);
            }
        }
        (display.width, display.height, display.bpp) = (width, height, bpp);
        self.display.store_mode(width, height, bpp);
        window::set_mode(width, height, bpp, true);
        // The window was already resized before this mode became known, so the game may have seen
        // the real client size once. Make the surface size the last `WM_SIZE` it observes.
        window::notify_logical_size();
        if let Some(renderer) = display.presentation.renderer.as_mut() {
            renderer.sync_overlay();
        }
        Ok(())
    }

    fn WaitForVerticalBlank(&self, flags: u32, event: HANDLE) -> Result<()> {
        if !event.is_invalid() {
            return Err(E_NOTIMPL.into());
        }
        if window::is_ui_thread_or_unbound() && self.display.dirty.load(Ordering::Acquire) {
            if let Some(display) = self.display.try_lock() {
                let _ = self.display.present_locked(display);
            }
        }
        let (phase_us, active_end_us, frame_us, _) = self.frame_timing();
        let wait_us = match flags as i32 {
            DDWAITVB_BLOCKBEGIN => {
                if phase_us < active_end_us {
                    active_end_us - phase_us
                } else {
                    (frame_us - phase_us) + active_end_us
                }
            }
            DDWAITVB_BLOCKEND => frame_us - phase_us,
            DDWAITVB_BLOCKBEGINEVENT => return Err(E_NOTIMPL.into()),
            _ => return Err(E_INVALIDARG.into()),
        };
        if wait_us > 0 {
            std::thread::sleep(Duration::from_micros(wait_us.min(frame_us)));
        }
        Ok(())
    }
}

macro_rules! delegate_draw_common {
    () => {
        fn Compact(&self) -> Result<()> {
            IDirectDraw_Impl::Compact(self)
        }
        fn CreateClipper(
            &self,
            flags: u32,
            out: OutRef<IDirectDrawClipper>,
            outer: Ref<IUnknown>,
        ) -> Result<()> {
            IDirectDraw_Impl::CreateClipper(self, flags, out, outer)
        }
        fn CreatePalette(
            &self,
            flags: u32,
            entries: *mut PALETTEENTRY,
            out: OutRef<IDirectDrawPalette>,
            outer: Ref<IUnknown>,
        ) -> Result<()> {
            IDirectDraw_Impl::CreatePalette(self, flags, entries, out, outer)
        }
        fn FlipToGDISurface(&self) -> Result<()> {
            IDirectDraw_Impl::FlipToGDISurface(self)
        }
        fn GetCaps(&self, driver: *mut DDCAPS_DX7, software: *mut DDCAPS_DX7) -> Result<()> {
            IDirectDraw_Impl::GetCaps(self, driver, software)
        }
        fn GetFourCCCodes(&self, count: *mut u32, codes: *mut u32) -> Result<()> {
            IDirectDraw_Impl::GetFourCCCodes(self, count, codes)
        }
        fn GetMonitorFrequency(&self, output: *mut u32) -> Result<()> {
            IDirectDraw_Impl::GetMonitorFrequency(self, output)
        }
        fn GetScanLine(&self, line: *mut u32) -> Result<()> {
            IDirectDraw_Impl::GetScanLine(self, line)
        }
        fn GetVerticalBlankStatus(&self, status: *mut BOOL) -> Result<()> {
            IDirectDraw_Impl::GetVerticalBlankStatus(self, status)
        }
        fn Initialize(&self, guid: *mut GUID) -> Result<()> {
            IDirectDraw_Impl::Initialize(self, guid)
        }
        fn RestoreDisplayMode(&self) -> Result<()> {
            IDirectDraw_Impl::RestoreDisplayMode(self)
        }
        fn SetCooperativeLevel(&self, hwnd: HWND, flags: u32) -> Result<()> {
            IDirectDraw_Impl::SetCooperativeLevel(self, hwnd, flags)
        }
        fn SetDisplayMode(
            &self,
            width: u32,
            height: u32,
            bpp: u32,
            _refresh_rate: u32,
            _flags: u32,
        ) -> Result<()> {
            IDirectDraw_Impl::SetDisplayMode(self, width, height, bpp)
        }
        fn WaitForVerticalBlank(&self, flags: u32, event: HANDLE) -> Result<()> {
            IDirectDraw_Impl::WaitForVerticalBlank(self, flags, event)
        }
    };
}

#[allow(non_snake_case)]
impl IDirectDraw2_Impl for Draw_Impl {
    delegate_draw_common!();

    fn CreateSurface(
        &self,
        desc: *mut DDSURFACEDESC,
        out: OutRef<IDirectDrawSurface>,
        outer: Ref<IUnknown>,
    ) -> Result<()> {
        IDirectDraw_Impl::CreateSurface(self, desc, out, outer)
    }
    fn DuplicateSurface(&self, src: Ref<IDirectDrawSurface>) -> Result<IDirectDrawSurface> {
        self.duplicate_surface_impl(src)
    }
    fn EnumDisplayModes(
        &self,
        flags: u32,
        filter: *mut DDSURFACEDESC,
        context: *mut c_void,
        callback: LPDDENUMMODESCALLBACK,
    ) -> Result<()> {
        IDirectDraw_Impl::EnumDisplayModes(self, flags, filter, context, callback)
    }
    fn EnumSurfaces(
        &self,
        flags: u32,
        desc: *mut DDSURFACEDESC,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK,
    ) -> Result<()> {
        IDirectDraw_Impl::EnumSurfaces(self, flags, desc, context, callback)
    }
    fn GetDisplayMode(&self, desc: *mut DDSURFACEDESC) -> Result<()> {
        IDirectDraw_Impl::GetDisplayMode(self, desc)
    }
    fn GetGDISurface(&self) -> Result<IDirectDrawSurface> {
        IDirectDraw_Impl::GetGDISurface(self)
    }
    fn GetAvailableVidMem(
        &self,
        caps: *mut DDSCAPS,
        total: *mut u32,
        free: *mut u32,
    ) -> Result<()> {
        if caps.is_null() {
            return Err(E_INVALIDARG.into());
        }
        // SAFETY: Caller provides writable `u32` output slots for non-null `total`/`free`.
        unsafe { self.get_available_vid_mem_impl(total, free) }
    }
}

#[allow(non_snake_case)]
impl IDirectDraw4_Impl for Draw_Impl {
    delegate_draw_common!();

    fn CreateSurface(
        &self,
        desc: *mut DDSURFACEDESC2,
        out: OutRef<IDirectDrawSurface4>,
        outer: Ref<IUnknown>,
    ) -> Result<()> {
        // SAFETY: Caller supplies a sized `DDSURFACEDESC2` and any initial pixel pointer.
        unsafe { self.create_surface_v2(desc, out, outer) }
    }
    fn DuplicateSurface(&self, src: Ref<IDirectDrawSurface4>) -> Result<IDirectDrawSurface4> {
        self.duplicate_surface_impl(src)
    }
    fn EnumDisplayModes(
        &self,
        flags: u32,
        filter: *mut DDSURFACEDESC2,
        context: *mut c_void,
        callback: LPDDENUMMODESCALLBACK2,
    ) -> Result<()> {
        let callback = callback.ok_or(E_INVALIDARG)?;
        let filter = if filter.is_null() {
            DDSURFACEDESC::default()
        } else {
            // SAFETY: Non-null `filter` satisfies the `DDSURFACEDESC2` COM input contract.
            unsafe { descriptor::read(filter)? }
        };
        self.enum_display_modes_impl(flags, filter, |mode| {
            let mut mode2 = descriptor::desc_to_desc2(mode);
            // SAFETY: Invoking caller-supplied mode callback with our initialized `mode2`.
            unsafe { callback(&mut mode2, context).0 }
        })
    }
    fn EnumSurfaces(
        &self,
        flags: u32,
        desc: *mut DDSURFACEDESC2,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK2,
    ) -> Result<()> {
        let callback = callback.ok_or(E_INVALIDARG)?;
        let filter = if desc.is_null() {
            None
        } else {
            // SAFETY: Non-null `desc` satisfies the `DDSURFACEDESC2` COM input contract.
            Some(unsafe { descriptor::read(desc)? })
        };
        self.enum_surfaces_impl::<IDirectDrawSurface4>(flags, filter, |surf, d| {
            let mut d2 = descriptor::desc_to_desc2(d);
            let owned = ManuallyDrop::new(surf);
            // SAFETY: DirectDraw transfers ownership of one surface ref to the enum callback.
            unsafe { callback((&*owned).into(), &mut d2, context).0 }
        })
    }
    fn GetDisplayMode(&self, desc: *mut DDSURFACEDESC2) -> Result<()> {
        // SAFETY: Caller supplies a sized `DDSURFACEDESC2` output buffer.
        let out = unsafe { descriptor::output(desc)? };
        let (width, height, bpp) = self.display.mode();
        let mode = mode_description(width, height, bpp)?;
        out.write(descriptor::desc_to_desc2(&mode));
        Ok(())
    }
    fn GetGDISurface(&self) -> Result<IDirectDrawSurface4> {
        IDirectDraw_Impl::GetGDISurface(self)?.cast()
    }
    fn GetAvailableVidMem(
        &self,
        caps: *mut DDSCAPS2,
        total: *mut u32,
        free: *mut u32,
    ) -> Result<()> {
        if caps.is_null() {
            return Err(E_INVALIDARG.into());
        }
        // SAFETY: Caller provides writable `u32` output slots for non-null `total`/`free`.
        unsafe { self.get_available_vid_mem_impl(total, free) }
    }
    fn GetSurfaceFromDC(&self, hdc: HDC) -> Result<IDirectDrawSurface4> {
        self.get_surface_from_dc_impl(hdc)
    }
    fn RestoreAllSurfaces(&self) -> Result<()> {
        Ok(())
    }
    fn TestCooperativeLevel(&self) -> Result<()> {
        Ok(())
    }
    fn GetDeviceIdentifier(&self, out: *mut DDDEVICEIDENTIFIER, flags: u32) -> Result<()> {
        if flags & !(DDGDI_GETHOSTIDENTIFIER as u32) != 0 {
            return Err(E_INVALIDARG.into());
        }
        // SAFETY: Caller passes a writable `DDDEVICEIDENTIFIER` output pointer.
        unsafe { ffi_ptr::write_out(out, fill_device_identifier(), E_INVALIDARG) }
    }
}

#[allow(non_snake_case)]
impl IDirectDraw7_Impl for Draw_Impl {
    delegate_draw_common!();

    fn CreateSurface(
        &self,
        desc: *mut DDSURFACEDESC2,
        out: OutRef<IDirectDrawSurface7>,
        outer: Ref<IUnknown>,
    ) -> Result<()> {
        // SAFETY: Caller supplies a sized `DDSURFACEDESC2` and any initial pixel pointer.
        unsafe { self.create_surface_v2(desc, out, outer) }
    }
    fn DuplicateSurface(&self, src: Ref<IDirectDrawSurface7>) -> Result<IDirectDrawSurface7> {
        self.duplicate_surface_impl(src)
    }
    fn EnumDisplayModes(
        &self,
        flags: u32,
        filter: *mut DDSURFACEDESC2,
        context: *mut c_void,
        callback: LPDDENUMMODESCALLBACK2,
    ) -> Result<()> {
        IDirectDraw4_Impl::EnumDisplayModes(self, flags, filter, context, callback)
    }
    fn EnumSurfaces(
        &self,
        flags: u32,
        desc: *mut DDSURFACEDESC2,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK7,
    ) -> Result<()> {
        let callback = callback.ok_or(E_INVALIDARG)?;
        let filter = if desc.is_null() {
            None
        } else {
            // SAFETY: Non-null `desc` satisfies the `DDSURFACEDESC2` COM input contract.
            Some(unsafe { descriptor::read(desc)? })
        };
        self.enum_surfaces_impl::<IDirectDrawSurface7>(flags, filter, |surf, d| {
            let mut d2 = descriptor::desc_to_desc2(d);
            let owned = ManuallyDrop::new(surf);
            // SAFETY: DirectDraw transfers ownership of one surface ref to the enum callback.
            unsafe { callback((&*owned).into(), &mut d2, context).0 }
        })
    }
    fn GetDisplayMode(&self, desc: *mut DDSURFACEDESC2) -> Result<()> {
        IDirectDraw4_Impl::GetDisplayMode(self, desc)
    }
    fn GetGDISurface(&self) -> Result<IDirectDrawSurface7> {
        IDirectDraw_Impl::GetGDISurface(self)?.cast()
    }
    fn GetAvailableVidMem(
        &self,
        caps: *mut DDSCAPS2,
        total: *mut u32,
        free: *mut u32,
    ) -> Result<()> {
        IDirectDraw4_Impl::GetAvailableVidMem(self, caps, total, free)
    }
    fn GetSurfaceFromDC(&self, hdc: HDC) -> Result<IDirectDrawSurface7> {
        self.get_surface_from_dc_impl(hdc)
    }
    fn RestoreAllSurfaces(&self) -> Result<()> {
        Ok(())
    }
    fn TestCooperativeLevel(&self) -> Result<()> {
        Ok(())
    }
    fn GetDeviceIdentifier(&self, out: *mut DDDEVICEIDENTIFIER2, flags: u32) -> Result<()> {
        if flags & !(DDGDI_GETHOSTIDENTIFIER as u32) != 0 {
            return Err(E_INVALIDARG.into());
        }
        // SAFETY: Caller passes a writable `DDDEVICEIDENTIFIER2` output pointer.
        unsafe { ffi_ptr::write_out(out, fill_device_identifier2(), E_INVALIDARG) }
    }
    fn StartModeTest(&self, _: *mut SIZE, _: u32, _: u32) -> Result<()> {
        Err(DDERR_CURRENTLYNOTAVAIL.into())
    }
    fn EvaluateMode(&self, _: u32, _: *mut u32) -> Result<()> {
        Err(DDERR_INVALIDOBJECT.into())
    }
}

fn valid_device_guid(device: *const GUID) -> bool {
    device.is_null()
        || device as usize == DDCREATE_HARDWAREONLY as usize
        || device as usize == DDCREATE_EMULATIONONLY as usize
}

#[unsafe(no_mangle)]
pub extern "system" fn DirectDrawCreate(
    device: *const GUID,
    out: OutRef<IDirectDraw>,
    outer: Ref<IUnknown>,
) -> HRESULT {
    ffi_ptr::put_interface(out, || {
        ffi_ptr::no_aggregation(outer)?;
        if !valid_device_guid(device) {
            return Err(E_INVALIDARG.into());
        }
        Draw::create_interface(true)
    })
    .into()
}

#[unsafe(no_mangle)]
pub extern "system" fn DirectDrawCreateEx(
    device: *const GUID,
    out: OutRef<IDirectDraw7>,
    iid: Option<&GUID>,
    outer: Ref<IUnknown>,
) -> HRESULT {
    ffi_ptr::put_interface(out, || {
        let iid = iid.ok_or(E_POINTER)?;
        ffi_ptr::no_aggregation(outer)?;
        if !valid_device_guid(device) {
            return Err(E_INVALIDARG.into());
        }
        if *iid != IDirectDraw7::IID {
            return Err(E_NOINTERFACE.into());
        }
        Draw::create_interface(true)?.cast()
    })
    .into()
}

#[unsafe(no_mangle)]
pub extern "system" fn DirectDrawCreateClipper(
    flags: u32,
    out: OutRef<IDirectDrawClipper>,
    outer: Ref<IUnknown>,
) -> HRESULT {
    ffi_ptr::put_interface(out, || {
        ffi_ptr::no_aggregation(outer)?;
        clipper::create_clipper(flags, None)
    })
    .into()
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DirectDrawEnumerateA(
    callback: LPDDENUMCALLBACKA,
    context: *mut c_void,
) -> HRESULT {
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    // SAFETY: Invoking caller-supplied enumeration callback with static strings and `context`.
    let _ = unsafe {
        callback(
            ptr::null_mut(),
            s!("cnc-ddraw wgpu"),
            s!("display"),
            context,
        )
    };
    S_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DirectDrawEnumerateW(
    callback: LPDDENUMCALLBACKW,
    context: *mut c_void,
) -> HRESULT {
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    // SAFETY: Invoking caller-supplied enumeration callback with static wide strings and `context`.
    let _ = unsafe {
        callback(
            ptr::null_mut(),
            w!("cnc-ddraw wgpu"),
            w!("display"),
            context,
        )
    };
    S_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DirectDrawEnumerateExA(
    callback: LPDDENUMCALLBACKEXA,
    context: *mut c_void,
    flags: u32,
) -> HRESULT {
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    if flags
        & !((DDENUM_ATTACHEDSECONDARYDEVICES
            | DDENUM_DETACHEDSECONDARYDEVICES
            | DDENUM_NONDISPLAYDEVICES) as u32)
        != 0
    {
        return E_INVALIDARG;
    }
    // SAFETY: Invoking caller-supplied extended enumeration callback with static strings.
    let _ = unsafe {
        callback(
            ptr::null_mut(),
            s!("cnc-ddraw wgpu"),
            s!("display"),
            context,
            HMONITOR::default(),
        )
    };
    S_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DirectDrawEnumerateExW(
    callback: LPDDENUMCALLBACKEXW,
    context: *mut c_void,
    flags: u32,
) -> HRESULT {
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    if flags
        & !((DDENUM_ATTACHEDSECONDARYDEVICES
            | DDENUM_DETACHEDSECONDARYDEVICES
            | DDENUM_NONDISPLAYDEVICES) as u32)
        != 0
    {
        return E_INVALIDARG;
    }
    // SAFETY: Invoking caller-supplied extended enumeration callback with static wide strings.
    let _ = unsafe {
        callback(
            ptr::null_mut(),
            w!("cnc-ddraw wgpu"),
            w!("display"),
            context,
            HMONITOR::default(),
        )
    };
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn retained_pixel_storage_does_not_keep_a_released_com_surface_alive() {
        let display = DisplayShared::new(640, 480, 32);
        let desc = DDSURFACEDESC {
            dwSize: size_of::<DDSURFACEDESC>() as u32,
            dwFlags: DDSD_CAPS as u32,
            ddsCaps: DDSCAPS {
                dwCaps: DDSCAPS_PRIMARYSURFACE as u32,
            },
            ..Default::default()
        };
        // SAFETY: `desc` has no `DDSD_LPSURFACE` pointer.
        let primary = unsafe { surface::create_surface(display.clone(), &desc) }.unwrap();
        let pixels = display.primary_slot().unwrap();
        assert!(display.primary_surface().is_some());
        // Final Release must unregister even while a presenter holds the display mutex.
        std::thread::scope(|scope| {
            let (locked, ready) = std::sync::mpsc::sync_channel(0);
            let (released, wait) = std::sync::mpsc::sync_channel(0);
            let display = &display;
            scope.spawn(move || {
                let _guard = display.lock();
                locked.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(3)).unwrap();
            });
            ready.recv().unwrap();
            drop(primary);
            released.send(()).unwrap();
        });
        assert!(display.primary_slot().is_none());
        assert!(display.registry().entries.is_empty());
        assert_eq!(display.allocated_bytes(), 0);
        assert!(display.primary_surface().is_none());
        assert!(display.live_surfaces().is_empty());
        drop(pixels);
    }

    #[test]
    fn draw_release_returns_zero_with_live_surfaces_and_mode_switch_succeeds() {
        let mut dd = None;
        assert_eq!(
            DirectDrawCreate(ptr::null(), (&mut dd).into(), Ref::default()),
            S_OK
        );
        let dd = dd.unwrap();
        // SAFETY: Calling standard COM methods on our newly created DirectDraw object.
        unsafe { dd.SetDisplayMode(1024, 768, 16) }.unwrap();

        let mut prim_desc = DDSURFACEDESC {
            dwSize: size_of::<DDSURFACEDESC>() as u32,
            dwFlags: DDSD_CAPS as u32,
            ddsCaps: DDSCAPS {
                dwCaps: DDSCAPS_PRIMARYSURFACE as u32,
            },
            ..Default::default()
        };
        let mut primary: Option<IDirectDrawSurface> = None;
        // SAFETY: Passing valid initialized `prim_desc` and `primary` out slot.
        unsafe { dd.CreateSurface(&mut prim_desc, &mut primary, None) }.unwrap();
        let primary = primary.unwrap();

        let mut off_desc = DDSURFACEDESC {
            dwSize: size_of::<DDSURFACEDESC>() as u32,
            dwFlags: (DDSD_CAPS | DDSD_WIDTH | DDSD_HEIGHT) as u32,
            dwWidth: 64,
            dwHeight: 64,
            ddsCaps: DDSCAPS {
                dwCaps: DDSCAPS_OFFSCREENPLAIN as u32,
            },
            ..Default::default()
        };
        let mut off: Option<IDirectDrawSurface> = None;
        // SAFETY: Passing valid initialized `off_desc` and `off` out slot.
        unsafe { dd.CreateSurface(&mut off_desc, &mut off, None) }.unwrap();
        let off = off.unwrap();

        // GetDDInterface works while IDirectDraw is alive and balances its own refcount.
        let off2: IDirectDrawSurface2 = off.cast().unwrap();
        let mut dd_ptr: *mut c_void = ptr::null_mut();
        // SAFETY: `dd_ptr` is a valid writable output slot.
        unsafe { off2.GetDDInterface(&mut dd_ptr) }.unwrap();
        assert!(!dd_ptr.is_null());
        // SAFETY: `GetDDInterface` returned an owning `IDirectDraw` interface pointer.
        let dd_from_surface = unsafe { IDirectDraw::from_raw(dd_ptr) };
        drop(dd_from_surface);

        // Mode switch and recreating primary while old primary still exists must succeed.
        // SAFETY: Valid display mode arguments.
        unsafe { dd.SetDisplayMode(800, 600, 16) }.unwrap();
        let mut primary2: Option<IDirectDrawSurface> = None;
        // SAFETY: Passing valid initialized `prim_desc` and `primary2` out slot.
        unsafe { dd.CreateSurface(&mut prim_desc, &mut primary2, None) }.unwrap();
        let primary2 = primary2.unwrap();

        // Releasing IDirectDraw while surfaces are still alive must return refcount == 0
        // (RA2 Reset_Video_Mode checks `DirectDrawObject->Release() == 0`).
        let release = dd.vtable().base__.Release;
        // SAFETY: Consuming the single remaining strong `IDirectDraw` reference via COM Release.
        let refcount = unsafe { release(dd.into_raw()) };
        assert_eq!(refcount, 0);

        // After IDirectDraw is released, GetDDInterface safely fails instead of dangling.
        let mut stale_ptr: *mut c_void = ptr::null_mut();
        // SAFETY: `stale_ptr` is a valid writable output slot.
        assert!(unsafe { off2.GetDDInterface(&mut stale_ptr) }.is_err());
        assert!(stale_ptr.is_null());

        drop(primary2);
        drop(primary);
        drop(off2);
        drop(off);
    }
}
