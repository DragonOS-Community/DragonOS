use log::{error, trace, warn};
use system_error::SystemError;

use super::{
    entry::{set_intr_gate, set_system_trap_gate},
    TrapFrame,
};
use crate::exception::debug::DebugException;
use crate::exception::ebreak::EBreak;
use crate::{
    arch::{CurrentIrqArch, MMArch},
    exception::InterruptArch,
    mm::VirtAddr,
    process::ProcessManager,
    smp::core::smp_get_processor_id,
};

extern "C" {
    fn trap_divide_error();
    fn trap_debug();
    fn trap_nmi();
    fn trap_int3();
    fn trap_overflow();
    fn trap_bounds();
    fn trap_undefined_opcode();
    fn trap_dev_not_avaliable();
    fn trap_double_fault();
    fn trap_coprocessor_segment_overrun();
    fn trap_invalid_TSS();
    fn trap_segment_not_exists();
    fn trap_stack_segment_fault();
    fn trap_general_protection();
    fn trap_page_fault();
    fn trap_x87_FPU_error();
    fn trap_alignment_check();
    fn trap_machine_check();
    fn trap_SIMD_exception();
    fn trap_virtualization_exception();
}

bitflags! {
    pub struct TrapNr: u64 {
        const X86_TRAP_DE = 0;
        const X86_TRAP_DB = 1;
        const X86_TRAP_NMI = 2;
        const X86_TRAP_BP = 3;
        const X86_TRAP_OF = 4;
        const X86_TRAP_BR = 5;
        const X86_TRAP_UD = 6;
        const X86_TRAP_NM = 7;
        const X86_TRAP_DF = 8;
        const X86_TRAP_OLD_MF = 9;
        const X86_TRAP_TS = 10;
        const X86_TRAP_NP = 11;
        const X86_TRAP_SS = 12;
        const X86_TRAP_GP = 13;
        const X86_TRAP_PF = 14;
        const X86_TRAP_SPURIOUS = 15;
        const X86_TRAP_MF = 16;
        const X86_TRAP_AC = 17;
        const X86_TRAP_MC = 18;
        const X86_TRAP_XF = 19;
        const X86_TRAP_VE = 20;
        const X86_TRAP_CP = 21;
        const X86_TRAP_VC = 29;
        const X86_TRAP_IRET = 32;
    }

        pub struct X86PfErrorCode : u32{
        const X86_PF_PROT = 1 << 0;
        const X86_PF_WRITE = 1 << 1;
        const X86_PF_USER = 1 << 2;
        const X86_PF_RSVD = 1 << 3;
        const X86_PF_INSTR = 1 << 4;
        const X86_PF_PK = 1 << 5;
        const X86_PF_SHSTK = 1 << 6;
        const X86_PF_SGX = 1 << 15;
    }
}

#[inline(never)]
pub fn arch_trap_init() -> Result<(), SystemError> {
    unsafe {
        set_intr_gate(0, 0, VirtAddr::new(trap_divide_error as usize));
        set_intr_gate(1, 0, VirtAddr::new(trap_debug as usize));
        set_intr_gate(2, 0, VirtAddr::new(trap_nmi as usize));
        set_system_trap_gate(3, 0, VirtAddr::new(trap_int3 as usize));
        set_system_trap_gate(4, 0, VirtAddr::new(trap_overflow as usize));
        set_system_trap_gate(5, 0, VirtAddr::new(trap_bounds as usize));
        set_intr_gate(6, 0, VirtAddr::new(trap_undefined_opcode as usize));
        set_intr_gate(7, 0, VirtAddr::new(trap_dev_not_avaliable as usize));
        set_intr_gate(8, 0, VirtAddr::new(trap_double_fault as usize));
        set_intr_gate(
            9,
            0,
            VirtAddr::new(trap_coprocessor_segment_overrun as usize),
        );
        set_intr_gate(10, 0, VirtAddr::new(trap_invalid_TSS as usize));
        set_intr_gate(11, 0, VirtAddr::new(trap_segment_not_exists as usize));
        set_intr_gate(12, 0, VirtAddr::new(trap_stack_segment_fault as usize));
        set_intr_gate(13, 0, VirtAddr::new(trap_general_protection as usize));
        set_intr_gate(14, 0, VirtAddr::new(trap_page_fault as usize));
        // 中断号15由Intel保留，不能使用
        set_intr_gate(16, 0, VirtAddr::new(trap_x87_FPU_error as usize));
        set_intr_gate(17, 0, VirtAddr::new(trap_alignment_check as usize));
        set_intr_gate(18, 0, VirtAddr::new(trap_machine_check as usize));
        set_intr_gate(19, 0, VirtAddr::new(trap_SIMD_exception as usize));
        set_intr_gate(20, 0, VirtAddr::new(trap_virtualization_exception as usize));
    }
    return Ok(());
}

