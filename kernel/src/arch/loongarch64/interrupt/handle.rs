use crate::driver::serial::serial8250::send_to_default_serial8250_port;

use super::TrapFrame;

/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/kernel/traps.c#508
#[no_mangle]
unsafe extern "C" fn do_ade_(frame: *mut TrapFrame) {
    send_to_default_serial8250_port(b"la64: do_ade_()\n");
    loop {}
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/kernel/traps.c#522
#[no_mangle]
unsafe extern "C" fn do_ale_(frame: *mut TrapFrame) {
    send_to_default_serial8250_port(b"la64: do_ale_()\n");
    loop {}
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/kernel/traps.c#583
#[no_mangle]
unsafe extern "C" fn do_bce_(frame: *mut TrapFrame) {
    send_to_default_serial8250_port(b"la64: do_bce_()\n");
    loop {}
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/kernel/traps.c#672
#[no_mangle]
unsafe extern "C" fn do_bp_(frame: *mut TrapFrame) {
    send_to_default_serial8250_port(b"la64: do_bp_()\n");
    loop {}
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/kernel/traps.c#481
#[no_mangle]
unsafe extern "C" fn do_fpe_(frame: *mut TrapFrame) {
    send_to_default_serial8250_port(b"la64: do_fpe_()\n");
    loop {}
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/kernel/traps.c#904
#[no_mangle]
unsafe extern "C" fn do_fpu_(frame: *mut TrapFrame) {
    send_to_default_serial8250_port(b"la64: do_fpu_()\n");
    loop {}
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/kernel/traps.c#921
#[no_mangle]
unsafe extern "C" fn do_lsx_(frame: *mut TrapFrame) {
    send_to_default_serial8250_port(b"la64: do_lsx_()\n");
    loop {}
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/kernel/traps.c#943
#[no_mangle]
unsafe extern "C" fn do_lasx_(frame: *mut TrapFrame) {
    send_to_default_serial8250_port(b"la64: do_lasx_()\n");
    loop {}
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/kernel/traps.c#978
#[no_mangle]
unsafe extern "C" fn do_lbt_(frame: *mut TrapFrame) {
    send_to_default_serial8250_port(b"la64: do_lbt_()\n");
    loop {}
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/kernel/traps.c#809
#[no_mangle]
unsafe extern "C" fn do_ri_(frame: *mut TrapFrame) {
    send_to_default_serial8250_port(b"la64: do_ri_()\n");
    loop {}
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/kernel/traps.c#756
#[no_mangle]
unsafe extern "C" fn do_watch_(frame: *mut TrapFrame) {
    send_to_default_serial8250_port(b"la64: do_watch_()\n");
    loop {}
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/kernel/traps.c#1009
#[no_mangle]
unsafe extern "C" fn do_reserved_(frame: *mut TrapFrame) {
    send_to_default_serial8250_port(b"la64: do_reserved_()\n");
    // loop {}
}
