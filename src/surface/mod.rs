//! COM DirectDraw 1-7 surface wrapper. All surface data, synchronization, and pixel blit logic
//! live in `model` and `blit` (`#![forbid(unsafe_code)]`), and GDI DIB ownership lives in `dc`.
#![allow(non_snake_case)]

mod blit;
mod dc;
mod model;

pub use model::{pixel_format, PixelFormat, SurfaceData, SurfaceSlot};

use std::ffi::c_void;
use std::mem::{size_of, ManuallyDrop};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use windows::core::{implement, Error, IUnknown, Interface, OutRef, Ref, Result, GUID};
use windows::Win32::Foundation::{E_INVALIDARG, E_POINTER, HANDLE, RECT};
use windows::Win32::Graphics::DirectDraw::*;
use windows::Win32::Graphics::Gdi::{BLACKNESS, HDC, PATCOPY, SRCCOPY, WHITENESS};

use crate::clipper;
use crate::draw::DisplayShared;
use crate::ffi::{descriptor, ptr as ffi_ptr};
use crate::simd_blit::{self, KeyRange};

use blit::blit;
use dc::SendHdc;
use model::{
    identity_gamma_ramp, lock_chain_ready, lock_pair_ready, unsupported, zeros, Access, BltOptions,
    Format, PixelStorage, PrivateData, DDERR_ALREADYINITIALIZED, DDERR_CANTDUPLICATE,
    DDERR_DCALREADYCREATED, DDERR_INVALIDCAPS, DDERR_INVALIDPIXELFORMAT, DDERR_INVALIDRECT,
    DDERR_MOREDATA, DDERR_NOCLIPPERATTACHED, DDERR_NOCOLORKEY, DDERR_NODC, DDERR_NOPALETTEATTACHED,
    DDERR_NOTAOVERLAYSURFACE, DDERR_NOTFLIPPABLE, DDERR_NOTFOUND, DDERR_NOTLOCKED,
    DDERR_SURFACEALREADYATTACHED, DDERR_SURFACENOTATTACHED, MAX_BACKBUFFERS,
};

/// # Safety
/// If `DDSD_LPSURFACE` is set and non-null, the caller must provide `row_bytes` initialized bytes
/// at each row start. Pitch padding is never accessed.
unsafe fn import_pixels(
    storage: &mut PixelStorage,
    desc: &DDSURFACEDESC,
    row_bytes: usize,
) -> Result<()> {
    if desc.dwFlags & DDSD_LPSURFACE as u32 == 0 || desc.lpSurface.is_null() {
        return Ok(());
    }
    let src_pitch = match descriptor::supplied_pitch(desc) {
        Some(p) if p > 0 && p as usize >= row_bytes => p as usize,
        Some(_) => return Err(E_INVALIDARG.into()),
        None => storage.pitch,
    };
    let height = storage.image_len / storage.pitch;
    (height - 1)
        .checked_mul(src_pitch)
        .and_then(|n| n.checked_add(row_bytes))
        .filter(|&len| len <= isize::MAX as usize)
        .ok_or(E_INVALIDARG)?;
    let dst_pitch = storage.pitch;
    for (y, row) in storage.image_mut().chunks_exact_mut(dst_pitch).enumerate() {
        // SAFETY: The checked offset addresses a caller-provided row. No slice spans
        // inter-row padding or extends past the final pixel.
        let src = unsafe {
            std::slice::from_raw_parts(desc.lpSurface.cast::<u8>().add(y * src_pitch), row_bytes)
        };
        row[..row_bytes].copy_from_slice(src);
    }
    Ok(())
}

#[implement(
    IDirectDrawSurface,
    IDirectDrawSurface2,
    IDirectDrawSurface3,
    IDirectDrawSurface4,
    IDirectDrawSurface7,
    IDirectDrawGammaControl,
    Agile = false
)]
struct Surface {
    display: Arc<DisplayShared>,
    data: Arc<SurfaceSlot>,
    is_primary: bool,
    attached: Mutex<Vec<IDirectDrawSurface>>,
}

impl Drop for Surface {
    fn drop(&mut self) {
        self.display.unregister_surface(&self.data);
    }
}

fn register_surface_object(
    display: Arc<DisplayShared>,
    data: Arc<SurfaceSlot>,
    is_primary: bool,
    attached: Vec<IDirectDrawSurface>,
) -> Result<IDirectDrawSurface> {
    let iface: IDirectDrawSurface = Surface {
        display: display.clone(),
        data: data.clone(),
        is_primary,
        attached: Mutex::new(attached),
    }
    .into();
    display.register_surface(iface.downgrade()?, Arc::downgrade(&data), is_primary);
    Ok(iface)
}

/// # Safety
/// When `DDSD_LPSURFACE` supplies an image, its pointer must be readable for
/// every row described by the source pitch, pixel format, and dimensions.
pub unsafe fn create_surface(
    display: Arc<DisplayShared>,
    desc: &DDSURFACEDESC,
) -> Result<IDirectDrawSurface> {
    if desc.dwSize as usize != size_of::<DDSURFACEDESC>() {
        return Err(E_INVALIDARG.into());
    }
    let caps = desc.ddsCaps.dwCaps;
    let supported_caps = DDSCAPS_PRIMARYSURFACE
        | DDSCAPS_OFFSCREENPLAIN
        | DDSCAPS_SYSTEMMEMORY
        | DDSCAPS_VIDEOMEMORY
        | DDSCAPS_LOCALVIDMEM
        | DDSCAPS_NONLOCALVIDMEM
        | DDSCAPS_FLIP
        | DDSCAPS_COMPLEX
        | DDSCAPS_VISIBLE
        | DDSCAPS_FRONTBUFFER
        | DDSCAPS_BACKBUFFER
        | DDSCAPS_3DDEVICE
        | DDSCAPS_TEXTURE
        | DDSCAPS_ZBUFFER
        | DDSCAPS_OWNDC
        | DDSCAPS_WRITEONLY
        | DDSCAPS_ALLOCONLOAD
        | DDSCAPS_STANDARDVGAMODE
        | DDSCAPS_MODEX
        | DDSCAPS_HWCODEC
        | DDSCAPS_OVERLAY
        | DDSCAPS_MIPMAP
        | DDSCAPS_ALPHA
        | DDSCAPS_PALETTE;
    let supported_flags = DDSD_CAPS
        | DDSD_WIDTH
        | DDSD_HEIGHT
        | DDSD_PITCH
        | DDSD_PIXELFORMAT
        | DDSD_BACKBUFFERCOUNT
        | DDSD_CKSRCBLT
        | DDSD_CKDESTBLT
        | DDSD_CKSRCOVERLAY
        | DDSD_CKDESTOVERLAY
        | DDSD_LPSURFACE
        | DDSD_ALPHABITDEPTH
        | DDSD_ZBUFFERBITDEPTH
        | DDSD_MIPMAPCOUNT
        | DDSD_REFRESHRATE
        | DDSD_TEXTURESTAGE
        | DDSD_LINEARSIZE;
    if caps & !(supported_caps as u32) != 0 || desc.dwFlags & !(supported_flags as u32) != 0 {
        return unsupported();
    }
    let primary = caps & DDSCAPS_PRIMARYSURFACE as u32 != 0;
    let d = display.lock();
    let (width, height) = if primary {
        (d.width, d.height)
    } else {
        (desc.dwWidth, desc.dwHeight)
    };
    let format = if desc.dwFlags & DDSD_PIXELFORMAT as u32 != 0 {
        Format::from_dd(&desc.ddpfPixelFormat)?
    } else {
        Format::from_depth(d.bpp)?
    };
    drop(d);
    let count = if desc.dwFlags & DDSD_BACKBUFFERCOUNT as u32 != 0 {
        desc.dwBackBufferCount
    } else {
        0
    };
    if count > MAX_BACKBUFFERS {
        return unsupported();
    }
    if desc.dwFlags & DDSD_LPSURFACE as u32 != 0 && !desc.lpSurface.is_null() {
        crate::compat::log_msg(&format!(
            "surface create: custom LPSURFACE primary={primary} {width}x{height} format={format:?} pitch={:?}",
            descriptor::supplied_pitch(desc)
        ));
    }
    if (count > 0) != (caps & DDSCAPS_FLIP as u32 != 0) {
        return Err(DDERR_INVALIDCAPS.into());
    }
    let mem_caps = if caps & DDSCAPS_SYSTEMMEMORY as u32 != 0 {
        caps
    } else if primary {
        caps | DDSCAPS_SYSTEMMEMORY as u32
    } else {
        caps | DDSCAPS_VIDEOMEMORY as u32
    };
    let mut data = SurfaceData::new(width, height, format, mem_caps)?;
    if primary {
        data.caps |= DDSCAPS_VISIBLE as u32;
        crate::compat::log_msg(&format!(
            "primary create: {width}x{height} format={format:?} backbuffers={count} lpsurface={}",
            desc.dwFlags & DDSD_LPSURFACE as u32 != 0 && !desc.lpSurface.is_null()
        ));
    }
    if desc.dwFlags & DDSD_CKSRCBLT as u32 != 0 {
        data.source_key = Some(desc.ddckCKSrcBlt);
    }
    if desc.dwFlags & DDSD_CKDESTBLT as u32 != 0 {
        data.dest_key = Some(desc.ddckCKDestBlt);
    }
    if desc.dwFlags & DDSD_CKSRCOVERLAY as u32 != 0 {
        data.src_overlay_key = Some(desc.ddckCKSrcOverlay);
    }
    if desc.dwFlags & DDSD_CKDESTOVERLAY as u32 != 0 {
        data.dest_overlay_key = Some(desc.ddckCKDestOverlay);
    }
    // SAFETY: Caller guarantees `DDSD_LPSURFACE` pixels cover every row of `width x height`.
    unsafe { import_pixels(&mut data.storage, desc, width as usize * format.bytes())? };

    let mut attached = Vec::new();
    if count > 0 {
        data.caps |= DDSCAPS_FRONTBUFFER as u32;
        let back_mem = if caps & DDSCAPS_VIDEOMEMORY as u32 != 0 {
            (DDSCAPS_BACKBUFFER | DDSCAPS_FLIP | DDSCAPS_VIDEOMEMORY) as u32
        } else {
            (DDSCAPS_BACKBUFFER | DDSCAPS_FLIP | DDSCAPS_SYSTEMMEMORY) as u32
        };
        // Build backbuffer chain from tail to head so back[0] -> back[1] -> ... -> back[count-1].
        let mut next: Option<IDirectDrawSurface> = None;
        for _ in 0..count {
            let back_data = Arc::new(SurfaceSlot::new(SurfaceData::new(
                width, height, format, back_mem,
            )?));
            let chain = next.into_iter().collect();
            next = Some(register_surface_object(
                display.clone(),
                back_data,
                false,
                chain,
            )?);
        }
        if let Some(first_back) = next {
            attached.push(first_back);
        }
    }

    let data = Arc::new(SurfaceSlot::new(data));
    let surface = register_surface_object(display, data, primary, attached)?;
    if primary {
        crate::window::set_mode(width, height, (format.bytes() * 8) as u32, true);
    }
    Ok(surface)
}