/// 处理除法错误 0 #DE
#[no_mangle]
unsafe extern "C" fn do_divide_error(regs: &'static TrapFrame, error_code: u64) {
    error!(
        "do_divide_error(0), \tError code: {:#x},\trsp: {:#x},\trip: {:#x},\t CPU: {}, \tpid: {:?}",
        error_code,
        regs.rsp,
        regs.rip,
        smp_get_processor_id().data(),
        ProcessManager::current_pid()
    );
    panic!("Divide Error");
}

/// 处理调试异常 1 #DB
#[no_mangle]
unsafe extern "C" fn do_debug(regs: &'static mut TrapFrame, error_code: u64) {
    trace!(
        "do_debug(1), \tError code: {:#x},\trsp: {:#x},\trip: {:#x},\t CPU: {}, \tpid: {:?}",
        error_code,
        regs.rsp,
        regs.rip,
        smp_get_processor_id().data(),
        ProcessManager::current_pid()
    );
    DebugException::handle(regs).unwrap();
}

/// 处理NMI中断 2 NMI
#[no_mangle]
unsafe extern "C" fn do_nmi(regs: &'static TrapFrame, error_code: u64) {
    error!(
        "do_nmi(2), \tError code: {:#x},\trsp: {:#x},\trip: {:#x},\t CPU: {}, \tpid: {:?}",
        error_code,
        regs.rsp,
        regs.rip,
        smp_get_processor_id().data(),
        ProcessManager::current_pid()
    );
    panic!("NMI Interrupt");
}

/// 处理断点异常 3 #BP
#[no_mangle]
unsafe extern "C" fn do_int3(regs: &'static mut TrapFrame, error_code: u64) {
    trace!(
        "do_int3(3), \tError code: {:#x},\trsp: {:#x},\trip: {:#x},\t CPU: {}, \tpid: {:?}",
        error_code,
        regs.rsp,
        regs.rip,
        smp_get_processor_id().data(),
        ProcessManager::current_pid()
    );
    EBreak::handle(regs).unwrap();
}

/// 处理溢出异常 4 #OF
#[no_mangle]
unsafe extern "C" fn do_overflow(regs: &'static TrapFrame, error_code: u64) {
    error!(
        "do_overflow(4), \tError code: {:#x},\trsp: {:#x},\trip: {:#x},\t CPU: {}, \tpid: {:?}",
        error_code,
        regs.rsp,
        regs.rip,
        smp_get_processor_id().data(),
        ProcessManager::current_pid()
    );
    panic!("Overflow Exception");
}

/// 处理BOUND指令检查异常 5 #BR
#[no_mangle]
unsafe extern "C" fn do_bounds(regs: &'static TrapFrame, error_code: u64) {
    error!(
        "do_bounds(5), \tError code: {:#x},\trsp: {:#x},\trip: {:#x},\t CPU: {}, \tpid: {:?}",
        error_code,
        regs.rsp,
        regs.rip,
        smp_get_processor_id().data(),
        ProcessManager::current_pid()
    );
    panic!("Bounds Check");
}

/// 处理未定义操作码异常 6 #UD
#[no_mangle]
unsafe extern "C" fn do_undefined_opcode(regs: &'static TrapFrame, error_code: u64) {
    error!(
        "do_undefined_opcode(6), \tError code: {:#x},\trsp: {:#x},\trip: {:#x},\t CPU: {}, \tpid: {:?}",
        error_code,
        regs.rsp,
        regs.rip,
        smp_get_processor_id().data(),
        ProcessManager::current_pid()
    );
    panic!("Undefined Opcode");
}

/// 处理设备不可用异常(FPU不存在) 7 #NM
#[no_mangle]
unsafe extern "C" fn do_dev_not_avaliable(regs: &'static TrapFrame, error_code: u64) {
    error!(
        "do_dev_not_avaliable(7), \tError code: {:#x},\trsp: {:#x},\trip: {:#x},\t CPU: {}, \tpid: {:?}",
        error_code,
        regs.rsp,
        regs.rip,
        smp_get_processor_id().data(),
        ProcessManager::current_pid()
    );
    panic!("Device Not Available");
}

/// 处理双重错误 8 #DF
#[no_mangle]
unsafe extern "C" fn do_double_fault(regs: &'static TrapFrame, error_code: u64) {
    error!(
        "do_double_fault(8), \tError code: {:#x},\trsp: {:#x},\trip: {:#x},\t CPU: {}, \tpid: {:?}",
        error_code,
        regs.rsp,
        regs.rip,
        smp_get_processor_id().data(),
        ProcessManager::current_pid()
    );
    panic!("Double Fault");
}

