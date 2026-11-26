//! 32-bit side of the 64-bit presentation bridge.
//!
//! This module owns the shared-memory frame ring and the lifetime of
//! `ddraw-host.exe`. It deliberately does not link any GPU API: the 32-bit
//! process never creates a D3D12 device, DXGI swapchain or D3DMetal object.
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::{s, w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HWND, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile,
    MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateProcessW, GetCurrentProcessId, SetEvent, WaitForSingleObject,
    CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTUPINFOW,
};

use ddraw_protocol as protocol;

use crate::config::{self, PresenterMode, RendererBackend};
use crate::surface::{PixelFormat, SurfaceSlot};

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The host executable is expected next to `ddraw.dll`.
pub fn host_path() -> Option<PathBuf> {
    let marker = host_path as *const () as *const c_void;
    crate::win32::module_path_at(marker)
        .and_then(|dll| dll.parent().map(|dir| dir.join("ddraw-host.exe")))
        .or_else(|| {
            std::env::current_exe()
                .ok()?
                .parent()
                .map(|dir| dir.join("ddraw-host.exe"))
        })
}

pub fn host_available() -> bool {
    host_path().is_some_and(|path| path.is_file())
}

/// Detect Wine without a build-time dependency. `ntdll!wine_get_version` is
/// exported by every Wine build and absent on native Windows.
pub fn running_under_wine() -> bool {
    // SAFETY: ntdll is always loaded; GetProcAddress only queries an export.
    unsafe {
        let Ok(ntdll) = GetModuleHandleW(w!("ntdll.dll")) else {
            return false;
        };
        GetProcAddress(ntdll, s!("wine_get_version")).is_some()
    }
}

pub fn use_host_presenter() -> bool {
    match config::get().presenter {
        PresenterMode::Host => true,
        PresenterMode::InProc => false,
        PresenterMode::Auto => running_under_wine() && host_available(),
    }
}

fn backend_name() -> &'static str {
    match config::get().renderer {
        RendererBackend::Dx12 => "dx12",
        RendererBackend::Vulkan => "vulkan",
        RendererBackend::Auto => "auto",
    }
}