pub fn duplicate_surface(
    display: Arc<DisplayShared>,
    source: &IDirectDrawSurface,
) -> Result<IDirectDrawSurface> {
    let src_obj = source.cast_object_ref::<Surface>()?;
    let has_attached = !src_obj.attached_lock().is_empty();
    let src = src_obj.data.lock_ready(true)?;
    if src.caps & (DDSCAPS_PRIMARYSURFACE | DDSCAPS_FLIP | DDSCAPS_COMPLEX | DDSCAPS_ZBUFFER) as u32
        != 0
        || has_attached
    {
        return Err(DDERR_CANTDUPLICATE.into());
    }
    let mut copy = SurfaceData::with_pitch(
        src.width,
        src.height,
        src.format,
        src.caps,
        Some(src.storage.pitch as i32),
    )?;
    copy.pixel_slice_mut().copy_from_slice(src.pixel_slice());
    copy.palette = src.palette.clone();
    copy.clipper = src.clipper.clone();
    copy.source_key = src.source_key;
    copy.dest_key = src.dest_key;
    copy.src_overlay_key = src.src_overlay_key;
    copy.dest_overlay_key = src.dest_overlay_key;
    copy.gamma = src.gamma;
    drop(src);
    register_surface_object(display, Arc::new(SurfaceSlot::new(copy)), false, Vec::new())
}

pub fn surface_desc(surface: &IDirectDrawSurface) -> Result<DDSURFACEDESC> {
    let obj = surface.cast_object_ref::<Surface>()?;
    let count = obj.backbuffer_chain_len();
    Ok(obj.read()?.desc(count))
}

pub fn matches_desc(surface: &IDirectDrawSurface, filter: &DDSURFACEDESC) -> Result<bool> {
    let desc = surface_desc(surface)?;
    if filter.dwFlags & DDSD_CAPS as u32 != 0
        && desc.ddsCaps.dwCaps & filter.ddsCaps.dwCaps != filter.ddsCaps.dwCaps
    {
        return Ok(false);
    }
    if filter.dwFlags & DDSD_WIDTH as u32 != 0 && desc.dwWidth != filter.dwWidth {
        return Ok(false);
    }
    if filter.dwFlags & DDSD_HEIGHT as u32 != 0 && desc.dwHeight != filter.dwHeight {
        return Ok(false);
    }
    if filter.dwFlags & DDSD_PIXELFORMAT as u32 != 0 {
        let a = &filter.ddpfPixelFormat;
        let b = &desc.ddpfPixelFormat;
        if a.dwFlags != b.dwFlags || descriptor::rgb_layout(a) != descriptor::rgb_layout(b) {
            return Ok(false);
        }
    }
    Ok(true)
}

static PRIMARY_TRACE: AtomicUsize = AtomicUsize::new(0);

fn trace_primary(operation: &str) {
    if PRIMARY_TRACE.fetch_add(1, Ordering::Relaxed) < 30 {
        crate::compat::log_msg(&format!("primary {operation}"));
    }
}

