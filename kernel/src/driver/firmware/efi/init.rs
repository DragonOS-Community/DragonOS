use core::{hint::spin_loop, intrinsics::unlikely, mem::size_of};

use log::{error, info, warn};
use system_error::SystemError;
use uefi_raw::table::boot::{MemoryAttribute, MemoryType};

use crate::{
    arch::MMArch,
    driver::{
        firmware::efi::{esrt::efi_esrt_init, EFIInitFlags},
        open_firmware::fdt::open_firmware_fdt_driver,
    },
    libs::align::{page_align_down, page_align_up},
    mm::{
        allocator::page_frame::PhysPageFrame, early_ioremap::EarlyIoRemap,
        memblock::mem_block_manager, MemoryManagementArch, PhysAddr, VirtAddr,
    },
};

use super::efi_manager;

#[allow(dead_code)]
#[inline(never)]
pub fn efi_init() {
    info!("Initializing efi...");
    let data_from_fdt = efi_manager()
        .get_fdt_params()
        .expect("Failed to get fdt params");

    if data_from_fdt.systable.is_none() {
        error!("Failed to get systable from fdt");
        return;
    }

    // debug!("to map memory table");

    // 映射mmap table
    if efi_manager().memmap_init_early(&data_from_fdt).is_err() {
        // 如果我们通过UEFI进行引导，
        // 那么 UEFI memory map 就是我们拥有的关于内存的唯一描述，
        // 所以如果我们无法访问它，那么继续进行下去就没有什么意义了

        error!("Failed to initialize early memory map");
        loop {
            spin_loop();
        }
    }
    // debug!("NNNN");
    // warn!("BBBB, e:{:?}", SystemError::EINVAL);

    let desc_version = efi_manager().desc_version();

    if unlikely(desc_version != 1) {
        warn!("Unexpected EFI memory map version: {}", desc_version);
    }

    let r = uefi_init(PhysAddr::new(data_from_fdt.systable.unwrap() as usize));
    if let Err(e) = r {
        error!("Failed to initialize UEFI: {:?}", e);
        efi_manager().efi_memmap_unmap();
        return;
    }

    reserve_memory_regions();
    // todo: 由于上面的`uefi_init`里面，按照UEFI的数据，初始化了内存块，
    // 但是UEFI给的数据可能不全，这里Linux会再次从设备树检测可用内存，从而填补完全相应的内存信息

    // 并且，Linux还对EFI BootService提供的Mokvar表进行了检测以及空间保留。

    // todo: 模仿Linux的行为，做好接下来的几步工作：
    // 参考： https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/firmware/efi/efi-init.c#217

    // todo: early_init_dt_check_for_usable_mem_range

    efi_find_mirror();
    efi_esrt_init();

    // 保留mmap table的内存
    let base = page_align_down(data_from_fdt.mmap_base.unwrap() as usize);
    let offset = data_from_fdt.mmap_base.unwrap() as usize - base;

    mem_block_manager()
        .reserve_block(
            PhysAddr::new(base),
            data_from_fdt.mmap_size.unwrap() as usize + offset,
        )
        .expect("Failed to reserve memory for EFI mmap table");

    // 保留内核的内存
    if let Some(info) = efi_manager().inner_read().dragonstub_load_info {
        mem_block_manager()
            .reserve_block(
                PhysAddr::new(info.paddr as usize),
                page_align_up(info.size as usize),
            )
            .expect("Failed to reserve kernel itself memory");
    }

    // todo: Initialize screen info

    info!("UEFI init done!");
}

fn efi_find_mirror() {
    let efi_guard = efi_manager().inner_read();
    let mut total_size = 0;
    let mut mirror_size = 0;
    for md in efi_guard.mmap.iter() {
        let start = PhysAddr::new(md.phys_start as usize);
        let size = (md.page_count << (MMArch::PAGE_SHIFT as u64)) as usize;

        if md.att.contains(MemoryAttribute::MORE_RELIABLE) {
            mem_block_manager().mark_mirror(start, size).unwrap();
            mirror_size += size;
        }

        total_size += size;
    }

    if mirror_size > 0 {
        info!(
            "Memory: {}M/{}M mirrored memory",
            mirror_size >> 20,
            total_size >> 20
        );
    }
}

