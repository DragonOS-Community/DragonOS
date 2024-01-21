use system_error::SystemError;

use crate::{
    driver::firmware::efi::EFIInitFlags,
    libs::align::page_align_down,
    mm::{early_ioremap::EarlyIoRemap, PhysAddr, VirtAddr},
};

use super::{fdt::EFIFdtParams, EFIManager};

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
}

impl EFIMemoryMapInfo {
    pub const DEFAULT: Self = EFIMemoryMapInfo {
        paddr: None,
        vaddr: None,
        size: 0,
        nr_map: 0,
        desc_size: 0,
        desc_version: 0,
    };

    /// 获取EFI Memory Map的虚拟的结束地址
    #[allow(dead_code)]
    pub fn map_end_vaddr(&self) -> Option<VirtAddr> {
        return self.vaddr.map(|v| v + self.size);
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
        kdebug!("do_efi_memmap_init: paddr={paddr:?}");
        let mut inner_guard = self.inner.write();
        if early {
            let offset = paddr.data() - page_align_down(paddr.data());
            let map_size = data.mmap_size.unwrap() as usize + offset;

            kdebug!("do_efi_memmap_init: map_size={map_size:#x}");
            // 映射内存
            let mut vaddr = EarlyIoRemap::map(
                PhysAddr::new(page_align_down(paddr.data())),
                map_size,
                false,
            )
            .map(|(vaddr, _)| vaddr)?;

            vaddr += offset;

            inner_guard.mmap.vaddr = Some(vaddr);
        } else {
            unimplemented!("efi_memmap_init_late")
        }

        if inner_guard.mmap.vaddr.is_none() {
            kerror!("Cannot map the EFI memory map!");
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
}