impl Surface {
    fn read(&self) -> Result<MutexGuard<'_, SurfaceData>> {
        Ok(self.data.lock())
    }

    fn write(&self) -> Result<MutexGuard<'_, SurfaceData>> {
        Ok(self.data.lock())
    }

    fn attached_lock(&self) -> MutexGuard<'_, Vec<IDirectDrawSurface>> {
        self.attached.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn refresh(&self) -> Result<()> {
        if self.is_primary {
            trace_primary("refresh");
            self.display.request_refresh()?;
        }
        Ok(())
    }

    fn present_now(&self) -> Result<()> {
        if self.is_primary {
            trace_primary("present_now");
            self.display.request_present_now()?;
        }
        Ok(())
    }

    /// Walks the attached flip chain (excluding self) without cycling.
    fn flip_chain(&self) -> Result<Vec<IDirectDrawSurface>> {
        let mut chain = Vec::new();
        let mut current = self
            .attached_lock()
            .iter()
            .find(|s| {
                s.cast_object_ref::<Surface>().is_ok_and(|o| {
                    o.data.lock().caps & (DDSCAPS_BACKBUFFER | DDSCAPS_FLIP) as u32 != 0
                })
            })
            .cloned();
        while let Some(surf) = current {
            let obj = surf.cast_object_ref::<Surface>()?;
            if Arc::ptr_eq(&obj.data, &self.data)
                || chain.iter().any(|existing: &IDirectDrawSurface| {
                    existing
                        .cast_object_ref::<Surface>()
                        .is_ok_and(|e| Arc::ptr_eq(&e.data, &obj.data))
                })
            {
                break;
            }
            let next = obj
                .attached_lock()
                .iter()
                .find(|s| {
                    s.cast_object_ref::<Surface>().is_ok_and(|o| {
                        o.data.lock().caps & (DDSCAPS_BACKBUFFER | DDSCAPS_FLIP) as u32 != 0
                    })
                })
                .cloned();
            chain.push(surf);
            current = next;
        }
        Ok(chain)
    }

    fn backbuffer_chain_len(&self) -> u32 {
        self.flip_chain().map(|c| c.len() as u32).unwrap_or(0)
    }

    fn add_attached_surface_impl<S: Interface>(&self, target: Ref<S>) -> Result<()> {
        let target_ref = target.ok()?;
        let target_obj = target_ref.cast_object_ref::<Surface>()?;
        if Arc::ptr_eq(&target_obj.data, &self.data) {
            return Err(E_INVALIDARG.into());
        }
        let target_v1 = target_ref.cast::<IDirectDrawSurface>()?;
        let mut attached = self.attached_lock();
        if attached.iter().any(|s| {
            s.cast_object_ref::<Surface>()
                .is_ok_and(|o| Arc::ptr_eq(&o.data, &target_obj.data))
        }) {
            return Err(DDERR_SURFACEALREADYATTACHED.into());
        }
        let is_front = self.data.lock().caps & DDSCAPS_FRONTBUFFER as u32 != 0;
        if is_front {
            let mut target_data = target_obj.data.lock();
            target_data.caps |= (DDSCAPS_BACKBUFFER | DDSCAPS_FLIP) as u32;
        }
        attached.push(target_v1);
        Ok(())
    }

    fn delete_attached_surface_impl<S: Interface>(&self, flags: u32, target: Ref<S>) -> Result<()> {
        if flags != 0 {
            return Err(E_INVALIDARG.into());
        }
        let mut attached = self.attached_lock();
        match target.as_ref() {
            None => {
                attached.clear();
                Ok(())
            }
            Some(target_ref) => {
                let target_obj = target_ref.cast_object_ref::<Surface>()?;
                let before = attached.len();
                attached.retain(|s| {
                    s.cast_object_ref::<Surface>()
                        .is_ok_and(|o| !Arc::ptr_eq(&o.data, &target_obj.data))
                });
                if attached.len() == before {
                    Err(DDERR_SURFACENOTATTACHED.into())
                } else {
                    Ok(())
                }
            }
        }
    }

    fn copy(
        &self,
        source: &Surface,
        src_rect: Option<RECT>,
        dest_rect: RECT,
        opts: BltOptions,
        wait: bool,
    ) -> Result<()> {
        // Snapshot overlapping self-blits before mutably writing the destination.
        if Arc::ptr_eq(&source.data, &self.data) {
            let mut dst = self.data.lock_ready(wait)?;
            dst.bounds(Some(dest_rect))?;
            let Some(src_rect) = dst.clamp_rect(src_rect) else {
                return Ok(());
            };
            let src_format = dst.format;
            let src_palette = if src_format == Format::Indexed {
                Some(dst.palette_entries()?)
            } else {
                None
            };
            let row_bytes = (src_rect.right - src_rect.left) as usize * src_format.bytes();
            let mut snapshot = zeros(row_bytes * (src_rect.bottom - src_rect.top) as usize)?;
            let src_pixels = dst.pixel_slice();
            for (y, row) in snapshot.chunks_exact_mut(row_bytes).enumerate() {
                let start = (src_rect.top as usize + y) * dst.storage.pitch
                    + src_rect.left as usize * src_format.bytes();
                row.copy_from_slice(&src_pixels[start..start + row_bytes]);
            }
            let local = RECT {
                left: 0,
                top: 0,
                right: src_rect.right - src_rect.left,
                bottom: src_rect.bottom - src_rect.top,
            };
            blit(
                &mut dst,
                dest_rect,
                &snapshot,
                row_bytes,
                src_format,
                src_palette.as_ref(),
                local,
                opts,
            );
            dst.bump_uniqueness();
        } else {
            let (src, mut dst) = lock_pair_ready(&source.data, &self.data, wait)?;
            dst.bounds(Some(dest_rect))?;
            let Some(src_rect) = src.clamp_rect(src_rect) else {
                return Ok(());
            };
            let src_format = src.format;
            let src_palette = if src_format == Format::Indexed {
                Some(src.palette_entries()?)
            } else {
                None
            };
            blit(
                &mut dst,
                dest_rect,
                src.pixel_slice(),
                src.storage.pitch,
                src_format,
                src_palette.as_ref(),
                src_rect,
                opts,
            );
            dst.bump_uniqueness();
        }
        Ok(())
    }

    fn blt_colorfill_rect(
        &self,
        rect: RECT,
        color: [u8; 4],
        dst_key: Option<DDCOLORKEY>,
        wait: bool,
    ) -> Result<()> {
        let mut dst = self.data.lock_ready(wait)?;
        dst.bounds(Some(rect))?;
        let bytes = dst.format.bytes();
        let pitch = dst.storage.pitch;
        let width = (rect.right - rect.left) as usize;
        let height = (rect.bottom - rect.top) as usize;
        let dk = dst_key.map(|k| KeyRange {
            lo: k.dwColorSpaceLowValue,
            hi: k.dwColorSpaceHighValue,
        });
        let pixels = dst.pixel_slice_mut();
        simd_blit::colorfill_rect(
            pixels,
            pitch,
            rect.left as usize,
            rect.top as usize,
            width,
            height,
            bytes,
            color,
            dk,
        );
        dst.bump_uniqueness();
        Ok(())
    }

    fn blt_impl<S: Interface>(
        &self,
        dest: *mut RECT,
        source: Ref<S>,
        src: *mut RECT,
        flags: u32,
        fx: *mut DDBLTFX,
    ) -> Result<()> {
        let supported = (DDBLT_WAIT
            | DDBLT_DONOTWAIT
            | DDBLT_ASYNC
            | DDBLT_COLORFILL
            | DDBLT_DEPTHFILL
            | DDBLT_KEYSRC
            | DDBLT_KEYSRCOVERRIDE
            | DDBLT_KEYDEST
            | DDBLT_KEYDESTOVERRIDE
            | DDBLT_DDFX
            | DDBLT_ROP) as u32;
        if flags & !supported != 0 {
            return unsupported();
        }
        // SAFETY: Caller passes nullable readable `RECT` and `DDBLTFX` COM inputs.
        let dest = unsafe { ffi_ptr::opt_copy(dest) };
        // SAFETY: Caller passes nullable readable `DDBLTFX` for the duration of `Blt`.
        let fx_ref = unsafe { ffi_ptr::opt_ref(fx) };
        if let Some(fx_val) = fx_ref {
            let needs_fx = flags
                & ((DDBLT_COLORFILL
                    | DDBLT_DEPTHFILL
                    | DDBLT_KEYSRCOVERRIDE
                    | DDBLT_KEYDESTOVERRIDE
                    | DDBLT_DDFX
                    | DDBLT_ROP) as u32)
                != 0;
            if needs_fx && fx_val.dwSize as usize != size_of::<DDBLTFX>() {
                return Err(E_INVALIDARG.into());
            }
        }
        let Some(rect) = self.read()?.clamp_rect(dest) else {
            return Ok(());
        };
        let clip_rects = {
            let clipper_opt = self.read()?.clipper.clone();
            match clipper_opt {
                Some(ref c) => clipper::clip_rects(c, rect)?,
                None => None,
            }
        };

        let dst_key = if flags & DDBLT_KEYDESTOVERRIDE as u32 != 0 {
            Some(fx_ref.ok_or(E_INVALIDARG)?.ddckDestColorkey)
        } else if flags & DDBLT_KEYDEST as u32 != 0 {
            Some(self.read()?.dest_key.ok_or(DDERR_NOCOLORKEY)?)
        } else {
            None
        };

        // Handle ROP fills (BLACKNESS / WHITENESS / PATCOPY).
        let mut fill_color: Option<[u8; 4]> = None;
        if flags & (DDBLT_COLORFILL | DDBLT_DEPTHFILL) as u32 != 0 {
            if !source.is_null() || flags & (DDBLT_KEYSRC | DDBLT_KEYSRCOVERRIDE) as u32 != 0 {
                return Err(E_INVALIDARG.into());
            }
            let fx_val = fx_ref.ok_or(E_INVALIDARG)?;
            let raw = if flags & DDBLT_COLORFILL as u32 != 0 {
                descriptor::bltfx_fill_color(fx_val)
            } else {
                descriptor::bltfx_fill_depth(fx_val)
            };
            fill_color = Some(raw.to_le_bytes());
        } else if flags & DDBLT_ROP as u32 != 0 {
            let fx_val = fx_ref.ok_or(E_INVALIDARG)?;
            let rop = fx_val.dwROP;
            if rop == BLACKNESS.0 {
                fill_color = Some([0, 0, 0, 0]);
            } else if rop == WHITENESS.0 {
                fill_color = Some([0xff, 0xff, 0xff, 0xff]);
            } else if rop == PATCOPY.0 {
                fill_color = Some(descriptor::bltfx_fill_color(fx_val).to_le_bytes());
            } else if rop != SRCCOPY.0 {
                return unsupported();
            }
        }

        let wait = flags & DDBLT_DONOTWAIT as u32 == 0;
        if let Some(color) = fill_color {
            match clip_rects {
                Some(rects) => {
                    for sub in rects {
                        self.blt_colorfill_rect(sub, color, dst_key, wait)?;
                    }
                }
                None => self.blt_colorfill_rect(rect, color, dst_key, wait)?,
            }
            return self.refresh();
        }

        let source_obj = source.ok()?.cast_object_ref::<Surface>()?;
        // SAFETY: Caller passes nullable readable source `RECT`.
        let src = unsafe { ffi_ptr::opt_copy(src) };
        let src_key = if flags & DDBLT_KEYSRCOVERRIDE as u32 != 0 {
            Some(fx_ref.ok_or(E_INVALIDARG)?.ddckSrcColorkey)
        } else if flags & DDBLT_KEYSRC as u32 != 0 {
            Some(source_obj.read()?.source_key.ok_or(DDERR_NOCOLORKEY)?)
        } else {
            None
        };

        let (mirror_lr, mirror_ud) = if flags & DDBLT_DDFX as u32 != 0 {
            let ddfx = fx_ref.ok_or(E_INVALIDARG)?.dwDDFX;
            let allowed_ddfx = (DDBLTFX_MIRRORLEFTRIGHT
                | DDBLTFX_MIRRORUPDOWN
                | DDBLTFX_NOTEARING
                | DDBLTFX_ARITHSTRETCHY) as u32;
            if ddfx & !allowed_ddfx != 0 {
                return unsupported();
            }
            (
                ddfx & DDBLTFX_MIRRORLEFTRIGHT as u32 != 0,
                ddfx & DDBLTFX_MIRRORUPDOWN as u32 != 0,
            )
        } else {
            (false, false)
        };

        let opts = BltOptions {
            src_key,
            dst_key,
            mirror_lr,
            mirror_ud,
        };

        match clip_rects {
            Some(rects) => {
                if !rects.is_empty() {
                    let full_src = source_obj.read()?.bounds(src)?;
                    let dw = (rect.right - rect.left) as i64;
                    let dh = (rect.bottom - rect.top) as i64;
                    let sw = (full_src.right - full_src.left) as i64;
                    let sh = (full_src.bottom - full_src.top) as i64;
                    for sub in rects {
                        let s_left =
                            full_src.left + ((sub.left - rect.left) as i64 * sw / dw) as i32;
                        let s_top = full_src.top + ((sub.top - rect.top) as i64 * sh / dh) as i32;
                        let s_right = (full_src.left
                            + (((sub.right - rect.left) as i64 * sw + dw - 1) / dw) as i32)
                            .max(s_left + 1)
                            .min(full_src.right);
                        let s_bottom = (full_src.top
                            + (((sub.bottom - rect.top) as i64 * sh + dh - 1) / dh) as i32)
                            .max(s_top + 1)
                            .min(full_src.bottom);
                        let sub_src = RECT {
                            left: s_left,
                            top: s_top,
                            right: s_right,
                            bottom: s_bottom,
                        };
                        self.copy(source_obj, Some(sub_src), sub, opts, wait)?;
                    }
                }
            }
            None => self.copy(source_obj, src, rect, opts, wait)?,
        }
        self.refresh()
    }

    fn blt_fast_impl<S: Interface>(
        &self,
        x: u32,
        y: u32,
        source: Ref<S>,
        src_rect: *mut RECT,
        flags: u32,
    ) -> Result<()> {
        if flags
            & !(DDBLTFAST_WAIT
                | DDBLTFAST_DONOTWAIT
                | DDBLTFAST_SRCCOLORKEY
                | DDBLTFAST_DESTCOLORKEY)
            != 0
        {
            return unsupported();
        }
        let source_obj = source.ok()?.cast_object_ref::<Surface>()?;
        // SAFETY: Caller passes nullable readable source `RECT`.
        let s = source_obj
            .read()?
            .bounds(unsafe { ffi_ptr::opt_copy(src_rect) })?;
        let left = i32::try_from(x).map_err(|_| DDERR_INVALIDRECT)?;
        let top = i32::try_from(y).map_err(|_| DDERR_INVALIDRECT)?;
        let rect = RECT {
            left,
            top,
            right: left
                .checked_add(s.right - s.left)
                .ok_or(DDERR_INVALIDRECT)?,
            bottom: top.checked_add(s.bottom - s.top).ok_or(DDERR_INVALIDRECT)?,
        };
        self.read()?.bounds(Some(rect))?;
        let src_key = if flags & DDBLTFAST_SRCCOLORKEY != 0 {
            Some(source_obj.read()?.source_key.ok_or(DDERR_NOCOLORKEY)?)
        } else {
            None
        };
        let dst_key = if flags & DDBLTFAST_DESTCOLORKEY != 0 {
            Some(self.read()?.dest_key.ok_or(DDERR_NOCOLORKEY)?)
        } else {
            None
        };
        let wait = flags & DDBLTFAST_DONOTWAIT == 0;
        self.copy(
            source_obj,
            Some(s),
            rect,
            BltOptions {
                src_key,
                dst_key,
                mirror_lr: false,
                mirror_ud: false,
            },
            wait,
        )?;
        self.refresh()
    }

    fn blt_batch_impl(&self, batch: *mut DDBLTBATCH, count: u32, flags: u32) -> Result<()> {
        if flags != 0 {
            return Err(E_INVALIDARG.into());
        }
        if count == 0 {
            return Ok(());
        }
        if batch.is_null() {
            return Err(E_INVALIDARG.into());
        }
        // SAFETY: Caller supplies `count` contiguous `DDBLTBATCH` entries per DirectDraw BltBatch.
        let entries = unsafe { std::slice::from_raw_parts(batch, count as usize) };
        for entry in entries {
            let src_ref: Ref<IDirectDrawSurface> = (&*entry.lpDDSSrc).into();
            self.blt_impl(
                entry.lprDest,
                src_ref,
                entry.lprSrc,
                entry.dwFlags,
                entry.lpDDBltFx,
            )?;
        }
        Ok(())
    }

    fn flip_impl<S: Interface>(&self, target: Ref<S>, flags: u32) -> Result<()> {
        if flags & !((DDFLIP_WAIT | DDFLIP_DONOTWAIT | DDFLIP_NOVSYNC) as u32) != 0 {
            return unsupported();
        }
        let wait = flags & DDFLIP_DONOTWAIT as u32 == 0;
        let chain = self.flip_chain()?;
        if chain.is_empty() {
            return Err(DDERR_NOTFLIPPABLE.into());
        }
        if let Some(t) = target.as_ref() {
            let t_obj = t
                .cast_object_ref::<Surface>()
                .map_err(|_| DDERR_SURFACENOTATTACHED)?;
            if !chain.iter().any(|s| {
                s.cast_object_ref::<Surface>()
                    .is_ok_and(|o| Arc::ptr_eq(&o.data, &t_obj.data))
            }) {
                return Err(DDERR_SURFACENOTATTACHED.into());
            }
            {
                let (mut front, mut back_data) = lock_pair_ready(&self.data, &t_obj.data, wait)?;
                if !front.has_same_layout(&back_data) {
                    return Err(DDERR_NOTFLIPPABLE.into());
                }
                std::mem::swap(&mut front.storage, &mut back_data.storage);
                std::mem::swap(&mut front.cached_dc, &mut back_data.cached_dc);
                front.bump_uniqueness();
                back_data.bump_uniqueness();
            }
            return self.present_now();
        }

        // Lock the entire flip chain atomically in canonical pointer order before rotating buffers.
        let mut slot_refs: Vec<&SurfaceSlot> = Vec::with_capacity(chain.len() + 1);
        slot_refs.push(&self.data);
        for surf in &chain {
            slot_refs.push(&surf.cast_object_ref::<Surface>()?.data);
        }
        {
            let mut guards = lock_chain_ready(&slot_refs, wait)?;
            if guards[1..]
                .iter()
                .any(|back| !guards[0].has_same_layout(back))
            {
                return Err(DDERR_NOTFLIPPABLE.into());
            }
            let (front, backs) = guards.split_first_mut().expect("flip chain has a front");
            for back in backs.iter_mut().rev() {
                std::mem::swap(&mut front.storage, &mut back.storage);
                std::mem::swap(&mut front.cached_dc, &mut back.cached_dc);
                back.bump_uniqueness();
            }
            front.bump_uniqueness();
        }
        self.present_now()
    }

    fn get_attached_surface_impl<S>(&self, caps_val: u32, out: OutRef<S>) -> Result<()>
    where
        S: Interface + windows_core::Type<S, Default = Option<S>>,
    {
        let attachment = (|| -> Result<S> {
            if caps_val == 0 {
                return Err(DDERR_NOTFOUND.into());
            }
            let attached = self.attached_lock();
            for s in attached.iter() {
                let actual = s.cast_object_ref::<Surface>()?.read()?.caps;
                if actual & caps_val == caps_val {
                    return s.cast::<S>();
                }
            }
            Err(DDERR_NOTFOUND.into())
        })();
        match attachment {
            Ok(back) => out.write(Some(back)),
            Err(error) => {
                out.write(None)?;
                Err(error)
            }
        }
    }

    fn lock_desc(&self, rect: *mut RECT, flags: u32) -> Result<DDSURFACEDESC> {
        if flags
            & !((DDLOCK_WAIT
                | DDLOCK_DONOTWAIT
                | DDLOCK_EVENT
                | DDLOCK_READONLY
                | DDLOCK_WRITEONLY
                | DDLOCK_NOSYSLOCK
                | DDLOCK_DISCARDCONTENTS
                | DDLOCK_NOOVERWRITE) as u32)
            != 0
        {
            return unsupported();
        }
        let wait = flags & DDLOCK_DONOTWAIT as u32 == 0;
        let count = self.backbuffer_chain_len();
        let mut data = self.data.lock_ready(wait)?;
        // SAFETY: Caller passes nullable readable lock `RECT`.
        let r = data.bounds(unsafe { ffi_ptr::opt_copy(rect) })?;
        let mut desc = data.desc(count);
        let offset = r.top as usize * data.storage.pitch + r.left as usize * data.format.bytes();
        desc.lpSurface = data.surface_ptr().wrapping_add(offset).cast();
        desc.dwFlags |= DDSD_LPSURFACE as u32;
        data.access = Access::Locked {
            rect: r,
            owner_tid: thread::current().id(),
        };
        Ok(desc)
    }

    fn unlock_ptr_impl(&self, address: *mut c_void) -> Result<()> {
        let mut data = self.write()?;
        let Access::Locked { .. } = data.access else {
            return Err(DDERR_NOTLOCKED.into());
        };
        if !address.is_null() {
            let base = data.surface_ptr() as usize;
            let end = base.saturating_add(data.storage.image_len);
            let ptr = address as usize;
            if ptr < base || ptr >= end {
                return Err(E_INVALIDARG.into());
            }
        }
        data.access = Access::Ready;
        data.bump_uniqueness();
        drop(data);
        self.data.notify_ready();
        self.refresh()
    }

    fn unlock_rect_impl(&self, rect: *mut RECT) -> Result<()> {
        let mut data = self.write()?;
        let Access::Locked { rect: locked, .. } = data.access else {
            return Err(DDERR_NOTLOCKED.into());
        };
        // SAFETY: Caller passes nullable readable `RECT` per Surface4/7 `Unlock`.
        if let Some(r) = unsafe { ffi_ptr::opt_ref(rect) } {
            if *r != locked {
                return Err(DDERR_INVALIDRECT.into());
            }
        }
        data.access = Access::Ready;
        data.bump_uniqueness();
        drop(data);
        self.data.notify_ready();
        self.refresh()
    }

    fn get_clipper_impl(&self) -> Result<IDirectDrawClipper> {
        self.read()?
            .clipper
            .as_ref()
            .map(|c| c.0.clone())
            .ok_or_else(|| Error::from(DDERR_NOCLIPPERATTACHED))
    }

    fn set_clipper_impl(&self, clipper: Ref<IDirectDrawClipper>) -> Result<()> {
        self.write()?.clipper = clipper.cloned().map(ffi_ptr::SendCom);
        Ok(())
    }

    fn get_dd_interface_impl(&self, out: *mut *mut c_void) -> Result<()> {
        // SAFETY: Caller supplies an output slot which may be uninitialized.
        let out = unsafe { ffi_ptr::out_slot(out, E_POINTER)? };
        out.write(std::ptr::null_mut());
        let parent = self.display.clone_owner_draw().ok_or(E_INVALIDARG)?;
        out.write(parent.into_raw());
        Ok(())
    }

    /// # Safety
    /// A supplied `DDSD_LPSURFACE` image must contain each requested row of pixels.
    unsafe fn set_surface_desc_impl(&self, desc: &DDSURFACEDESC, flags: u32) -> Result<()> {
        if flags != 0 {
            return Err(E_INVALIDARG.into());
        }
        let mut data = self.data.lock_ready(true)?;
        let format = if desc.dwFlags & DDSD_PIXELFORMAT as u32 != 0 {
            Format::from_dd(&desc.ddpfPixelFormat)?
        } else {
            data.format
        };
        let width = if desc.dwFlags & DDSD_WIDTH as u32 != 0 {
            desc.dwWidth
        } else {
            data.width
        };
        let height = if desc.dwFlags & DDSD_HEIGHT as u32 != 0 {
            desc.dwHeight
        } else {
            data.height
        };
        let mut storage =
            PixelStorage::new(width, height, format, descriptor::supplied_pitch(desc))?;
        // The input can point to the old surface. Keep it alive until all rows have been copied.
        // SAFETY: Caller guarantees `DDSD_LPSURFACE` contains each requested row of pixels.
        unsafe { import_pixels(&mut storage, desc, width as usize * format.bytes())? };
        data.width = width;
        data.height = height;
        data.format = format;
        data.storage = storage;
        data.cached_dc = None;
        data.bump_uniqueness();
        Ok(())
    }

    fn set_private_data_impl(
        &self,
        tag: *const GUID,
        data_ptr: *mut c_void,
        size: u32,
        flags: u32,
    ) -> Result<()> {
        // SAFETY: Caller passes a valid readable `GUID` tag pointer.
        let tag = unsafe { ffi_ptr::req_ref(tag, E_INVALIDARG)? };
        let valid_flags = (DDSPD_IUNKNOWNPOINTER | DDSPD_VOLATILE) as u32;
        if flags & !valid_flags != 0 || data_ptr.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let entry = if flags & DDSPD_IUNKNOWNPOINTER as u32 != 0 {
            if size as usize != size_of::<*mut c_void>() {
                return Err(E_INVALIDARG.into());
            }
            // SAFETY: With `DDSPD_IUNKNOWNPOINTER`, `data_ptr` is a valid borrowed `IUnknown*`.
            let unk = unsafe { IUnknown::from_raw_borrowed(&data_ptr) }
                .ok_or(E_INVALIDARG)?
                .clone();
            PrivateData::Unknown(ffi_ptr::SendCom(unk))
        } else {
            // SAFETY: Caller supplies `size` readable bytes at `data_ptr`.
            let slice = unsafe { std::slice::from_raw_parts(data_ptr.cast::<u8>(), size as usize) };
            PrivateData::Bytes(slice.to_vec())
        };
        self.write()?.private_data.insert(*tag, entry);
        Ok(())
    }

    fn get_private_data_impl(
        &self,
        tag: *const GUID,
        buffer: *mut c_void,
        size: *mut u32,
    ) -> Result<()> {
        // SAFETY: Caller passes a readable `GUID` tag and a writable in/out `size` DWORD.
        let tag = unsafe { ffi_ptr::req_ref(tag, E_INVALIDARG)? };
        // SAFETY: `size` is an initialized in/out `u32` pointer per COM GetPrivateData.
        let size = unsafe { size.as_mut() }.ok_or(E_INVALIDARG)?;
        let data = self.read()?;
        let entry = data.private_data.get(tag).ok_or(DDERR_NOTFOUND)?;
        match entry {
            PrivateData::Bytes(bytes) => {
                let needed = bytes.len() as u32;
                if *size < needed || buffer.is_null() {
                    *size = needed;
                    return Err(DDERR_MOREDATA.into());
                }
                // SAFETY: `buffer` is non-null and holds at least `*size >= bytes.len()` writable bytes.
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.cast::<u8>(), bytes.len());
                }
                *size = needed;
                Ok(())
            }
            PrivateData::Unknown(unk) => {
                let needed = size_of::<*mut c_void>() as u32;
                if *size < needed || buffer.is_null() {
                    *size = needed;
                    return Err(DDERR_MOREDATA.into());
                }
                // SAFETY: `buffer` is non-null and writable for one COM interface pointer.
                unsafe {
                    buffer.cast::<*mut c_void>().write(unk.0.clone().into_raw());
                }
                *size = needed;
                Ok(())
            }
        }
    }

    fn free_private_data_impl(&self, tag: *const GUID) -> Result<()> {
        // SAFETY: Caller passes a valid readable `GUID` tag pointer.
        let tag = unsafe { ffi_ptr::req_ref(tag, E_INVALIDARG)? };
        self.write()?.private_data.remove(tag);
        Ok(())
    }

    fn get_uniqueness_value_impl(&self, out: *mut u32) -> Result<()> {
        let value = self.read()?.uniqueness;
        // SAFETY: Caller passes a writable `u32` output slot.
        unsafe { ffi_ptr::write_out(out, value, E_INVALIDARG) }
    }

    fn change_uniqueness_value_impl(&self) -> Result<()> {
        self.write()?.bump_uniqueness();
        Ok(())
    }

    fn set_priority_impl(&self, priority: u32) -> Result<()> {
        self.write()?.priority = priority;
        Ok(())
    }

    fn get_priority_impl(&self, out: *mut u32) -> Result<()> {
        let value = self.read()?.priority;
        // SAFETY: Caller passes a writable `u32` output slot.
        unsafe { ffi_ptr::write_out(out, value, E_INVALIDARG) }
    }

    fn set_lod_impl(&self, lod: u32) -> Result<()> {
        self.write()?.lod = lod;
        Ok(())
    }

    fn get_lod_impl(&self, out: *mut u32) -> Result<()> {
        let value = self.read()?.lod;
        // SAFETY: Caller passes a writable `u32` output slot.
        unsafe { ffi_ptr::write_out(out, value, E_INVALIDARG) }
    }

    fn ensure_overlay(&self) -> Result<()> {
        if !self.read()?.is_overlay() {
            return Err(DDERR_NOTAOVERLAYSURFACE.into());
        }
        Ok(())
    }

    fn get_overlay_position_impl(&self, x: *mut i32, y: *mut i32) -> Result<()> {
        self.ensure_overlay()?;
        if x.is_null() || y.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let data = self.read()?;
        // SAFETY: Output slots are non-null and writable (and may coincide, so write sequentially).
        unsafe {
            x.write(data.overlay.x);
            y.write(data.overlay.y);
        }
        Ok(())
    }

    fn set_overlay_position_impl(&self, x: i32, y: i32) -> Result<()> {
        self.ensure_overlay()?;
        let mut data = self.write()?;
        data.overlay.x = x;
        data.overlay.y = y;
        Ok(())
    }

    fn update_overlay_impl<S: Interface>(
        &self,
        src_rect: *mut RECT,
        dest: Ref<S>,
        dest_rect: *mut RECT,
        flags: u32,
        fx: *mut DDOVERLAYFX,
    ) -> Result<()> {
        self.ensure_overlay()?;
        let dest_obj = dest.ok()?.cast_object_ref::<Surface>()?;
        // SAFETY: Caller passes nullable readable `RECT` and `DDOVERLAYFX` COM inputs.
        let dest_rect = unsafe { ffi_ptr::opt_copy(dest_rect) };
        // SAFETY: Caller passes nullable readable `DDOVERLAYFX` for the duration of this call.
        let fx_ref = unsafe { ffi_ptr::opt_ref(fx) };
        let d_rect = dest_obj.read()?.bounds(dest_rect)?;
        let src_key = if flags & DDOVER_KEYSRCOVERRIDE as u32 != 0 {
            Some(fx_ref.ok_or(E_INVALIDARG)?.dckSrcColorkey)
        } else if flags & DDOVER_KEYSRC as u32 != 0 {
            Some(self.read()?.src_overlay_key.ok_or(DDERR_NOCOLORKEY)?)
        } else {
            None
        };
        let dst_key = if flags & DDOVER_KEYDESTOVERRIDE as u32 != 0 {
            Some(fx_ref.ok_or(E_INVALIDARG)?.dckDestColorkey)
        } else if flags & DDOVER_KEYDEST as u32 != 0 {
            Some(dest_obj.read()?.dest_overlay_key.ok_or(DDERR_NOCOLORKEY)?)
        } else {
            None
        };
        {
            let mut data = self.write()?;
            data.overlay.x = d_rect.left;
            data.overlay.y = d_rect.top;
            if flags & DDOVER_HIDE as u32 != 0 {
                data.overlay.visible = false;
            } else if flags & DDOVER_SHOW as u32 != 0 {
                data.overlay.visible = true;
            }
        }
        if flags & DDOVER_HIDE as u32 == 0 && self.read()?.overlay.visible {
            // SAFETY: Caller passes nullable readable source `RECT`.
            let src_copy = unsafe { ffi_ptr::opt_copy(src_rect) };
            dest_obj.copy(
                self,
                src_copy,
                d_rect,
                BltOptions {
                    src_key,
                    dst_key,
                    mirror_lr: false,
                    mirror_ud: false,
                },
                true,
            )?;
            dest_obj.refresh()?;
        }
        Ok(())
    }

    fn enum_attached_surfaces_v2<S: Interface>(
        &self,
        mut emit: impl FnMut(S, &mut DDSURFACEDESC2) -> i32,
    ) -> Result<()> {
        let snapshot: Vec<IDirectDrawSurface> = self.attached_lock().clone();
        for s in snapshot {
            let mut desc = descriptor::desc_to_desc2(&surface_desc(&s)?);
            let typed = s.cast::<S>()?;
            if emit(typed, &mut desc) == DDENUMRET_CANCEL as i32 {
                break;
            }
        }
        Ok(())
    }
}