/// 处理协处理器段越界 9 #MF
#[no_mangle]
unsafe extern "C" fn do_coprocessor_segment_overrun(regs: &'static TrapFrame, error_code: u64) {
    error!(
        "do_coprocessor_segment_overrun(9), \tError code: {:#x},\trsp: {:#x},\trip: {:#x},\t CPU: {}, \tpid: {:?}",
        error_code,
        regs.rsp,
        regs.rip,
        smp_get_processor_id().data(),
        ProcessManager::current_pid()
    );
    panic!("Coprocessor Segment Overrun");
}

/// 处理无效TSS 10 #TS
#[no_mangle]
unsafe extern "C" fn do_invalid_TSS(regs: &'static TrapFrame, error_code: u64) {
    const ERR_MSG_1: &str =
        "The exception occurred during delivery of an event external to the program.\n";
    const ERR_MSG_2: &str = "Refers to a descriptor in the IDT.\n";
    const ERR_MSG_3: &str = "Refers to a descriptor in the current LDT.\n";
    const ERR_MSG_4: &str = "Refers to a descriptor in the GDT.\n";

    let msg1: &str = if (error_code & 0x1) != 0 {
        ERR_MSG_1
    } else {
        ""
    };

    let msg2: &str = if (error_code & 0x02) != 0 {
        ERR_MSG_2
    } else if (error_code & 0x04) != 0 {
        ERR_MSG_3
    } else {
        ERR_MSG_4
    };

    error!(
        "do_invalid_TSS(10), \tError code: {:#x},\trsp: {:#x},\trip: {:#x},\t CPU: {}, \tpid: {:?}\n{}{}",
        error_code,
        regs.rsp,
        regs.rip,
        smp_get_processor_id().data(),
        ProcessManager::current_pid(),
        msg1,
        msg2
    );
    panic!("Invalid TSS");
}

/// 处理段不存在 11 #NP
#[no_mangle]
unsafe extern "C" fn do_segment_not_exists(regs: &'static TrapFrame, error_code: u64) {
    error!(
        "do_segment_not_exists(11), \tError code: {:#x},\trsp: {:#x},\trip: {:#x},\t CPU: {}, \tpid: {:?}",
        error_code,
        regs.rsp,
        regs.rip,
        smp_get_processor_id().data(),
        ProcessManager::current_pid()
    );
    panic!("Segment Not Exists");
}

/// 处理栈段错误 12 #SS
#[no_mangle]
unsafe extern "C" fn do_stack_segment_fault(regs: &'static TrapFrame, error_code: u64) {
    error!(
        "do_stack_segment_fault(12), \tError code: {:#x},\trsp: {:#x},\trip: {:#x},\t CPU: {}, \tpid: {:?}",
        error_code,
        regs.rsp,
        regs.rip,
        smp_get_processor_id().data(),
        ProcessManager::current_pid()
    );
    panic!("Stack Segment Fault");
}

