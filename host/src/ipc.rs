//! Shared-memory and named-event access for the 64-bit host.
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Memory::{
    FILE_MAP_ALL_ACCESS, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile,
    MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows::Win32::System::Threading::{
    EVENT_MODIFY_STATE, OpenEventW, SYNCHRONIZATION_ACCESS_RIGHTS,
};
use windows::core::PCWSTR;

use ddraw_protocol as protocol;
use protocol::Header;

/// `SYNCHRONIZE | EVENT_MODIFY_STATE`; the host both waits on and signals events.
const EVENT_ACCESS: u32 = 0x0010_0000 | EVENT_MODIFY_STATE.0;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

pub struct FrameSource {
    mapping: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    header: *mut Header,
}

// SAFETY: FrameSource owns a mapped view and is only used from the host's
// single presentation thread. The mapping is valid for the lifetime of the
// struct and all accesses go through the raw view pointer.
unsafe impl Send for FrameSource {}

impl FrameSource {
    pub fn open(base: &str) -> Result<Self, String> {
        let name = wide(&format!("{base}_shm"));
        // SAFETY: `name` is a NUL-terminated UTF-16 buffer valid for the call.
        let mapping = unsafe {
            OpenFileMappingW(
                FILE_MAP_ALL_ACCESS.0,
                false,
                PCWSTR(name.as_ptr()),
            )
        }
        .map_err(|err| format!("OpenFileMappingW({}): {err}", base))?;

        // SAFETY: The mapping is at least TOTAL_BYTES long by protocol contract.
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
            // SAFETY: `mapping` was successfully opened and is owned here.
            unsafe {
                let _ = CloseHandle(mapping);
            }
            return Err("MapViewOfFile returned null".to_string());
        }

        let header = view.Value.cast::<Header>();
        // SAFETY: The producer created this mapping with the same Header layout
        // and published magic/version before signalling readiness.
        let header_ref = unsafe { &*header };
        if header_ref.magic != protocol::MAGIC
            || header_ref.version != protocol::VERSION
            || header_ref.slot_bytes != protocol::SLOT_BYTES
        {
            // SAFETY: The view and mapping are owned here.
            unsafe {
                let _ = UnmapViewOfFile(view);
                let _ = CloseHandle(mapping);
            }
            return Err("shared frame header mismatch".to_string());
        }

        Ok(Self {
            mapping,
            view,
            header,
        })
    }

    pub fn header(&self) -> &Header {
        // SAFETY: `header` points into the live mapping and is only accessed
        // while the mapping is owned by this struct.
        unsafe { &*self.header }
    }

    pub fn slot(&self, index: usize) -> *const u8 {
        debug_assert!(index < protocol::SLOT_COUNT);
        // SAFETY: `slot_offset` stays inside TOTAL_BYTES for index < SLOT_COUNT.
        unsafe { self.view.Value.cast::<u8>().add(protocol::slot_offset(index)) }
    }
}

impl Drop for FrameSource {
    fn drop(&mut self) {
        // SAFETY: `view` and `mapping` were acquired by this struct.
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.mapping);
        }
    }
}

pub fn open_event(base: &str, suffix: &str) -> Result<HANDLE, String> {
    let name = wide(&format!("{base}{suffix}"));
    // SAFETY: `name` is a NUL-terminated UTF-16 buffer valid for the call.
    unsafe {
        OpenEventW(
            SYNCHRONIZATION_ACCESS_RIGHTS(EVENT_ACCESS),
            false,
            PCWSTR(name.as_ptr()),
        )
    }
    .map_err(|err| format!("OpenEventW({base}{suffix}): {err}"))
}
