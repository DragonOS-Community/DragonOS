use crate::{
    arch::MMArch,
    libs::align::AlignedBox,
    mm::{MemoryManagementArch, VirtAddr},
};

use super::acpi_manager;

static mut RSDP_TMP_BOX: Option<AlignedBox<[u8; 4096], 4096>> = None;

#[no_mangle]
unsafe extern "C" fn rs_acpi_init(rsdp_vaddr: u64) {
    RSDP_TMP_BOX = Some(AlignedBox::new_zeroed().expect("rs_acpi_init(): failed to alloc"));
    let size = core::mem::size_of::<acpi::rsdp::Rsdp>();
    let tmp_data = core::slice::from_raw_parts(rsdp_vaddr as usize as *const u8, size);
    RSDP_TMP_BOX.as_mut().unwrap()[0..size].copy_from_slice(tmp_data);

    let rsdp_paddr = MMArch::virt_2_phys(VirtAddr::new(
        RSDP_TMP_BOX.as_ref().unwrap().as_ptr() as usize
    ))
    .unwrap();

    acpi_manager()
        .init(rsdp_paddr)
        .expect("rs_acpi_init(): failed to init acpi");
}