impl IDirectDrawGammaControl_Impl for Surface_Impl {
    fn GetGammaRamp(&self, flags: u32, ramp: *mut DDGAMMARAMP) -> Result<()> {
        if flags != 0 {
            return Err(E_INVALIDARG.into());
        }
        let value = self.read()?.gamma.unwrap_or_else(identity_gamma_ramp);
        // SAFETY: Caller passes a writable `DDGAMMARAMP` output pointer.
        unsafe { ffi_ptr::write_out(ramp, value, E_INVALIDARG) }
    }

    fn SetGammaRamp(&self, flags: u32, ramp: *mut DDGAMMARAMP) -> Result<()> {
        if flags & !(DDSGR_CALIBRATE as u32) != 0 {
            return Err(E_INVALIDARG.into());
        }
        // SAFETY: Caller passes a readable `DDGAMMARAMP` input pointer.
        let ramp = unsafe { ffi_ptr::req_ref(ramp, E_INVALIDARG)? };
        self.write()?.gamma = Some(*ramp);
        self.refresh()
    }
}

impl IDirectDrawSurface_Impl for Surface_Impl {
    fn Blt(
        &self,
        dest: *mut RECT,
        source: Ref<IDirectDrawSurface>,
        src: *mut RECT,
        flags: u32,
        fx: *mut DDBLTFX,
    ) -> Result<()> {
        self.blt_impl(dest, source, src, flags, fx)
    }

