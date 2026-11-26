//! Fixed-layout frame protocol shared by the 32-bit `ddraw.dll` and the 64-bit
//! `ddraw-host.exe`. The layout is deliberately free of pointers, `usize` and
//! 64-bit fields so it is identical on i686 and x86_64 Windows.
#![no_std]

use core::mem::size_of;
use core::sync::atomic::AtomicU32;

/// "CDDH" in little endian.
pub const MAGIC: u32 = 0x4344_4448;
pub const VERSION: u32 = 1;

/// Number of rotating frame slots. The writer rotates, the reader takes the
/// newest slot; a lagging reader can therefore never observe a torn header.
pub const SLOT_COUNT: usize = 3;

/// Highest logical frame size accepted by both sides.
pub const MAX_WIDTH: u32 = 2048;
pub const MAX_HEIGHT: u32 = 1536;
pub const MAX_BYTES_PER_PIXEL: u32 = 4;

pub const LUT_ENTRIES: usize = 256;

/// `Header::flags` bits.
pub const FLAG_HOST_READY: u32 = 1 << 0;
pub const FLAG_SHUTDOWN: u32 = 1 << 1;

/// Raw primary-surface formats. The values are also the `format` uniform used
/// by the WGSL presenter, so they must not be renumbered.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameFormat {
    Indexed = 0,
    Rgb555 = 1,
    Rgb565 = 2,
    Xrgb = 3,
}

impl FrameFormat {
    pub const fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Indexed => 1,
            Self::Rgb555 | Self::Rgb565 => 2,
            Self::Xrgb => 4,
        }
    }

    pub const fn from_u32(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Indexed),
            1 => Some(Self::Rgb555),
            2 => Some(Self::Rgb565),
            3 => Some(Self::Xrgb),
            _ => None,
        }
    }
}

pub const fn slot_bytes() -> u32 {
    MAX_WIDTH * MAX_HEIGHT * MAX_BYTES_PER_PIXEL
}

pub const fn slot_offset(index: usize) -> usize {
    size_of::<Header>() + index * SLOT_BYTES as usize
}

pub const SLOT_BYTES: u32 = slot_bytes();
pub const TOTAL_BYTES: usize = size_of::<Header>() + SLOT_COUNT * SLOT_BYTES as usize;

/// Shared frame ring header.
///
/// Writers fill the non-atomic fields for a slot, copy the pixels, then publish
/// with `seq.store(seq + 1, Release)`. Readers load `seq` with `Acquire` and
/// validate the non-atomic fields against it.
#[repr(C)]
pub struct Header {
    pub magic: u32,
    pub version: u32,
    pub seq: AtomicU32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    /// `FrameFormat` value.
    pub format: u32,
    pub lut_gen: u32,
    pub flags: AtomicU32,
    /// Win32 `HWND` value; HWND is a 32-bit user handle even in 64-bit processes.
    pub game_hwnd: u32,
    pub game_pid: u32,
    pub slot_bytes: u32,
    pub reserved: u32,
    pub lut: [[u8; 4]; LUT_ENTRIES],
}

/// Size of `Header`; equal for i686 and x86_64 because every field is 32-bit
/// or a byte array and the struct is `repr(C)`.
pub const HEADER_SIZE: usize = size_of::<Header>();

const _: () = assert!(HEADER_SIZE % 4 == 0);
const _: () = assert!(SLOT_BYTES as usize <= TOTAL_BYTES);