/// 处理一般保护异常 13 #GP
#[no_mangle]
unsafe extern "C" fn do_general_protection(regs: &'static TrapFrame, error_code: u64) {
    const ERR_MSG_1: &str = "The exception occurred during delivery of an event external to the program, such as an interrupt or an earlier exception.";
    const ERR_MSG_2: &str = "Refers to a gate descriptor in the IDT;\n";
    const ERR_MSG_3: &str = "Refers to a descriptor in the GDT or the current LDT;\n";
    const ERR_MSG_4: &str = "Refers to a segment or gate descriptor in the LDT;\n";
    const ERR_MSG_5: &str = "Refers to a descriptor in the current GDT;\n";

    let msg1: &str = if (error_code & 0x1) != 0 {
        ERR_MSG_1
    } else {
        ""
    };

    let msg2: &str = if (error_code & 0x02) != 0 {
        ERR_MSG_2
    } else {
        ERR_MSG_3
    };

    let msg3: &str = if (error_code & 0x02) == 0 {
        if (error_code & 0x04) != 0 {
            ERR_MSG_4
        } else {
            ERR_MSG_5
        }
    } else {
        ""
    };
    error!(
        "do_general_protection(13), \tError code: {:#x},\trsp: {:#x},\trip: {:#x},\t rflags: {:#x}\t CPU: {}, \tpid: {:?}
{}{}{}
Segment Selector Index: {:#x}\n
",
        error_code,
        regs.rsp,
        regs.rip,
        regs.rflags,
        smp_get_processor_id().data(),
        ProcessManager::current_pid(),
        msg1, msg2, msg3,
        error_code & 0xfff8
    );
    panic!("General Protection");
}

/// 处理页错误 14 #PF
#[no_mangle]
unsafe extern "C" fn do_page_fault(regs: &'static TrapFrame, error_code: u64) {
    // error!(
    //     "do_page_fault(14), \tError code: {:#x},\trsp: {:#x},\trip: {:#x},\t CPU: {}, \tpid: {:?}, \nFault Address: {:#x}",
    //     error_code,
    //     regs.rsp,
    //     regs.rip,
    //     smp_get_processor_id().data(),
    //     ProcessManager::current_pid(),
    //     x86::controlregs::cr2()
    // );

    // if (error_code & 0x01) == 0 {
    //     print!("Page Not Present,\t");
    // }
    // if (error_code & 0x02) != 0 {
    //     print!("Write Access,\t");
    // } else {
    //     print!("Read Access,\t");
    // }

    // if (error_code & 0x04) != 0 {
    //     print!("Fault in user(3),\t");
    // } else {
    //     print!("Fault in supervisor(0,1,2),\t");
    // }

    // if (error_code & 0x08) != 0 {
    //     print!("Reserved bit violation cause fault,\t");
    // }

    // if (error_code & 0x10) != 0 {
    //     print!("Instruction fetch cause fault,\t");
    // }
    // print!("\n");

    // CurrentIrqArch::interrupt_enable();
    // panic!("Page Fault");
    CurrentIrqArch::interrupt_disable();
    let address = x86::controlregs::cr2();
    // log::info!(
    //     "fault address: {:#x}, error_code: {:#b}, pid: {}\n",
    //     address,
    //     error_code,
    //     ProcessManager::current_pid().data()
    // );

    let address = VirtAddr::new(address);
    let error_code = X86PfErrorCode::from_bits_truncate(error_code as u32);
    if address.check_user() {
        MMArch::do_user_addr_fault(regs, error_code, address);
    } else {
        MMArch::do_kern_addr_fault(regs, error_code, address);
    }
    CurrentIrqArch::interrupt_enable();
}

/// 处理x87 FPU错误 16 #MF
#[no_mangle]
unsafe extern "C" fn do_x87_FPU_error(regs: &'static TrapFrame, error_code: u64) {
    error!(
        "do_x87_FPU_error(16), \tError code: {:#x},\trsp: {:#x},\trip: {:#x},\t CPU: {}, \tpid: {:?}",
        error_code,
        regs.rsp,
        regs.rip,
        smp_get_processor_id().data(),
        ProcessManager::current_pid()
    );
    panic!("x87 FPU Error");
}

/// 处理对齐检查 17 #AC
#[no_mangle]
unsafe extern "C" fn do_alignment_check(regs: &'static TrapFrame, error_code: u64) {
    error!(
        "do_alignment_check(17), \tError code: {:#x},\trsp: {:#x},\trip: {:#x},\t CPU: {}, \tpid: {:?}",
        error_code,
        regs.rsp,
        regs.rip,
        smp_get_processor_id().data(),
        ProcessManager::current_pid()
    );
    panic!("Alignment Check");
}

/// 处理机器检查 18 #MC
#[no_mangle]
unsafe extern "C" fn do_machine_check(regs: &'static TrapFrame, error_code: u64) {
    error!(
        "do_machine_check(18), \tError code: {:#x},\trsp: {:#x},\trip: {:#x},\t CPU: {}, \tpid: {:?}",
        error_code,
        regs.rsp,
        regs.rip,
        smp_get_processor_id().data(),
        ProcessManager::current_pid()
    );
    panic!("Machine Check");
}

/// 处理SIMD异常 19 #XM
#[no_mangle]
unsafe extern "C" fn do_SIMD_exception(regs: &'static TrapFrame, error_code: u64) {
    error!(
        "do_SIMD_exception(19), \tError code: {:#x},\trsp: {:#x},\trip: {:#x},\t CPU: {}, \tpid: {:?}",
        error_code,
        regs.rsp,
        regs.rip,
        smp_get_processor_id().data(),
        ProcessManager::current_pid()
    );
    panic!("SIMD Exception");
}

/// 处理虚拟化异常 20 #VE
#[no_mangle]
unsafe extern "C" fn do_virtualization_exception(regs: &'static TrapFrame, error_code: u64) {
    error!(
        "do_virtualization_exception(20), \tError code: {:#x},\trsp: {:#x},\trip: {:#x},\t CPU: {}, \tpid: {:?}",
        error_code,
        regs.rsp,
        regs.rip,
        smp_get_processor_id().data(),
        ProcessManager::current_pid()
    );
    panic!("Virtualization Exception");
}

#[no_mangle]
unsafe extern "C" fn ignore_int_handler(_regs: &'static TrapFrame, _error_code: u64) {
    warn!("Unknown interrupt.");
}
