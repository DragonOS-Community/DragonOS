use super::acpi_manager;

#[no_mangle]
unsafe extern "C" fn rs_acpi_init(rsdp_vaddr1: u64, rsdp_vaddr2: u64) {
    acpi_manager()
        .init(rsdp_vaddr1, rsdp_vaddr2)
        .expect("rs_acpi_init(): failed to init acpi");
}
