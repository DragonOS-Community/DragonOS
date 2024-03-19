use core::hint::spin_loop;

use system_error::SystemError;

use crate::{kdebug, kerror};

use super::TrapFrame;

type ExceptionHandler = fn(&mut TrapFrame) -> Result<(), SystemError>;

static EXCEPTION_HANDLERS: [ExceptionHandler; 16] = [
    do_trap_insn_misaligned,    // 0
    do_trap_insn_access_fault,  // 1
    do_trap_insn_illegal,       // 2
    do_trap_break,              // 3
    do_trap_load_misaligned,    // 4
    do_trap_load_access_fault,  // 5
    do_trap_store_misaligned,   // 6
    do_trap_store_access_fault, // 7
    do_trap_user_env_call,      // 8
    default_handler,            // 9
    default_handler,            // 10
    default_handler,            // 11
    do_trap_insn_page_fault,    // 12
    do_trap_load_page_fault,    // 13
    default_handler,            // 14
    do_trap_store_page_fault,   // 15
];

#[no_mangle]
unsafe extern "C" fn riscv64_do_irq(trap_frame: &mut TrapFrame) {
    if trap_frame.cause.is_interrupt() {
        riscv64_do_interrupt(trap_frame);
    } else if trap_frame.cause.is_exception() {
        riscv64_do_exception(trap_frame);
    }
}

/// 处理中断
fn riscv64_do_interrupt(_trap_frame: &mut TrapFrame) {
    kdebug!("todo: riscv64_do_irq: interrupt");
    loop {
        spin_loop();
    }
}

/// 处理异常
fn riscv64_do_exception(trap_frame: &mut TrapFrame) {
    kdebug!(
        "riscv64_do_exception: from_user: {}",
        trap_frame.from_user()
    );
    let code = trap_frame.cause.code();

    if code < EXCEPTION_HANDLERS.len() {
        let handler = EXCEPTION_HANDLERS[code];
        handler(trap_frame).ok();
    } else {
        kerror!("riscv64_do_irq: exception code out of range");
        loop {
            // kernel die
            spin_loop();
        }
    };
}

fn default_handler(_trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    kerror!("riscv64_do_irq: handler not found");
    loop {
        spin_loop();
    }
}

/// 处理指令地址不对齐异常 #0
fn do_trap_insn_misaligned(_trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    kerror!("riscv64_do_irq: do_trap_insn_misaligned");
    loop {
        spin_loop();
    }
}

/// 处理指令访问异常 #1
fn do_trap_insn_access_fault(_trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    kerror!("riscv64_do_irq: do_trap_insn_access_fault");
    loop {
        spin_loop();
    }
}

/// 处理非法指令异常 #2
fn do_trap_insn_illegal(_trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    kerror!("riscv64_do_irq: do_trap_insn_illegal");
    loop {
        spin_loop();
    }
}

/// 处理断点异常 #3
fn do_trap_break(_trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    kerror!("riscv64_do_irq: do_trap_break");
    loop {
        spin_loop();
    }
}

/// 处理加载地址不对齐异常 #4
fn do_trap_load_misaligned(_trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    kerror!("riscv64_do_irq: do_trap_load_misaligned");
    loop {
        spin_loop();
    }
}

/// 处理加载访问异常 #5
fn do_trap_load_access_fault(_trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    kerror!("riscv64_do_irq: do_trap_load_access_fault");
    loop {
        spin_loop();
    }
}

/// 处理存储地址不对齐异常 #6
fn do_trap_store_misaligned(_trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    kerror!("riscv64_do_irq: do_trap_store_misaligned");
    loop {
        spin_loop();
    }
}

/// 处理存储访问异常 #7
fn do_trap_store_access_fault(_trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    kerror!("riscv64_do_irq: do_trap_store_access_fault");
    loop {
        spin_loop();
    }
}

/// 处理环境调用异常 #8
fn do_trap_user_env_call(_trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    kerror!("riscv64_do_irq: do_trap_user_env_call");
    loop {
        spin_loop();
    }
}

// 9-11 reserved

/// 处理指令页错误异常 #12
fn do_trap_insn_page_fault(_trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    kerror!("riscv64_do_irq: do_insn_page_fault");
    loop {
        spin_loop();
    }
}

/// 处理页加载错误异常 #13
fn do_trap_load_page_fault(_trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    kerror!("riscv64_do_irq: do_trap_load_page_fault");
    loop {
        spin_loop();
    }
}

// 14 reserved

/// 处理页存储错误异常 #15
fn do_trap_store_page_fault(_trap_frame: &mut TrapFrame) -> Result<(), SystemError> {
    kerror!("riscv64_do_irq: do_trap_store_page_fault");
    loop {
        spin_loop();
    }
}
