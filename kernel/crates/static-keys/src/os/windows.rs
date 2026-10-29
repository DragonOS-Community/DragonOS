//! Windows-specific implementations

use windows::Win32::System::{
    Diagnostics::Debug::FlushInstructionCache,
    Memory::{PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS, VirtualProtect},
    SystemInformation::{GetSystemInfo, SYSTEM_INFO},
    Threading::GetCurrentProcess,
};

use crate::{JumpEntry, code_manipulate::CodeManipulator};

// Bugs here, DO NOT USE. See https://github.com/rust-lang/rust/issues/128177
// See https://sourceware.org/binutils/docs/as/Section.html
/// Name and attribute of section storing jump entries
#[doc(hidden)]
#[macro_export]
macro_rules! os_static_key_sec_name_attr {
    () => {
        ".stks$b"
    };
}

// See https://stackoverflow.com/a/14783759 and https://devblogs.microsoft.com/oldnewthing/20181107-00/?p=100155
/// Address of this static is the start address of .stks section
#[unsafe(link_section = ".stks$a")]
pub static mut JUMP_ENTRY_START: JumpEntry = JumpEntry::dummy();
/// Address of this static is the end address of .stks section
#[unsafe(link_section = ".stks$c")]
pub static mut JUMP_ENTRY_STOP: JumpEntry = JumpEntry::dummy();

/// Arch-specific [`CodeManipulator`] using `VirtualProtect`.
pub struct ArchCodeManipulator;

impl CodeManipulator for ArchCodeManipulator {
    unsafe fn write_code<const L: usize>(addr: *mut core::ffi::c_void, data: &[u8; L]) {
        // TODO: page_size can be initialized once
        let mut system_info = SYSTEM_INFO::default();
        unsafe {
            GetSystemInfo(&mut system_info);
        }
        let page_size = system_info.dwPageSize as usize;
        let aligned_addr_val = (addr as usize) / page_size * page_size;
        let aligned_addr = aligned_addr_val as *mut core::ffi::c_void;
        let aligned_length = if (addr as usize) + L - aligned_addr_val > page_size {
            page_size * 2
        } else {
            page_size
        };
        let mut origin_protect = PAGE_PROTECTION_FLAGS::default();
        let res = unsafe {
            VirtualProtect(
                aligned_addr,
                aligned_length,
                PAGE_EXECUTE_READWRITE,
                &mut origin_protect,
            )
        };
        if res.is_err() {
            panic!("Unable to make code region writable");
        }
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), addr.cast(), L);
        }
        let mut old_protect = PAGE_PROTECTION_FLAGS::default();
        let res = unsafe {
            VirtualProtect(
                aligned_addr,
                aligned_length,
                origin_protect,
                &mut old_protect,
            )
        };
        if res.is_err() {
            panic!("Unable to restore code region to non-writable");
        }
        let res = unsafe { FlushInstructionCache(GetCurrentProcess(), Some(addr), L) };
        if res.is_err() {
            panic!("Failed to flush instruction cache");
        }
    }
}
