use core::{intrinsics::unlikely, mem::size_of};

use system_error::SystemError;

use crate::{
    driver::firmware::efi::EFIInitFlags,
    libs::align::page_align_down,
    mm::{early_ioremap::EarlyIoRemap, PhysAddr, VirtAddr},
};

use super::efi_manager;

#[allow(dead_code)]
#[inline(never)]
pub fn efi_init() {
    let data_from_fdt = efi_manager()
        .get_fdt_params()
        .expect("Failed to get fdt params");

    if data_from_fdt.systable.is_none() {
        kerror!("Failed to get systable from fdt");
        return;
    }

    kdebug!("to map memory table");

    // 映射mmap table
    if efi_manager().memmap_init_early(&data_from_fdt).is_err() {
        // 如果我们通过UEFI进行引导，
        // 那么 UEFI memory map 就是我们拥有的关于内存的唯一描述，
        // 所以如果我们无法访问它，那么继续进行下去就没有什么意义了

        kerror!("Failed to initialize early memory map");
        loop {}
    }
    // kdebug!("NNNN");
    // kwarn!("BBBB, e:{:?}", SystemError::EINVAL);

    let desc_version = efi_manager().desc_version();

    if unlikely(desc_version != 1) {
        kwarn!("Unexpected EFI memory map version: {}", desc_version);
    }

    // todo: 映射table，初始化runtime services

    let r = uefi_init(PhysAddr::new(data_from_fdt.systable.unwrap() as usize));

    if let Err(r) = r {
        kerror!("Failed to initialize UEFI: {:?}", r);
    }

    loop {}
}

#[inline(never)]
fn uefi_init(system_table: PhysAddr) -> Result<(), SystemError> {
    // 定义错误处理函数

    // 错误处理：取消systable的映射
    let err_unmap_systable = |st_vaddr: VirtAddr| {
        EarlyIoRemap::unmap(st_vaddr)
            .map_err(|e| {
                kerror!("Failed to unmap system table: {e:?}");
            })
            .ok();
    };

    // 映射system table

    let st_size = size_of::<uefi_raw::table::system::SystemTable>();
    kdebug!("system table: {system_table:?}, size: {st_size}");
    let st_map_phy_base = PhysAddr::new(page_align_down(system_table.data()));

    let st_map_offset = system_table.data() - st_map_phy_base.data();
    let st_map_size = st_size + st_map_offset;
    let (st_vaddr, _st_map_size) =
        EarlyIoRemap::map(st_map_phy_base, st_map_size, true).map_err(|e| {
            kwarn!("Unable to map EFI system table, e:{e:?}");
            e
        })?;

    let st_vaddr = st_vaddr + st_map_offset;

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

    kdebug!("to parse EFI system table: p: {st_vaddr:?}");

    if st_vaddr.is_null() {
        return Err(SystemError::EINVAL);
    }

    // 解析system table
    let st_ptr = st_vaddr.data() as *const uefi_raw::table::system::SystemTable;
    efi_manager()
        .check_system_table_header(unsafe { &st_ptr.as_ref().unwrap().header }, 2)
        .map_err(|e| {
            err_unmap_systable(st_vaddr);
            e
        })?;

    kdebug!("parse ok!");
    let mut inner_write_guard = efi_manager().inner.write();
    let st_ref = unsafe { st_ptr.as_ref().unwrap() };
    inner_write_guard.runtime_paddr = Some(PhysAddr::new(st_ref.runtime_services as usize));
    inner_write_guard.runtime_service_version = Some(st_ref.header.revision);

    kdebug!(
        "runtime service paddr: {:?}",
        inner_write_guard.runtime_paddr.unwrap()
    );
    kdebug!(
        "runtime service version: {}",
        inner_write_guard.runtime_service_version.unwrap()
    );

    unimplemented!("report header");
    // return Ok(());
}
