use core::ffi::c_void;

use alloc::string::String;

use crate::{
    include::bindings::bindings::set_system_trap_gate,
    syscall::{Syscall, SystemError, SYS_FORK, SYS_RT_SIGRETURN, SYS_VFORK},
};

use super::{interrupt::TrapFrame, mm::barrier::mfence};

extern "C" {
    fn syscall_int();
}

macro_rules! syscall_return {
    ($val:expr, $regs:expr) => {{
        let ret = $val;
        $regs.rax = ret as u64;
        return;
    }};
}

#[no_mangle]
pub extern "C" fn syscall_handler(frame: &mut TrapFrame) -> () {
    let syscall_num = frame.rax as usize;
    let args = [
        frame.r8 as usize,
        frame.r9 as usize,
        frame.r10 as usize,
        frame.r11 as usize,
        frame.r12 as usize,
        frame.r13 as usize,
        frame.r14 as usize,
        frame.r15 as usize,
    ];
    mfence();
    mfence();
    let from_user = frame.from_user();

    // 由于进程管理未完成重构，有些系统调用需要在这里临时处理，以后这里的特殊处理要删掉。
    match syscall_num {
        SYS_FORK | SYS_VFORK => {
            syscall_return!(
                Syscall::fork(frame).unwrap_or_else(|e| e.to_posix_errno() as usize),
                frame
            )
        }

        SYS_RT_SIGRETURN => {
            syscall_return!(SystemError::ENOSYS.to_posix_errno() as usize, frame);
        }
        // SYS_SCHED => {
        //     syscall_return!(sched(from_user) as u64, regs);
        // }
        _ => {}
    }
    syscall_return!(Syscall::handle(syscall_num, &args, frame) as u64, frame);
}

/// 系统调用初始化
pub fn arch_syscall_init() -> Result<(), SystemError> {
    // kinfo!("arch_syscall_init\n");
    unsafe { set_system_trap_gate(0x80, 0, syscall_int as *mut c_void) }; // 系统调用门
    return Ok(());
}

/// 执行第一个用户进程的函数（只应该被调用一次）
///
/// 当进程管理重构完成后，这个函数应该被删除。调整为别的函数。
#[no_mangle]
pub extern "C" fn rs_exec_init_process(frame: &mut TrapFrame) -> usize {
    let path = String::from("/bin/shell.elf");
    let argv = vec![String::from("/bin/shell.elf")];
    let envp = vec![String::from("PATH=/bin")];
    let r = Syscall::do_execve(path, argv, envp, frame);
    // kdebug!("rs_exec_init_process: r: {:?}\n", r);
    return r.map(|_| 0).unwrap_or_else(|e| e.to_posix_errno() as usize);
}
