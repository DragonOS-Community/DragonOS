use core::{
    fmt::{Debug, Error, Formatter},
    marker::PhantomData,
};

use crate::mm::{page::PageFlags, MemoryManagementArch}
;

bitflags::bitflags! {
    pub struct PteFlags: u64 {
        const PRESENT = 1 << 0;
        const READ_WRITE = 1 << 1;
        const USER_SUPERVISOR = 1 << 2;
        const PAGE_WRITE_THROUGH = 1 << 3;
        const PAGE_CACHE_DISABLE = 1 << 4;
        const ACCESSED = 1 << 5;
        const DIRTY = 1 << 6;
        const PAGE_SIZE = 1 << 7;
        const GLOBAL = 1 << 8;
        const EXECUTE_DISABLE = 1 << 63;
    }
}

// 页表项
#[repr(C, align(8))]
#[derive(Copy, Clone)]
pub struct EptPageEntry<Arch> {
    data: u64,
    phantom: PhantomData<Arch>,
}

impl<Arch> Debug for EptPageEntry<Arch> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), Error> {
        f.write_fmt(format_args!("EptPageEntry({:#x})", self.data))
    }
}

impl<Arch: MemoryManagementArch> EptPageEntry<Arch> {
    #[inline(always)]
    pub fn new(paddr: u64, flags: PageFlags<Arch>) -> Self {
        Self {
            data: paddr | flags.data() as u64,
            phantom: PhantomData,
        }
    }
    #[inline(always)]
    pub fn from_u64(data: u64) -> Self {
        Self {
            data,
            phantom: PhantomData,
        }
    }

    #[inline(always)]
    pub fn data(&self) -> u64 {
        self.data
    }

    /// 获取当前页表项指向的物理地址
    ///
    /// ## 返回值
    ///
    /// - Ok(PhysAddr) 如果当前页面存在于物理内存中, 返回物理地址
    /// - Err(PhysAddr) 如果当前页表项不存在, 返回物理地址
    #[inline(always)]
    pub fn address(&self) -> Result<u64, u64> {
        let paddr: u64 = {
            #[cfg(target_arch = "x86_64")]
            {
                self.data & Arch::PAGE_ADDRESS_MASK as u64
            }

            #[cfg(target_arch = "riscv64")]
            {
                let ppn = ((self.data & (!((1 << 10) - 1))) >> 10) & ((1 << 54) - 1);
                super::allocator::page_frame::PhysPageFrame::from_ppn(ppn).phys_address()
            }
        };

        if self.present() {
            Ok(paddr)
        } else {
            Err(paddr)
        }
    }

    #[inline(always)]
    pub fn flags(&self) -> PageFlags<Arch> {
        //这里不用担心是不是32bits的，因为PageFlags的data不超过usize
        unsafe { PageFlags::from_data(self.data as usize & Arch::ENTRY_FLAGS_MASK) }
    }

    #[inline(always)]
    pub fn set_flags(&mut self, flags: PageFlags<Arch>) {
        self.data = (self.data & !(Arch::ENTRY_FLAGS_MASK as u64)) | flags.data() as u64;
    }

    #[inline(always)]
    pub fn present(&self) -> bool {
        return self.data & Arch::ENTRY_FLAG_PRESENT as u64 != 0;
    }
}