#[inline(never)]
fn uefi_init(system_table: PhysAddr) -> Result<(), SystemError> {
    // 定义错误处理函数

    // 错误处理：取消systable的映射
    let err_unmap_systable = |st_vaddr: VirtAddr| {
        EarlyIoRemap::unmap(st_vaddr)
            .map_err(|e| {
                error!("Failed to unmap system table: {e:?}");
            })
            .ok();
    };

    // 映射system table

    let st_size = size_of::<uefi_raw::table::system::SystemTable>();

    let st_vaddr = EarlyIoRemap::map_not_aligned(system_table, st_size, true).map_err(|e| {
        warn!("Unable to map EFI system table, e:{e:?}");
        e
    })?;

    efi_manager()
        .inner
        .write()
        .init_flags
        .set(EFIInitFlags::BOOT, true);

    efi_manager()
        .inner
        .write()
        .init_flags
        .set(EFIInitFlags::EFI_64BIT, true);

    if st_vaddr.is_null() {
        return Err(SystemError::EINVAL);
    }

    // 解析system table
    let st_ptr = st_vaddr.data() as *const uefi_raw::table::system::SystemTable;
    efi_manager()
        .check_system_table_header(unsafe { &st_ptr.as_ref().unwrap().header }, 2)
        .inspect_err(|_| {
            err_unmap_systable(st_vaddr);
        })?;

    let st_ref = unsafe { st_ptr.as_ref().unwrap() };

    let runtime_service_paddr = efi_vaddr_2_paddr(st_ref.runtime_services as usize);
    let mut inner_write_guard = efi_manager().inner_write();
    inner_write_guard.runtime_paddr = Some(runtime_service_paddr);
    inner_write_guard.runtime_service_version = Some(st_ref.header.revision);

    drop(inner_write_guard);
    efi_manager().report_systable_header(
        &st_ref.header,
        efi_vaddr_2_paddr(st_ref.firmware_vendor as usize),
    );

    {
        // 映射configuration table
        let table_size = st_ref.number_of_configuration_table_entries
            * size_of::<uefi_raw::table::configuration::ConfigurationTable>();
        let config_table_vaddr = EarlyIoRemap::map_not_aligned(
            efi_vaddr_2_paddr(st_ref.configuration_table as usize),
            table_size,
            true,
        )
        .map_err(|e| {
            warn!("Unable to map EFI configuration table, e:{e:?}");
            err_unmap_systable(st_vaddr);
            e
        })?;
        let cfg_tables = unsafe {
            core::slice::from_raw_parts(
                config_table_vaddr.data()
                    as *const uefi_raw::table::configuration::ConfigurationTable,
                st_ref.number_of_configuration_table_entries,
            )
        };
        // 解析configuration table
        let r = efi_manager().parse_config_tables(cfg_tables);

        EarlyIoRemap::unmap(config_table_vaddr).expect("Failed to unmap EFI config table");
        return r;
    }
}

/// 把EFI固件提供的虚拟地址转换为物理地址。
///
/// 因为在调用SetVirtualAddressMap()之后，`EFI SystemTable` 的一些数据成员会被虚拟重映射
///
/// ## 锁
///
/// 在进入该函数前，请不要持有`efi_manager().inner`的写锁
fn efi_vaddr_2_paddr(efi_vaddr: usize) -> PhysAddr {
    let guard = efi_manager().inner_read();
    let mmap = &guard.mmap;

    let efi_vaddr: u64 = efi_vaddr as u64;
    for md in mmap.iter() {
        if !md.att.contains(MemoryAttribute::RUNTIME) {
            continue;
        }

        if md.virt_start == 0 {
            // no virtual mapping has been installed by the DragonStub
            break;
        }

        if md.virt_start <= efi_vaddr
            && ((efi_vaddr - md.virt_start) < (md.page_count << (MMArch::PAGE_SHIFT as u64)))
        {
            return PhysAddr::new((md.phys_start + (efi_vaddr - md.virt_start)) as usize);
        }
    }

    return PhysAddr::new(efi_vaddr as usize);
}

/// 根据UEFI提供的内存描述符的信息，填写内存区域信息
fn reserve_memory_regions() {
    // 忽略之前已经发现的任何内存块。因为之前发现的内存块来自平坦设备树，
    // 但是UEFI有自己的内存映射表，我们以UEFI提供的为准
    mem_block_manager()
        .remove_block(PhysAddr::new(0), PhysAddr::MAX.data())
        .expect("Failed to remove all memblocks!");

    let inner_guard = efi_manager().inner.read_irqsave();
    for md in inner_guard.mmap.iter() {
        let page_count = (PhysPageFrame::new(PhysAddr::new(page_align_up(
            (md.phys_start + (md.page_count << (MMArch::PAGE_SHIFT as u64))) as usize,
        )))
        .ppn()
            - PhysPageFrame::new(PhysAddr::new(page_align_down(md.phys_start as usize))).ppn())
            as u64;
        let phys_start = page_align_down(md.phys_start as usize);
        let size = (page_count << (MMArch::PAGE_SHIFT as u64)) as usize;

        // debug!("Reserve memory region: {:#x}-{:#x}({:#x}), is_memory: {}, is_usable_memory:{}, type: {:?}, att: {:?}", phys_start, phys_start + size, page_count, md.is_memory(), md.is_usable_memory(), md.ty, md.att);
        if md.is_memory() {
            open_firmware_fdt_driver().early_init_dt_add_memory(phys_start as u64, size as u64);
            if !md.is_usable_memory() {
                // debug!(
                //     "Marking non-usable memory as nomap: {:#x}-{:#x}",
                //     phys_start,
                //     phys_start + size
                // );
                mem_block_manager()
                    .mark_nomap(PhysAddr::new(phys_start), size)
                    .unwrap();
            }

            //  keep ACPI reclaim memory intact for kexec etc.
            if md.ty == MemoryType::ACPI_RECLAIM {
                mem_block_manager()
                    .reserve_block(PhysAddr::new(phys_start), size)
                    .unwrap();
            }
        }
    }
}