fn protocol_format(format: PixelFormat) -> u32 {
    format as u32
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitOutcome {
    Presented,
    Busy,
}

pub struct FrameSink {
    mapping: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    header: *mut protocol::Header,
    frame_event: HANDLE,
    presented_event: HANDLE,
    shutdown_event: HANDLE,
    process: HANDLE,
    first_submit: AtomicBool,
    test_pattern: bool,
}

// SAFETY: `FrameSink` owns OS handles and a process-wide mapping. All access is
// serialized by `Display`'s mutex; `HANDLE`/raw pointers are only opaque values
// here and are never dereferenced concurrently by two Rust threads.
unsafe impl Send for FrameSink {}

impl FrameSink {
    pub fn start(hwnd: HWND) -> Result<Self, String> {
        let Some(host) = host_path().filter(|path| path.is_file()) else {
            return Err("ddraw-host.exe is not next to ddraw.dll".to_string());
        };

        // SAFETY: GetCurrentProcessId takes no arguments and cannot fail.
        let pid = unsafe { GetCurrentProcessId() };
        let base = format!("Local\\CncDdrawHost_{pid}");
        let shm_name = wide(&format!("{base}_shm"));

        // SAFETY: The name and size are valid; None security attributes means
        // the default DACL for the current user/session.
        let mapping = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                protocol::TOTAL_BYTES as u32,
                PCWSTR(shm_name.as_ptr()),
            )
        }
        .map_err(|err| format!("CreateFileMappingW: {err}"))?;

        // SAFETY: `mapping` is a live file mapping object.
        let view = unsafe {
            MapViewOfFile(
                mapping,
                FILE_MAP_ALL_ACCESS,
                0,
                0,
                protocol::TOTAL_BYTES,
            )
        };
        if view.Value.is_null() {
            // SAFETY: Both resources were acquired in this function.
            unsafe {
                let _ = CloseHandle(mapping);
            }
            return Err("MapViewOfFile returned null".to_string());
        }

        let frame_event = create_event(&base, "_frame", false)?;
        let presented_event = create_event(&base, "_presented", false)?;
        let shutdown_event = create_event(&base, "_shutdown", true)?;

        let header = view.Value.cast::<protocol::Header>();
        // SAFETY: The mapping was just created and is zero-filled by the OS;
        // all fields are initialized below before the host can observe a frame.
        unsafe {
            let header = &mut *header;
            header.magic = protocol::MAGIC;
            header.version = protocol::VERSION;
            header.seq.store(0, Ordering::Release);
            header.width = 0;
            header.height = 0;
            header.pitch = 0;
            header.format = 0;
            header.lut_gen = 0;
            header.flags.store(0, Ordering::Release);
            header.game_hwnd = hwnd.0 as usize as u32;
            header.game_pid = pid;
            header.slot_bytes = protocol::SLOT_BYTES;
            header.reserved = 0;
            header.lut = [[0u8; 4]; protocol::LUT_ENTRIES];
        }

        let app: Vec<u16> = host
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let maintas = u8::from(config::get().maintas);
        let command = format!(
            "\"{}\" --base {} --hwnd {:x} --pid {} --backend {} --maintas {}",
            host.display(),
            base,
            hwnd.0 as usize,
            pid,
            backend_name(),
            maintas
        );
        let mut command: Vec<u16> = command
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut process_info = PROCESS_INFORMATION::default();

        // SAFETY: All pointers refer to live, NUL-terminated buffers or valid
        // stack structures for the duration of the call. No handles are inherited.
        let created = unsafe {
            CreateProcessW(
                PCWSTR(app.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NO_WINDOW,
                None,
                PCWSTR::null(),
                &startup as *const STARTUPINFOW,
                &mut process_info as *mut PROCESS_INFORMATION,
            )
        };
        if let Err(err) = created {
            // SAFETY: These resources are owned by this function and the launch failed.
            unsafe {
                let _ = SetEvent(shutdown_event);
                let _ = CloseHandle(frame_event);
                let _ = CloseHandle(presented_event);
                let _ = CloseHandle(shutdown_event);
                let _ = UnmapViewOfFile(view);
                let _ = CloseHandle(mapping);
            }
            return Err(format!("CreateProcessW({}): {err}", host.display()));
        }

        // The thread handle is not needed; the process handle is used to detect
        // host crashes and to avoid leaking an orphan.
        // SAFETY: `process_info.hThread` is a fresh, owned handle.
        unsafe {
            let _ = CloseHandle(process_info.hThread);
        }

        let test_pattern = std::env::var_os("CNC_DDRAW_TEST_PATTERN").is_some();
        if test_pattern {
            crate::compat::log_msg("CNC_DDRAW_TEST_PATTERN enabled");
        }

        Ok(Self {
            mapping,
            view,
            header,
            frame_event,
            presented_event,
            shutdown_event,
            process: process_info.hProcess,
            first_submit: AtomicBool::new(false),
            test_pattern,
        })
    }

    pub fn submit(
        &self,
        primary: &SurfaceSlot,
        on_uploaded: impl FnOnce(),
    ) -> Result<SubmitOutcome, String> {
        // SAFETY: A zero-timeout wait is a liveness probe for the host process.
        if unsafe { WaitForSingleObject(self.process, 0) } == WAIT_OBJECT_0 {
            return Err("ddraw-host.exe has exited".to_string());
        }

        let guard = primary.lock();
        if guard.locked() {
            return Ok(SubmitOutcome::Busy);
        }
        let width = guard.width;
        let height = guard.height;
        if width == 0 || height == 0 {
            return Ok(SubmitOutcome::Busy);
        }
        if width > protocol::MAX_WIDTH || height > protocol::MAX_HEIGHT {
            return Err(format!("frame {width}x{height} exceeds protocol limit"));
        }
        let format = guard.pixel_format();
        let pitch = guard.pitch();
        let min_row = width
            .checked_mul(format.bytes() as u32)
            .ok_or_else(|| "frame row size overflow".to_string())?;
        let pixels = guard.pixel_slice();
        if pitch < min_row || (pitch as usize).checked_mul(height as usize) != Some(pixels.len()) {
            return Err("incorrect primary frame buffer length".to_string());
        }
        if u64::from(pitch) * u64::from(height) > u64::from(protocol::SLOT_BYTES) {
            return Err("frame does not fit in a shared slot".to_string());
        }

        let lut = guard
            .build_lut()
            .map_err(|err| format!("building frame LUT: {err}"))?;

        if !self.first_submit.swap(true, Ordering::Relaxed) {
            let sample = &pixels[..pixels.len().min(32)];
            // SAFETY: `header` points into the live mapping owned by this sink.
            let lut_gen = unsafe { (*self.header).lut_gen };
            crate::compat::log_msg(&format!(
                "host frame: {width}x{height} pitch={pitch} format={format:?} lut_gen={lut_gen} first={sample:02x?} lut0={:?}",
                &lut[..4]
            ));
        }
        // SAFETY: `header` points into the live mapping owned by this sink.
        let seq = unsafe { (*self.header).seq.load(Ordering::Relaxed) };
        let index = ((seq as usize) + 1) % protocol::SLOT_COUNT;
        // SAFETY: `slot_offset(index)` is within the mapping for index < SLOT_COUNT.
        let destination = unsafe {
            self.view
                .Value
                .cast::<u8>()
                .add(protocol::slot_offset(index))
        };
        if self.test_pattern {
            for row in 0..height as usize {
                let offset = row * pitch as usize;
                for col in 0..pitch as usize {
                    // SAFETY: The slot is `pitch * height` bytes and both
                    // indexes are inside it.
                    unsafe {
                        *destination.add(offset + col) =
                            ((col.wrapping_mul(13) ^ row.wrapping_mul(7)) & 0xff) as u8;
                    }
                }
            }
        } else {
            for row in 0..height as usize {
                let offset = row * pitch as usize;
                // SAFETY: The producer validated pitch * height and both buffers
                // have at least that many bytes. Rows may overlap only if pitch is
                // valid; source and destination live in different mappings.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        pixels.as_ptr().add(offset),
                        destination.add(offset),
                        pitch as usize,
                    );
                }
            }
        }

        // SAFETY: This struct owns `header`, and the host only publishes frames
        // after observing the release-store to `seq`.
        unsafe {
            let header = &mut *self.header;
            header.width = width;
            header.height = height;
            header.pitch = pitch;
            header.format = protocol_format(format);
            header.slot_bytes = protocol::SLOT_BYTES;
            if header.lut != lut {
                header.lut = lut;
                header.lut_gen = header.lut_gen.wrapping_add(1);
            }
            header.seq.store(seq.wrapping_add(1), Ordering::Release);
        }
        on_uploaded();

        // SAFETY: The event handle is owned by this sink for its lifetime.
        unsafe {
            let _ = SetEvent(self.frame_event);
        }
        let wait_ms = config::get().min_frame_interval_ms().min(u32::MAX as u64) as u32;
        // SAFETY: `presented_event` is a valid auto-reset event. A timeout is
        // expected when the host is hidden or still initializing.
        unsafe {
            let _ = WaitForSingleObject(self.presented_event, wait_ms);
        }
        Ok(SubmitOutcome::Presented)
    }
}

fn create_event(base: &str, suffix: &str, manual_reset: bool) -> Result<HANDLE, String> {
    let name = wide(&format!("{base}{suffix}"));
    // SAFETY: The name is a NUL-terminated UTF-16 buffer valid for the call.
    unsafe {
        CreateEventW(
            None,
            manual_reset,
            false,
            PCWSTR(name.as_ptr()),
        )
    }
    .map_err(|err| format!("CreateEventW({base}{suffix}): {err}"))
}

impl Drop for FrameSink {
    fn drop(&mut self) {
        // SAFETY: All handles and the view are owned by this sink. Signalling
        // shutdown first lets the host exit cleanly; handles are closed even if
        // it is already gone.
        unsafe {
            let _ = SetEvent(self.shutdown_event);
            let _ = CloseHandle(self.frame_event);
            let _ = CloseHandle(self.presented_event);
            let _ = CloseHandle(self.shutdown_event);
            let _ = CloseHandle(self.process);
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.mapping);
        }
    }
}