    fn BltFast(
        &self,
        x: u32,
        y: u32,
        source: Ref<IDirectDrawSurface>,
        src_rect: *mut RECT,
        flags: u32,
    ) -> Result<()> {
        self.blt_fast_impl(x, y, source, src_rect, flags)
    }

    fn GetAttachedSurface(
        &self,
        caps: *mut DDSCAPS,
        out: OutRef<IDirectDrawSurface>,
    ) -> Result<()> {
        // SAFETY: Caller passes a readable `DDSCAPS` pointer.
        let caps_val = unsafe { ffi_ptr::req_ref(caps, E_INVALIDARG)? }.dwCaps;
        self.get_attached_surface_impl(caps_val, out)
    }

    fn EnumAttachedSurfaces(
        &self,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK,
    ) -> Result<()> {
        let callback = callback.ok_or(E_INVALIDARG)?;
        let snapshot: Vec<IDirectDrawSurface> = self.attached_lock().clone();
        for s in snapshot {
            let mut desc = surface_desc(&s)?;
            let owned = ManuallyDrop::new(s);
            // SAFETY: DirectDraw transfers one surface ref to the caller's enumeration callback.
            if unsafe { callback((&*owned).into(), &mut desc, context) }.0
                == DDENUMRET_CANCEL as i32
            {
                break;
            }
        }
        Ok(())
    }

    fn Flip(&self, target: Ref<IDirectDrawSurface>, flags: u32) -> Result<()> {
        self.flip_impl(target, flags)
    }

    fn GetCaps(&self, caps: *mut DDSCAPS) -> Result<()> {
        let value = DDSCAPS {
            dwCaps: self.read()?.caps,
        };
        // SAFETY: Caller passes a writable `DDSCAPS` output pointer.
        unsafe { ffi_ptr::write_out(caps, value, E_INVALIDARG) }
    }

    fn SetColorKey(&self, flags: u32, key: *mut DDCOLORKEY) -> Result<()> {
        let which = flags & !(DDCKEY_COLORSPACE as u32);
        // SAFETY: Caller passes nullable readable `DDCOLORKEY` pointer.
        let mut key = unsafe { ffi_ptr::opt_copy(key) };
        if flags & DDCKEY_COLORSPACE as u32 == 0 {
            if let Some(key) = &mut key {
                key.dwColorSpaceHighValue = key.dwColorSpaceLowValue;
            }
        }
        let mut data = self.write()?;
        match which as i32 {
            DDCKEY_SRCBLT => data.source_key = key,
            DDCKEY_DESTBLT => data.dest_key = key,
            DDCKEY_SRCOVERLAY => data.src_overlay_key = key,
            DDCKEY_DESTOVERLAY => data.dest_overlay_key = key,
            _ => return unsupported(),
        }
        Ok(())
    }

    fn GetColorKey(&self, flags: u32, key: *mut DDCOLORKEY) -> Result<()> {
        // SAFETY: Caller passes a writable `DDCOLORKEY` output pointer.
        let out = unsafe { ffi_ptr::out_slot(key, E_INVALIDARG)? };
        let data = self.read()?;
        let stored = match flags as i32 {
            DDCKEY_SRCBLT => data.source_key,
            DDCKEY_DESTBLT => data.dest_key,
            DDCKEY_SRCOVERLAY => data.src_overlay_key,
            DDCKEY_DESTOVERLAY => data.dest_overlay_key,
            _ => return unsupported(),
        };
        out.write(stored.ok_or(DDERR_NOCOLORKEY)?);
        Ok(())
    }

    fn SetPalette(&self, palette: Ref<IDirectDrawPalette>) -> Result<()> {
        let mut data = self.write()?;
        if data.format != Format::Indexed {
            return Err(DDERR_INVALIDPIXELFORMAT.into());
        }
        if let Some(palette) = palette.as_ref() {
            crate::palette::check_owner(palette, &self.display)?;
        }
        data.palette = palette.cloned().map(ffi_ptr::SendCom);
        let locked = data.locked();
        drop(data);
        if locked {
            Ok(())
        } else {
            self.refresh()
        }
    }

    fn GetPalette(&self) -> Result<IDirectDrawPalette> {
        self.read()?
            .palette
            .as_ref()
            .map(|p| p.0.clone())
            .ok_or_else(|| Error::from(DDERR_NOPALETTEATTACHED))
    }

    fn GetPixelFormat(&self, out: *mut DDPIXELFORMAT) -> Result<()> {
        // SAFETY: Caller passes a sized `DDPIXELFORMAT` output buffer.
        unsafe { descriptor::output(out)? }.write(self.read()?.format.description());
        Ok(())
    }

    fn GetSurfaceDesc(&self, out: *mut DDSURFACEDESC) -> Result<()> {
        // SAFETY: Caller passes a sized `DDSURFACEDESC` output buffer.
        let out = unsafe { descriptor::output(out)? };
        let count = self.backbuffer_chain_len();
        out.write(self.read()?.desc(count));
        Ok(())
    }

