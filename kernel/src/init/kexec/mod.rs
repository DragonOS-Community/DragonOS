pub mod kexec_core;
pub mod syscall;

use crate::libs::spinlock::SpinLock;
use crate::mm::page::Page;
use alloc::rc::Rc;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ffi::c_void;

const KEXEC_SEGMENT_MAX: usize = 16;

pub static mut KEXEC_IMAGE: Option<Rc<SpinLock<Kimage>>> = None;

const IND_DESTINATION_BIT: usize = 0;
const IND_INDIRECTION_BIT: usize = 1;
const IND_DONE_BIT: usize = 2;
const IND_SOURCE_BIT: usize = 3;

const IND_DESTINATION: usize = 1 << IND_DESTINATION_BIT;
const IND_INDIRECTION: usize = 1 << IND_INDIRECTION_BIT;
const IND_DONE: usize = 1 << IND_DONE_BIT;
const IND_SOURCE: usize = 1 << IND_SOURCE_BIT;

type KimageEntry = usize;

#[derive(Clone, Copy)]
#[repr(C)]
pub union kexec_segment_buf {
    pub buf: *mut c_void,  // For user memory (user space pointer)
    pub kbuf: *mut c_void, // For kernel memory (kernel space pointer)
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct KexecSegment {
    /// This pointer can point to user memory if kexec_load() system
    /// call is used or will point to kernel memory if
    /// kexec_file_load() system call is used.
    ///
    /// Use ->buf when expecting to deal with user memory and use ->kbuf
    /// when expecting to deal with kernel memory.
    pub buffer: kexec_segment_buf,
    pub bufsz: usize,
    pub mem: usize, // unsigned long typically matches usize
    pub memsz: usize,
}

/// kimage结构体定义, 没写全, 见https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/kexec.h#321
#[repr(C)]
pub struct Kimage {
    pub head: KimageEntry,
    pub entry: *mut KimageEntry,
    pub last_entry: *mut KimageEntry,

    pub start: usize,
    pub control_code_page: Option<Arc<Page>>,
    // stack_page
    // 这里与 linux 不一样, 因为 linux 的 control_page 是一个 page *,
    // 他实际上指向两个页面, 也就是 control_page 和 stack_page, 且要求这俩页面地址连续
    // 但是 rust 这块我还没想好要不要用 Vec 去做, 因此先这么用着
    pub stack_page: Option<Arc<Page>>,

    pub nr_segments: usize,
    pub segment: [KexecSegment; KEXEC_SEGMENT_MAX],

    pub pages: Vec<Arc<Page>>,

    /*
     * This is a kimage control page, as it must not overlap with either
     * source or destination address ranges.
     */
    pub pgd: usize,
}

bitflags! {
    pub struct KexecFlags: u64 {
        const KEXEC_ON_CRASH = 0x00000001;
        const KEXEC_PRESERVE_CONTEXT = 0x00000002;
        const KEXEC_ARCH_MASK = 0xffff0000;
    }
}
