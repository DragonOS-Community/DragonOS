//! Other OS-specific implementations

use crate::{JumpEntry, code_manipulate::CodeManipulator};
use core::ffi::c_void;

/// Name and attribute of section storing jump entries
#[doc(hidden)]
#[macro_export]
macro_rules! os_static_key_sec_name_attr {
    () => {
        "__static_keys, \"awR\""
    };
}

unsafe extern "Rust" {
    /// Address of this static is the start address of __static_keys section
    #[link_name = "__start___static_keys"]
    pub static mut JUMP_ENTRY_START: JumpEntry;
    /// Address of this static is the end address of __static_keys section (excluded)
    #[link_name = "__stop___static_keys"]
    pub static mut JUMP_ENTRY_STOP: JumpEntry;
}

/// Arch-specific [`CodeManipulator`] using [`libc`] with `mprotect`.
pub struct ArchCodeManipulator;

impl CodeManipulator for crate::os::ArchCodeManipulator {
    unsafe fn write_code<const L: usize>(addr: *mut c_void, data: &[u8; L]) {
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), addr.cast(), L) };
    }
}