    fn Lock(&self, rect: *mut RECT, out: *mut DDSURFACEDESC, flags: u32, _: HANDLE) -> Result<()> {
        // SAFETY: Caller passes a sized `DDSURFACEDESC` output buffer.
        unsafe { descriptor::output(out)? }.write(self.lock_desc(rect, flags)?);
        Ok(())
    }

    fn Unlock(&self, address: *mut c_void) -> Result<()> {
        self.unlock_ptr_impl(address)
    }

    fn IsLost(&self) -> Result<()> {
        Ok(())
    }

    fn Restore(&self) -> Result<()> {
        Ok(())
    }

    fn GetBltStatus(&self, flags: u32) -> Result<()> {
        match flags as i32 {
            DDGBS_ISBLTDONE => Ok(()),
            DDGBS_CANBLT => self.read()?.ready(),
            _ => Err(E_INVALIDARG.into()),
        }
    }

    fn GetFlipStatus(&self, flags: u32) -> Result<()> {
        let chain = self.flip_chain()?;
        if chain.is_empty() {
            return Err(DDERR_NOTFLIPPABLE.into());
        }
        match flags as i32 {
            DDGFS_ISFLIPDONE => Ok(()),
            DDGFS_CANFLIP => {
                self.read()?.ready()?;
                for s in &chain {
                    s.cast_object_ref::<Surface>()?.read()?.ready()?;
                }
                Ok(())
            }
            _ => Err(E_INVALIDARG.into()),
        }
    }

    fn Initialize(&self, _: Ref<IDirectDraw>, _: *mut DDSURFACEDESC) -> Result<()> {
        Err(DDERR_ALREADYINITIALIZED.into())
    }

    fn GetClipper(&self) -> Result<IDirectDrawClipper> {
        self.get_clipper_impl()
    }

    fn SetClipper(&self, clipper: Ref<IDirectDrawClipper>) -> Result<()> {
        self.set_clipper_impl(clipper)
    }

    fn AddAttachedSurface(&self, surface: Ref<IDirectDrawSurface>) -> Result<()> {
        self.add_attached_surface_impl(surface)
    }

    fn DeleteAttachedSurface(&self, flags: u32, surface: Ref<IDirectDrawSurface>) -> Result<()> {
        self.delete_attached_surface_impl(flags, surface)
    }

    fn AddOverlayDirtyRect(&self, rect: *mut RECT) -> Result<()> {
        self.ensure_overlay()?;
        // SAFETY: Caller passes nullable readable dirty `RECT`.
        self.read()?.bounds(unsafe { ffi_ptr::opt_copy(rect) })?;
        Ok(())
    }

    fn BltBatch(&self, batch: *mut DDBLTBATCH, count: u32, flags: u32) -> Result<()> {
        self.blt_batch_impl(batch, count, flags)
    }

    fn EnumOverlayZOrders(
        &self,
        flags: u32,
        _: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK,
    ) -> Result<()> {
        let _ = callback.ok_or(E_INVALIDARG)?;
        if flags != DDENUMOVERLAYZ_FRONTTOBACK as u32 && flags != DDENUMOVERLAYZ_BACKTOFRONT as u32
        {
            return Err(E_INVALIDARG.into());
        }
        Ok(())
    }

    fn GetDC(&self, out: *mut HDC) -> Result<()> {
        // SAFETY: Caller passes a writable `HDC` output pointer.
        let out = unsafe { ffi_ptr::out_slot(out, E_INVALIDARG)? };
        out.write(HDC::default());
        let tid = thread::current().id();
        {
            let data = self.data.lock();
            if matches!(data.access, Access::Gdi { owner_tid, .. } if owner_tid == tid) {
                return Err(DDERR_DCALREADYCREATED.into());
            }
        }
        let mut data = self.data.lock_ready(true)?;
        let entries = if data.format == Format::Indexed {
            Some(data.palette_entries()?)
        } else {
            None
        };
        let own_dc = data.caps & DDSCAPS_OWNDC as u32 != 0;
        let uniqueness = data.uniqueness;
        let len = data.storage.image_len;
        let pitch = data.storage.pitch;
        data.ensure_cached_dc()?;
        let src_start = data.storage.pixel_offset;
        let SurfaceData {
            ref storage,
            ref mut cached_dc,
            ..
        } = *data;
        let dc = cached_dc.as_mut().expect("cached_dc initialized");
        let (handle, saved_state) = dc.begin_drawing(
            &storage.pixels[src_start..src_start + len],
            pitch,
            uniqueness,
            entries.as_ref(),
            own_dc,
        );
        out.write(handle);
        data.access = Access::Gdi {
            handle: SendHdc(handle),
            saved_state,
            owner_tid: tid,
        };
        Ok(())
    }

    fn ReleaseDC(&self, handle: HDC) -> Result<()> {
        let mut data = self.write()?;
        let Access::Gdi {
            handle: dc_handle,
            saved_state,
            ..
        } = data.access
        else {
            return Err(DDERR_NODC.into());
        };
        if handle != dc_handle.0 {
            return Err(E_INVALIDARG.into());
        }
        let len = data.storage.image_len;
        let pitch = data.storage.pitch;
        let dst_start = data.storage.pixel_offset;
        data.bump_uniqueness();
        let new_uniqueness = data.uniqueness;
        let SurfaceData {
            ref mut storage,
            ref mut cached_dc,
            ..
        } = *data;
        if let Some(dc) = cached_dc.as_mut() {
            dc.finish_drawing(
                &mut storage.pixels[dst_start..dst_start + len],
                pitch,
                new_uniqueness,
                saved_state,
            );
        }
        data.access = Access::Ready;
        drop(data);
        self.data.notify_ready();
        self.refresh()
    }

    fn GetOverlayPosition(&self, x: *mut i32, y: *mut i32) -> Result<()> {
        self.get_overlay_position_impl(x, y)
    }

    fn SetOverlayPosition(&self, x: i32, y: i32) -> Result<()> {
        self.set_overlay_position_impl(x, y)
    }

    fn UpdateOverlay(
        &self,
        src_rect: *mut RECT,
        dest: Ref<IDirectDrawSurface>,
        dest_rect: *mut RECT,
        flags: u32,
        fx: *mut DDOVERLAYFX,
    ) -> Result<()> {
        self.update_overlay_impl(src_rect, dest, dest_rect, flags, fx)
    }

    fn UpdateOverlayDisplay(&self, _: u32) -> Result<()> {
        self.refresh()
    }

    fn UpdateOverlayZOrder(&self, _: u32, _: Ref<IDirectDrawSurface>) -> Result<()> {
        self.ensure_overlay()
    }
}

macro_rules! delegate_surface_common {
    ($iface:ty) => {
        fn AddAttachedSurface(&self, surface: Ref<$iface>) -> Result<()> {
            self.add_attached_surface_impl(surface)
        }
        fn AddOverlayDirtyRect(&self, r: *mut RECT) -> Result<()> {
            IDirectDrawSurface_Impl::AddOverlayDirtyRect(self, r)
        }
        fn Blt(
            &self,
            dest: *mut RECT,
            source: Ref<$iface>,
            src: *mut RECT,
            flags: u32,
            fx: *mut DDBLTFX,
        ) -> Result<()> {
            self.blt_impl(dest, source, src, flags, fx)
        }
        fn BltBatch(&self, batch: *mut DDBLTBATCH, count: u32, flags: u32) -> Result<()> {
            self.blt_batch_impl(batch, count, flags)
        }
        fn BltFast(
            &self,
            x: u32,
            y: u32,
            source: Ref<$iface>,
            src_rect: *mut RECT,
            flags: u32,
        ) -> Result<()> {
            self.blt_fast_impl(x, y, source, src_rect, flags)
        }
        fn DeleteAttachedSurface(&self, flags: u32, surface: Ref<$iface>) -> Result<()> {
            self.delete_attached_surface_impl(flags, surface)
        }
        fn Flip(&self, target: Ref<$iface>, flags: u32) -> Result<()> {
            self.flip_impl(target, flags)
        }
        fn GetBltStatus(&self, flags: u32) -> Result<()> {
            IDirectDrawSurface_Impl::GetBltStatus(self, flags)
        }
        fn GetClipper(&self) -> Result<IDirectDrawClipper> {
            self.get_clipper_impl()
        }
        fn GetColorKey(&self, flags: u32, key: *mut DDCOLORKEY) -> Result<()> {
            IDirectDrawSurface_Impl::GetColorKey(self, flags, key)
        }
        fn GetDC(&self, out: *mut HDC) -> Result<()> {
            IDirectDrawSurface_Impl::GetDC(self, out)
        }
        fn GetFlipStatus(&self, flags: u32) -> Result<()> {
            IDirectDrawSurface_Impl::GetFlipStatus(self, flags)
        }
        fn GetOverlayPosition(&self, x: *mut i32, y: *mut i32) -> Result<()> {
            self.get_overlay_position_impl(x, y)
        }
        fn GetPalette(&self) -> Result<IDirectDrawPalette> {
            IDirectDrawSurface_Impl::GetPalette(self)
        }
        fn GetPixelFormat(&self, out: *mut DDPIXELFORMAT) -> Result<()> {
            IDirectDrawSurface_Impl::GetPixelFormat(self, out)
        }
        fn IsLost(&self) -> Result<()> {
            Ok(())
        }
        fn ReleaseDC(&self, handle: HDC) -> Result<()> {
            IDirectDrawSurface_Impl::ReleaseDC(self, handle)
        }
        fn Restore(&self) -> Result<()> {
            Ok(())
        }
        fn SetClipper(&self, clipper: Ref<IDirectDrawClipper>) -> Result<()> {
            self.set_clipper_impl(clipper)
        }
        fn SetColorKey(&self, flags: u32, key: *mut DDCOLORKEY) -> Result<()> {
            IDirectDrawSurface_Impl::SetColorKey(self, flags, key)
        }
        fn SetOverlayPosition(&self, x: i32, y: i32) -> Result<()> {
            self.set_overlay_position_impl(x, y)
        }
        fn SetPalette(&self, palette: Ref<IDirectDrawPalette>) -> Result<()> {
            IDirectDrawSurface_Impl::SetPalette(self, palette)
        }
        fn UpdateOverlay(
            &self,
            src_rect: *mut RECT,
            dest: Ref<$iface>,
            dest_rect: *mut RECT,
            flags: u32,
            fx: *mut DDOVERLAYFX,
        ) -> Result<()> {
            self.update_overlay_impl(src_rect, dest, dest_rect, flags, fx)
        }
        fn UpdateOverlayDisplay(&self, flags: u32) -> Result<()> {
            IDirectDrawSurface_Impl::UpdateOverlayDisplay(self, flags)
        }
        fn UpdateOverlayZOrder(&self, _: u32, _: Ref<$iface>) -> Result<()> {
            self.ensure_overlay()
        }
        fn GetDDInterface(&self, out: *mut *mut c_void) -> Result<()> {
            self.get_dd_interface_impl(out)
        }
        fn PageLock(&self, _: u32) -> Result<()> {
            Ok(())
        }
        fn PageUnlock(&self, _: u32) -> Result<()> {
            Ok(())
        }
    };
}

