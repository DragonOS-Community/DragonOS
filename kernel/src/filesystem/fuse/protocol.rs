//! FUSE protocol structures (Linux 6.6 uapi compatible subset).
//!
//! Reference: linux-6.6.21 `include/uapi/linux/fuse.h`.

use core::mem::size_of;

use system_error::SystemError;

pub const FUSE_KERNEL_VERSION: u32 = 7;
pub const FUSE_KERNEL_MINOR_VERSION: u32 = 39;

/// The read buffer is required to be at least 8k on Linux.
pub const FUSE_MIN_READ_BUFFER: usize = 8192;

pub const FUSE_ROOT_ID: u64 = 1;

pub const FUSE_INIT: u32 = 26;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseInHeader {
    pub len: u32,
    pub opcode: u32,
    pub unique: u64,
    pub nodeid: u64,
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
    pub total_extlen: u16,
    pub padding: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseOutHeader {
    pub len: u32,
    pub error: i32,
    pub unique: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseInitIn {
    pub major: u32,
    pub minor: u32,
    pub max_readahead: u32,
    pub flags: u32,
    pub flags2: u32,
    pub unused: [u32; 11],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseInitOut {
    pub major: u32,
    pub minor: u32,
    pub max_readahead: u32,
    pub flags: u32,
    pub max_background: u16,
    pub congestion_threshold: u16,
    pub max_write: u32,
    pub time_gran: u32,
    pub max_pages: u16,
    pub map_alignment: u16,
    pub flags2: u32,
    pub unused: [u32; 7],
}

pub fn fuse_pack_struct<T: Copy>(v: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts((v as *const T).cast::<u8>(), size_of::<T>()) }
}

pub fn fuse_read_struct<T: Copy>(buf: &[u8]) -> Result<T, SystemError> {
    if buf.len() < size_of::<T>() {
        return Err(SystemError::EINVAL);
    }
    // FUSE messages are packed and 64-bit aligned, but userspace may still
    // pass unaligned buffers; use unaligned reads for robustness.
    Ok(unsafe { core::ptr::read_unaligned(buf.as_ptr().cast::<T>()) })
}
