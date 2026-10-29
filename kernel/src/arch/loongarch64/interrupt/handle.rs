use crate::{
    arch::MMArch, driver::serial::serial8250::send_to_default_serial8250_port,
    exception::extable::ExceptionTableManager, mm::MemoryManagementArch, process::ProcessManager,
};

use super::TrapFrame;

fn try_fixup_kernel_user_access(frame: *mut TrapFrame) -> bool {
    let Some(frame) = (unsafe { frame.as_mut() }) else {
        return false;
    };

    if frame.is_from_user() || ProcessManager::current_pcb().pagefault_disabled() == 0 {
        return false;
    }
    if frame.csr_badvaddr >= MMArch::PHYS_OFFSET {
        return false;
    }

    if let Some(fixup_addr) = ExceptionTableManager::search_exception_table(frame.csr_era) {
        frame.csr_era = fixup_addr;
        return true;
    }

    false
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/kernel/traps.c#508
#[no_mangle]
unsafe extern "C" fn do_ade_(frame: *mut TrapFrame) {
    if try_fixup_kernel_user_access(frame) {
        return;
    }

    send_to_default_serial8250_port(b"la64: do_ade_()\n");
    loop {}
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/loongarch/kernel/traps.c#522
#[no_mangle]
unsafe extern "C" fn do_ale_(frame: *mut TrapFrame) {
    if try_fixup_kernel_user_access(frame) {
        return;
    }

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