macro_rules! delegate_surface_v1_layout {
    ($iface:ty) => {
        fn EnumAttachedSurfaces(
            &self,
            context: *mut c_void,
            callback: LPDDENUMSURFACESCALLBACK,
        ) -> Result<()> {
            IDirectDrawSurface_Impl::EnumAttachedSurfaces(self, context, callback)
        }
        fn EnumOverlayZOrders(
            &self,
            flags: u32,
            context: *mut c_void,
            callback: LPDDENUMSURFACESCALLBACK,
        ) -> Result<()> {
            IDirectDrawSurface_Impl::EnumOverlayZOrders(self, flags, context, callback)
        }
        fn GetAttachedSurface(&self, caps: *mut DDSCAPS, out: OutRef<$iface>) -> Result<()> {
            // SAFETY: Caller passes a readable `DDSCAPS` pointer.
            let caps_val = unsafe { ffi_ptr::req_ref(caps, E_INVALIDARG)? }.dwCaps;
            self.get_attached_surface_impl(caps_val, out)
        }
        fn GetCaps(&self, caps: *mut DDSCAPS) -> Result<()> {
            IDirectDrawSurface_Impl::GetCaps(self, caps)
        }
        fn GetSurfaceDesc(&self, out: *mut DDSURFACEDESC) -> Result<()> {
            IDirectDrawSurface_Impl::GetSurfaceDesc(self, out)
        }
        fn Initialize(&self, dd: Ref<IDirectDraw>, desc: *mut DDSURFACEDESC) -> Result<()> {
            IDirectDrawSurface_Impl::Initialize(self, dd, desc)
        }
        fn Lock(
            &self,
            rect: *mut RECT,
            out: *mut DDSURFACEDESC,
            flags: u32,
            ev: HANDLE,
        ) -> Result<()> {
            IDirectDrawSurface_Impl::Lock(self, rect, out, flags, ev)
        }
        fn Unlock(&self, address: *mut c_void) -> Result<()> {
            self.unlock_ptr_impl(address)
        }
    };
}

impl IDirectDrawSurface2_Impl for Surface_Impl {
    delegate_surface_common!(IDirectDrawSurface2);
    delegate_surface_v1_layout!(IDirectDrawSurface2);
}

impl IDirectDrawSurface3_Impl for Surface_Impl {
    delegate_surface_common!(IDirectDrawSurface3);
    delegate_surface_v1_layout!(IDirectDrawSurface3);

    fn SetSurfaceDesc(&self, desc: *mut DDSURFACEDESC, flags: u32) -> Result<()> {
        // SAFETY: Caller passes a sized `DDSURFACEDESC` and optional valid `DDSD_LPSURFACE` pixels.
        unsafe { self.set_surface_desc_impl(&descriptor::read(desc)?, flags) }
    }
}

impl IDirectDrawSurface4_Impl for Surface_Impl {
    delegate_surface_common!(IDirectDrawSurface4);

    fn EnumAttachedSurfaces(
        &self,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK2,
    ) -> Result<()> {
        let callback = callback.ok_or(E_INVALIDARG)?;
        self.enum_attached_surfaces_v2::<IDirectDrawSurface4>(|typed, desc| {
            let owned = ManuallyDrop::new(typed);
            // SAFETY: DirectDraw transfers one surface ref to the caller's enumeration callback.
            unsafe { callback((&*owned).into(), desc, context).0 }
        })
    }

    fn EnumOverlayZOrders(
        &self,
        flags: u32,
        _: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK2,
    ) -> Result<()> {
        let _ = callback.ok_or(E_INVALIDARG)?;
        if flags != DDENUMOVERLAYZ_FRONTTOBACK as u32 && flags != DDENUMOVERLAYZ_BACKTOFRONT as u32
        {
            return Err(E_INVALIDARG.into());
        }
        Ok(())
    }

    fn GetAttachedSurface(
        &self,
        caps: *mut DDSCAPS2,
        out: OutRef<IDirectDrawSurface4>,
    ) -> Result<()> {
        // SAFETY: Caller passes a readable `DDSCAPS2` pointer.
        let caps_val = unsafe { ffi_ptr::req_ref(caps, E_INVALIDARG)? }.dwCaps;
        self.get_attached_surface_impl(caps_val, out)
    }

    fn GetCaps(&self, caps: *mut DDSCAPS2) -> Result<()> {
        let value = DDSCAPS2 {
            dwCaps: self.read()?.caps,
            ..Default::default()
        };
        // SAFETY: Caller passes a writable `DDSCAPS2` output pointer.
        unsafe { ffi_ptr::write_out(caps, value, E_INVALIDARG) }
    }

    fn GetSurfaceDesc(&self, out: *mut DDSURFACEDESC2) -> Result<()> {
        // SAFETY: Caller passes a sized `DDSURFACEDESC2` output buffer.
        let out = unsafe { descriptor::output(out)? };
        let count = self.backbuffer_chain_len();
        out.write(descriptor::desc_to_desc2(&self.read()?.desc(count)));
        Ok(())
    }

    fn Initialize(&self, _: Ref<IDirectDraw>, _: *mut DDSURFACEDESC2) -> Result<()> {
        Err(DDERR_ALREADYINITIALIZED.into())
    }

    fn Lock(&self, rect: *mut RECT, out: *mut DDSURFACEDESC2, flags: u32, _: HANDLE) -> Result<()> {
        // SAFETY: Caller passes a sized `DDSURFACEDESC2` output buffer.
        unsafe { descriptor::output(out)? }
            .write(descriptor::desc_to_desc2(&self.lock_desc(rect, flags)?));
        Ok(())
    }

    fn Unlock(&self, rect: *mut RECT) -> Result<()> {
        self.unlock_rect_impl(rect)
    }

    fn SetSurfaceDesc(&self, desc: *mut DDSURFACEDESC2, flags: u32) -> Result<()> {
        // SAFETY: Caller passes a sized `DDSURFACEDESC2` and optional valid `DDSD_LPSURFACE` pixels.
        unsafe { self.set_surface_desc_impl(&descriptor::read(desc)?, flags) }
    }

    fn SetPrivateData(
        &self,
        tag: *const GUID,
        data: *mut c_void,
        size: u32,
        flags: u32,
    ) -> Result<()> {
        self.set_private_data_impl(tag, data, size, flags)
    }

    fn GetPrivateData(&self, tag: *const GUID, buffer: *mut c_void, size: *mut u32) -> Result<()> {
        self.get_private_data_impl(tag, buffer, size)
    }

    fn FreePrivateData(&self, tag: *const GUID) -> Result<()> {
        self.free_private_data_impl(tag)
    }

    fn GetUniquenessValue(&self, out: *mut u32) -> Result<()> {
        self.get_uniqueness_value_impl(out)
    }

    fn ChangeUniquenessValue(&self) -> Result<()> {
        self.change_uniqueness_value_impl()
    }
}

impl IDirectDrawSurface7_Impl for Surface_Impl {
    delegate_surface_common!(IDirectDrawSurface7);

    fn EnumAttachedSurfaces(
        &self,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK7,
    ) -> Result<()> {
        let callback = callback.ok_or(E_INVALIDARG)?;
        self.enum_attached_surfaces_v2::<IDirectDrawSurface7>(|typed, desc| {
            let owned = ManuallyDrop::new(typed);
            // SAFETY: DirectDraw transfers one surface ref to the caller's enumeration callback.
            unsafe { callback((&*owned).into(), desc, context).0 }
        })
    }

    fn EnumOverlayZOrders(
        &self,
        flags: u32,
        _: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK7,
    ) -> Result<()> {
        let _ = callback.ok_or(E_INVALIDARG)?;
        if flags != DDENUMOVERLAYZ_FRONTTOBACK as u32 && flags != DDENUMOVERLAYZ_BACKTOFRONT as u32
        {
            return Err(E_INVALIDARG.into());
        }
        Ok(())
    }

    fn GetAttachedSurface(
        &self,
        caps: *mut DDSCAPS2,
        out: OutRef<IDirectDrawSurface7>,
    ) -> Result<()> {
        // SAFETY: Caller passes a readable `DDSCAPS2` pointer.
        let caps_val = unsafe { ffi_ptr::req_ref(caps, E_INVALIDARG)? }.dwCaps;
        self.get_attached_surface_impl(caps_val, out)
    }

    fn GetCaps(&self, caps: *mut DDSCAPS2) -> Result<()> {
        IDirectDrawSurface4_Impl::GetCaps(self, caps)
    }

    fn GetSurfaceDesc(&self, out: *mut DDSURFACEDESC2) -> Result<()> {
        IDirectDrawSurface4_Impl::GetSurfaceDesc(self, out)
    }

    fn Initialize(&self, dd: Ref<IDirectDraw>, desc: *mut DDSURFACEDESC2) -> Result<()> {
        IDirectDrawSurface4_Impl::Initialize(self, dd, desc)
    }

    fn Lock(
        &self,
        rect: *mut RECT,
        out: *mut DDSURFACEDESC2,
        flags: u32,
        ev: HANDLE,
    ) -> Result<()> {
        IDirectDrawSurface4_Impl::Lock(self, rect, out, flags, ev)
    }

    fn Unlock(&self, rect: *mut RECT) -> Result<()> {
        self.unlock_rect_impl(rect)
    }

    fn SetSurfaceDesc(&self, desc: *mut DDSURFACEDESC2, flags: u32) -> Result<()> {
        IDirectDrawSurface4_Impl::SetSurfaceDesc(self, desc, flags)
    }

    fn SetPrivateData(
        &self,
        tag: *const GUID,
        data: *mut c_void,
        size: u32,
        flags: u32,
    ) -> Result<()> {
        self.set_private_data_impl(tag, data, size, flags)
    }

    fn GetPrivateData(&self, tag: *const GUID, buffer: *mut c_void, size: *mut u32) -> Result<()> {
        self.get_private_data_impl(tag, buffer, size)
    }

    fn FreePrivateData(&self, tag: *const GUID) -> Result<()> {
        self.free_private_data_impl(tag)
    }

    fn GetUniquenessValue(&self, out: *mut u32) -> Result<()> {
        self.get_uniqueness_value_impl(out)
    }

    fn ChangeUniquenessValue(&self) -> Result<()> {
        self.change_uniqueness_value_impl()
    }

    fn SetPriority(&self, priority: u32) -> Result<()> {
        self.set_priority_impl(priority)
    }

