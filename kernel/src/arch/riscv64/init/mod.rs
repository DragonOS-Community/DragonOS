use core::intrinsics::unreachable;

use crate::{init::init_before_mem_init, kdebug};

#[no_mangle]
unsafe extern "C" fn kernel_main(hartid: usize, fdt_addr: usize) -> ! {
    crate::arch::driver::sbi::legacy::console_putchar(b'K' as u8);
    init_before_mem_init();
    crate::arch::driver::sbi::legacy::console_putchar(b'A' as u8);
    kdebug!("Hello, world!");
    loop {}
    unreachable()
}
