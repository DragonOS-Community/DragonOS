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

    kdebug!("data_from_fdt: {:?}", data_from_fdt);

    // todo: 映射table，初始化runtime services
}