    fn GetPriority(&self, out: *mut u32) -> Result<()> {
        self.get_priority_impl(out)
    }

    fn SetLOD(&self, lod: u32) -> Result<()> {
        self.set_lod_impl(lod)
    }

    fn GetLOD(&self, out: *mut u32) -> Result<()> {
        self.get_lod_impl(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::DDERR_SURFACEBUSY;

    #[test]
    fn custom_surface_pitch_round_trips_through_gdi_and_rejects_partial_resize() {
        use windows::Win32::Foundation::COLORREF;
        use windows::Win32::Graphics::Gdi::{GetPixel, SetPixelV};

        let display = DisplayShared::new(2, 2, 32);
        let mut desc = DDSURFACEDESC {
            dwSize: size_of::<DDSURFACEDESC>() as u32,
            dwFlags: (DDSD_WIDTH | DDSD_HEIGHT | DDSD_CAPS) as u32,
            dwWidth: 2,
            dwHeight: 2,
            ddsCaps: DDSCAPS {
                dwCaps: DDSCAPS_OFFSCREENPLAIN as u32,
            },
            ..Default::default()
        };
        // Two visible eight-byte rows at pitch 12. There is no final padding.
        let mut pixels = [0xadu8; 20];
        pixels[12..16].copy_from_slice(&[1, 2, 3, 0]);
        desc.dwFlags |= (DDSD_PITCH | DDSD_LPSURFACE) as u32;
        desc.Anonymous1.lPitch = 12;
        desc.lpSurface = pixels.as_mut_ptr().cast();
        // SAFETY: All COM input/output buffers live through the calls and `pixels` backs `desc`.
        unsafe {
            let surface: IDirectDrawSurface3 = create_surface(display.clone(), &desc)
                .unwrap()
                .cast()
                .unwrap();
            let obj = surface.cast_object_ref::<Surface>().unwrap();
            {
                let data = obj.read().unwrap();
                assert_eq!(data.pitch(), 8);
                assert_eq!(&data.pixel_slice()[8..12], &[1, 2, 3, 0]);
            }
            surface.SetSurfaceDesc(&mut desc, 0).unwrap();
            {
                let mut data = obj.write().unwrap();
                // Import skipped the external 0xad padding. External code
                // may subsequently use padding in the surface it has locked.
                assert_eq!(&data.pixel_slice()[8..12], &[0; 4]);
                assert_eq!(&data.pixel_slice()[20..24], &[0; 4]);
                data.pixel_slice_mut()[20..24].fill(0x57);
            }
            desc.dwFlags = (DDSD_WIDTH | DDSD_PITCH) as u32;
            desc.dwWidth = 4;
            assert_eq!(
                surface.SetSurfaceDesc(&mut desc, 0).unwrap_err().code(),
                E_INVALIDARG
            );
            let mut dc = HDC::default();
            surface.GetDC(&mut dc).unwrap();
            assert_eq!(GetPixel(dc, 0, 1), COLORREF(0x010203));
            assert!(SetPixelV(dc, 1, 1, COLORREF(0x080706)).as_bool());
            surface.ReleaseDC(dc).unwrap();
            surface.GetSurfaceDesc(&mut desc).unwrap();
            assert_eq!(
                (desc.dwWidth, desc.dwHeight, desc.Anonymous1.lPitch),
                (2, 2, 12)
            );
            let duplicate = duplicate_surface(display, &surface.cast().unwrap()).unwrap();
            let data = surface
                .cast_object_ref::<Surface>()
                .unwrap()
                .read()
                .unwrap();
            assert_eq!(&data.pixel_slice()[16..19], &[8, 7, 6]);
            assert_eq!(&data.pixel_slice()[20..24], &[0x57; 4]);
            let copy = duplicate
                .cast_object_ref::<Surface>()
                .unwrap()
                .read()
                .unwrap();
            assert_eq!(copy.pitch(), 12);
            assert_eq!(copy.pixel_slice(), data.pixel_slice());
        }
    }

    #[test]
    fn cross_thread_primary_lock_and_blt_never_fail_with_null_surface() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Barrier;

        let display = crate::draw::DisplayShared::new(1024, 768, 16);
        let prim_desc = DDSURFACEDESC {
            dwSize: size_of::<DDSURFACEDESC>() as u32,
            dwFlags: DDSD_CAPS as u32,
            ddsCaps: DDSCAPS {
                dwCaps: DDSCAPS_PRIMARYSURFACE as u32,
            },
            ..Default::default()
        };
        let back_desc = DDSURFACEDESC {
            dwSize: size_of::<DDSURFACEDESC>() as u32,
            dwFlags: (DDSD_CAPS | DDSD_WIDTH | DDSD_HEIGHT) as u32,
            dwWidth: 1024,
            dwHeight: 768,
            ddsCaps: DDSCAPS {
                dwCaps: (DDSCAPS_OFFSCREENPLAIN | DDSCAPS_SYSTEMMEMORY) as u32,
            },
            ..Default::default()
        };
        // SAFETY: Both descriptors request owned surface memory (no `DDSD_LPSURFACE`).
        let primary = unsafe { create_surface(display.clone(), &prim_desc) }.unwrap();
        // SAFETY: Same for offscreen backbuffer descriptor.
        let back = unsafe { create_surface(display.clone(), &back_desc) }.unwrap();

        // Same-thread Lock while GetDC is held must still fail immediately (no self-deadlock).
        let mut dc = HDC::default();
        // SAFETY: `dc` is a valid writable output handle.
        unsafe { primary.GetDC(&mut dc) }.unwrap();
        let mut same_thread_desc = DDSURFACEDESC {
            dwSize: size_of::<DDSURFACEDESC>() as u32,
            ..Default::default()
        };
        assert_eq!(
            // SAFETY: Valid COM Lock call with initialized `dwSize`.
            unsafe {
                primary.Lock(
                    std::ptr::null_mut(),
                    &mut same_thread_desc,
                    0,
                    HANDLE::default(),
                )
            }
            .unwrap_err()
            .code(),
            DDERR_SURFACEBUSY
        );
        // SAFETY: Releasing the DC obtained above.
        unsafe { primary.ReleaseDC(dc) }.unwrap();

        // Cross-thread DONOTWAIT must still return DDERR_SURFACEBUSY immediately when locked.
        let mut main_lock = DDSURFACEDESC {
            dwSize: size_of::<DDSURFACEDESC>() as u32,
            ..Default::default()
        };
        // SAFETY: Valid COM Lock call with initialized `dwSize`.
        unsafe { primary.Lock(std::ptr::null_mut(), &mut main_lock, 0, HANDLE::default()) }
            .unwrap();
        let weak_primary = primary.downgrade().unwrap();
        let worker_surface = weak_primary.clone();
        let dnw_code = std::thread::spawn(move || {
            let s = worker_surface.upgrade().unwrap();
            let mut d = DDSURFACEDESC {
                dwSize: size_of::<DDSURFACEDESC>() as u32,
                ..Default::default()
            };
            // SAFETY: Valid COM Lock call with initialized `dwSize`.
            unsafe {
                s.Lock(
                    std::ptr::null_mut(),
                    &mut d,
                    DDLOCK_DONOTWAIT as u32,
                    HANDLE::default(),
                )
            }
            .unwrap_err()
            .code()
        })
        .join()
        .unwrap();
        assert_eq!(dnw_code, DDERR_SURFACEBUSY);
        // SAFETY: Unlocking the pointer returned by `Lock`.
        unsafe { primary.Unlock(main_lock.lpSurface) }.unwrap();

        // Stress test: simulate RA2's background cursor worker thread calling Primary->Lock()
        // and reading a 37x28 rect at (82, 353) concurrently with main-thread Blt + Lock + present reads.
        let done = Arc::new(AtomicBool::new(false));
        let start = Arc::new(Barrier::new(2));
        let done_worker = Arc::clone(&done);
        let start_worker = Arc::clone(&start);
        let worker = std::thread::spawn(move || {
            let s = weak_primary.upgrade().unwrap();
            start_worker.wait();
            let mut cursor_buf = [0u16; 37 * 28];
            let mut locks = 0u32;
            while !done_worker.load(Ordering::Relaxed) || locks < 2000 {
                let mut desc = DDSURFACEDESC {
                    dwSize: size_of::<DDSURFACEDESC>() as u32,
                    ..Default::default()
                };
                // SAFETY: Valid COM Lock/read/Unlock sequence on a 1024x768x16 surface.
                unsafe {
                    s.Lock(
                        std::ptr::null_mut(),
                        &mut desc,
                        DDLOCK_WAIT as u32,
                        HANDLE::default(),
                    )
                    .expect("cross-thread DDLOCK_WAIT must never fail with DDERR_SURFACEBUSY");
                    assert!(!desc.lpSurface.is_null());
                    let pitch = desc.Anonymous1.lPitch as usize;
                    let base = desc.lpSurface.cast::<u8>();
                    for row in 0..28usize {
                        let src_row = base.add((353 + row) * pitch + 82 * 2).cast::<u16>();
                        std::ptr::copy_nonoverlapping(
                            src_row,
                            cursor_buf.as_mut_ptr().add(row * 37),
                            37,
                        );
                    }
                    s.Unlock(desc.lpSurface).unwrap();
                }
                locks += 1;
                if locks >= 4000 {
                    break;
                }
            }
            locks
        });

        let primary_slot = display.primary_slot().unwrap();
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 256,
            bottom: 256,
        };
        start.wait();
        for i in 0..1000u32 {
            // SAFETY: Valid COM Blt call with live `rect` and `back` surface.
            unsafe {
                primary
                    .Blt(
                        &mut rect,
                        &back,
                        &mut rect,
                        DDBLT_WAIT as u32,
                        std::ptr::null_mut(),
                    )
                    .unwrap();
            }
            if i % 4 == 0 {
                let mut desc = DDSURFACEDESC {
                    dwSize: size_of::<DDSURFACEDESC>() as u32,
                    ..Default::default()
                };
                // SAFETY: Valid COM Lock/write/Unlock sequence on a 16-bit surface.
                unsafe {
                    primary
                        .Lock(
                            std::ptr::null_mut(),
                            &mut desc,
                            DDLOCK_WAIT as u32,
                            HANDLE::default(),
                        )
                        .unwrap();
                    *desc.lpSurface.cast::<u16>() = i as u16;
                    primary.Unlock(desc.lpSurface).unwrap();
                }
            }
            // Simulate present_surface brief pixel read under SurfaceSlot lock.
            let guard = primary_slot.lock();
            if !guard.locked() {
                let _ = guard.pixel_slice()[0];
            }
        }
        done.store(true, Ordering::Relaxed);
        let worker_locks = worker.join().unwrap();
        assert!(worker_locks >= 2000);
    }
}
