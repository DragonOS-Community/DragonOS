use core::intrinsics::unreachable;

use crate::{
    driver::tty::serial::serial8250::send_to_default_serial8250_port, init::init_before_mem_init,
    kdebug,
};

#[no_mangle]
unsafe extern "C" fn kernel_main(hartid: usize, fdt_addr: usize) -> ! {
    init_before_mem_init();
    send_to_default_serial8250_port(&b"Hello, world! RISC-V!\n"[..]);
    loop {}
    unreachable()
}
