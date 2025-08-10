use core::{intrinsics::unlikely, mem::size_of};

use log::error;
use system_error::SystemError;

use crate::{
    driver::firmware::efi::EFIInitFlags,
    libs::align::page_align_down,
    mm::{early_ioremap::EarlyIoRemap, PhysAddr, VirtAddr},
};

use super::{fdt::EFIFdtParams, tables::MemoryDescriptor, EFIManager};

#[derive(Debug)]
pub struct EFIMemoryMapInfo {
    /// EFI Memory Map的物理地址
    pub(super) paddr: Option<PhysAddr>,
    /// EFI Memory Map的虚拟地址
    pub(super) vaddr: Option<VirtAddr>,
    /// EFI Memory Map的大小
    pub(super) size: usize,
    /// 映射的描述信息的数量
    pub(super) nr_map: usize,
    /// EFI Memory Map的描述信息的大小
    pub(super) desc_size: usize,
    /// EFI Memory Map的描述信息的版本
    pub(super) desc_version: usize,
    /// 当前是否在内存管理已经完成初始化后，对该结构体进行操作
    ///
    /// true: 内存管理已经完成初始化
    /// false: 内存管理还未完成初始化
    pub(super) late: bool,
}

impl EFIMemoryMapInfo {
    pub const DEFAULT: Self = EFIMemoryMapInfo {
        paddr: None,
        vaddr: None,
        size: 0,
        nr_map: 0,
        desc_size: 0,
        desc_version: 0,
        late: false,
    };

    /// 获取EFI Memory Map的虚拟的结束地址
    #[allow(dead_code)]
    pub fn map_end_vaddr(&self) -> Option<VirtAddr> {
        return self.vaddr.map(|v| v + self.size);
    }

    /// 迭代所有的内存描述符
    pub fn iter(&self) -> EFIMemoryDescIter {
        EFIMemoryDescIter::new(self)
    }
}

/// UEFI 内存描述符的迭代器
pub struct EFIMemoryDescIter<'a> {
    inner: &'a EFIMemoryMapInfo,
    offset: usize,
}

impl<'a> EFIMemoryDescIter<'a> {
    fn new(inner: &'a EFIMemoryMapInfo) -> Self {
        Self { inner, offset: 0 }
    }
}

impl Iterator for EFIMemoryDescIter<'_> {
    type Item = MemoryDescriptor;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset + size_of::<Self::Item>() > self.inner.size {
            return None;
        }

        // 如果是空指针，返回None
        if unlikely(self.inner.vaddr.unwrap_or(VirtAddr::new(0)).is_null()) {
            return None;
        }

        let vaddr = self.inner.vaddr? + self.offset;
        self.offset += size_of::<Self::Item>();
        let res = unsafe { *(vaddr.data() as *const Self::Item) };
        return Some(res);
    }
}

impl EFIManager {
    /// Map the EFI memory map data structure
    ///
    /// 进入当前函数前，不应持有efi_manager.inner的锁
    #[inline(never)]
    pub(super) fn memmap_init_early(&self, data: &EFIFdtParams) -> Result<(), SystemError> {
        return self.do_efi_memmap_init(data, true);
    }

    /// 映射 EFI memory map
    ///
    /// 该函数在内核启动过程中使用
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/firmware/efi/memmap.c?fi=efi_memmap_init_early#104
    #[inline(never)]
    fn do_efi_memmap_init(&self, data: &EFIFdtParams, early: bool) -> Result<(), SystemError> {
        let paddr = data.mmap_base.expect("mmap_base is not set");
        let paddr = PhysAddr::new(paddr as usize);

        let mut inner_guard = self.inner.write();
        if early {
            let offset = paddr.data() - page_align_down(paddr.data());
            let map_size = data.mmap_size.unwrap() as usize + offset;

            // debug!("do_efi_memmap_init: map_size={map_size:#x}");

            // 映射内存
            let mut vaddr = EarlyIoRemap::map(
                PhysAddr::new(page_align_down(paddr.data())),
                map_size,
                false,
            )
            .map(|(vaddr, _)| vaddr)?;

            vaddr += offset;

            inner_guard.mmap.vaddr = Some(vaddr);
            inner_guard.mmap.late = false;
        } else {
            inner_guard.mmap.late = true;
            unimplemented!("efi_memmap_init_late")
        }

        if inner_guard.mmap.vaddr.is_none() {
            error!("Cannot map the EFI memory map!");
            return Err(SystemError::ENOMEM);
        }

        inner_guard.mmap.paddr = Some(paddr);
        inner_guard.mmap.size = data.mmap_size.unwrap() as usize;
        inner_guard.mmap.nr_map =
            data.mmap_size.unwrap() as usize / data.mmap_desc_size.unwrap() as usize;
        inner_guard.mmap.desc_size = data.mmap_desc_size.unwrap() as usize;
        inner_guard.mmap.desc_version = data.mmap_desc_version.unwrap() as usize;

        inner_guard.init_flags.set(EFIInitFlags::MEMMAP, true);

        return Ok(());
    }

    /// 清除EFI Memory Table在内存中的映射
    pub fn efi_memmap_unmap(&self) {
        let mut inner_guard = self.inner.write_irqsave();

        // 没有启用memmap
        if !inner_guard.init_flags.contains(EFIInitFlags::MEMMAP) {
            return;
        }

        if !inner_guard.mmap.late {
            EarlyIoRemap::unmap(inner_guard.mmap.vaddr.take().unwrap()).unwrap();
        } else {
            unimplemented!("efi_memmap_unmap");
        }
        inner_guard.init_flags.set(EFIInitFlags::MEMMAP, false);
    }
}
