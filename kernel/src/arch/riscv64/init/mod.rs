use core::intrinsics::unreachable;

use crate::{init::init_before_mem_init, kinfo, mm::PhysAddr};

#[no_mangle]
unsafe extern "C" fn kernel_main(hartid: usize, fdt_paddr: usize) -> ! {
    let fdt_paddr = PhysAddr::new(fdt_paddr);
    init_before_mem_init();
    kinfo!(
        "DragonOS kernel is running on hart {}, fdt address:{:?}",
        hartid,
        fdt_paddr
    );
    loop {}
    unreachable()
}
