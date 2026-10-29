//! macOS-specific implementations

use crate::{JumpEntry, code_manipulate::CodeManipulator};

// See https://developer.apple.com/library/archive/documentation/DeveloperTools/Reference/Assembler/040-Assembler_Directives/asm_directives.html#//apple_ref/doc/uid/TP30000823-CJBIFBJG
/// Name and attribute of section storing jump entries
#[doc(hidden)]
#[macro_export]
macro_rules! os_static_key_sec_name_attr {
    () => {
        "__DATA,__static_keys,regular,no_dead_strip"
    };
}

// See https://stackoverflow.com/q/17669593/10005095 and https://github.com/apple-opensource-mirror/ld64/blob/master/unit-tests/test-cases/section-labels/main.c
unsafe extern "Rust" {
    /// Address of this static is the start address of __static_keys section
    #[link_name = "\x01section$start$__DATA$__static_keys"]
    pub static mut JUMP_ENTRY_START: JumpEntry;
    /// Address of this static is the end address of __static_keys section (excluded)
    #[link_name = "\x01section$end$__DATA$__static_keys"]
    pub static mut JUMP_ENTRY_STOP: JumpEntry;
}

unsafe extern "C" {
    // libkern/OSCacheControl.h
    // void	sys_dcache_flush( void *start, size_t len) __OSX_AVAILABLE_STARTING(__MAC_10_5, __IPHONE_2_0);
    fn sys_dcache_flush(start: *mut core::ffi::c_void, len: usize);

    // libkern/OSCacheControl.h
    // void	sys_icache_invalidate( void *start, size_t len) __OSX_AVAILABLE_STARTING(__MAC_10_5, __IPHONE_2_0);
    fn sys_icache_invalidate(start: *mut core::ffi::c_void, len: usize);
}

/// Arch-specific [`CodeManipulator`] using `mach_vm_remap` to remap the code page
/// to a writable page, and remap back to bypass the W xor X rule.
pub struct ArchCodeManipulator;

// From mach/vm_statistics.h
/// Return address of target data, rather than base of page
const VM_FLAGS_RETURN_DATA_ADDR: i32 = 0x00100000;

impl CodeManipulator for ArchCodeManipulator {
    // See https://stackoverflow.com/a/76552040/10005095
    unsafe fn write_code<const L: usize>(addr: *mut core::ffi::c_void, data: &[u8; L]) {
        let mut remap_addr = 0;
        let mut cur_prot = 0;
        let mut max_prot = 0;
        let self_task = unsafe { mach2::traps::mach_task_self() };
        let length = L as u64;

        // 1. Remap the page somewhere else
        let ret = unsafe {
            mach2::vm::mach_vm_remap(
                self_task,
                &mut remap_addr,
                length,
                0,
                mach2::vm_statistics::VM_FLAGS_ANYWHERE | VM_FLAGS_RETURN_DATA_ADDR,
                self_task,
                addr as u64,
                0,
                &mut cur_prot,
                &mut max_prot,
                mach2::vm_inherit::VM_INHERIT_NONE,
            )
        };
        if ret != mach2::kern_return::KERN_SUCCESS {
            panic!("mach_vm_remap to new failed");
        }

        // 2. Reprotect the page to rw- (needs VM_PROT_COPY because the max protection is currently r-x)
        let ret = unsafe {
            mach2::vm::mach_vm_protect(
                self_task,
                remap_addr,
                length,
                0,
                mach2::vm_prot::VM_PROT_READ
                    | mach2::vm_prot::VM_PROT_WRITE
                    | mach2::vm_prot::VM_PROT_COPY,
            )
        };
        if ret != mach2::kern_return::KERN_SUCCESS {
            panic!("mach_vm_protect to write failed");
        }

        // 3. Write the changes
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), remap_addr as *mut _, L);
        }

        // 4. Flush the data cache
        unsafe {
            sys_dcache_flush(addr, L);
        }

        // 5. Reprotect the page to r-x
        let ret = unsafe {
            mach2::vm::mach_vm_protect(
                self_task,
                remap_addr,
                length,
                0,
                mach2::vm_prot::VM_PROT_READ | mach2::vm_prot::VM_PROT_EXECUTE,
            )
        };
        if ret != mach2::kern_return::KERN_SUCCESS {
            panic!("mach_vm_protect to execute failed");
        }

        // 6. Invalidate the instruction cache
        unsafe {
            sys_icache_invalidate(addr, L);
        }

        // 7. Remap the page back over the original
        let mut origin_addr = addr as u64;
        let ret = unsafe {
            mach2::vm::mach_vm_remap(
                self_task,
                &mut origin_addr,
                length,
                0,
                mach2::vm_statistics::VM_FLAGS_OVERWRITE | VM_FLAGS_RETURN_DATA_ADDR,
                self_task,
                remap_addr,
                0,
                &mut cur_prot,
                &mut max_prot,
                mach2::vm_inherit::VM_INHERIT_NONE,
            )
        };
        if ret != mach2::kern_return::KERN_SUCCESS {
            panic!("mach_vm_remap to origin failed");
        }
    }
}
