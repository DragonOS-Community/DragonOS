use core::ffi::c_void;

use crate::{
    include::bindings::bindings::{
        pt_regs, set_system_trap_gate, verify_area, CLONE_FS, CLONE_SIGNAL, CLONE_VM, PAGE_4K_SIZE,
    },
    ipc::signal::sys_rt_sigreturn,
    kinfo,
    sched::core::do_sched,
    syscall::{Syscall, SystemError, SYS_EXECVE, SYS_FORK, SYS_RT_SIGRETURN, SYS_SCHED, SYS_VFORK},
};

use super::{
    asm::{current::current_pcb, ptrace::user_mode},
    context::switch_process,
    interrupt::{cli, sti},
    mm::barrier::mfence,
};

extern "C" {
    fn do_fork(regs: *mut pt_regs, clone_flags: u64, stack_start: u64, stack_size: u64) -> u64;
    fn c_sys_execve(
        path: *const u8,
        argv: *const *const u8,
        envp: *const *const u8,
        regs: &mut pt_regs,
    ) -> u64;

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
pub extern "C" fn syscall_handler(regs: &mut pt_regs) -> () {
    let syscall_num = regs.rax as usize;
    let args = [
        regs.r8 as usize,
        regs.r9 as usize,
        regs.r10 as usize,
        regs.r11 as usize,
        regs.r12 as usize,
        regs.r13 as usize,
        regs.r14 as usize,
        regs.r15 as usize,
    ];
    mfence();
    mfence();
    let from_user = user_mode(regs);

    // 由于进程管理未完成重构，有些系统调用需要在这里临时处理，以后这里的特殊处理要删掉。
    match syscall_num {
        SYS_FORK => unsafe {
            syscall_return!(do_fork(regs, 0, regs.rsp, 0), regs);
        },
        SYS_VFORK => unsafe {
            syscall_return!(
                do_fork(
                    regs,
                    (CLONE_VM | CLONE_FS | CLONE_SIGNAL) as u64,
                    regs.rsp,
                    0,
                ),
                regs
            );
        },
        SYS_EXECVE => {
            let path_ptr = args[0];
            let argv_ptr = args[1];
            let env_ptr = args[2];

            // 权限校验
            if from_user
                && (unsafe { !verify_area(path_ptr as u64, PAGE_4K_SIZE as u64) }
                    || unsafe { !verify_area(argv_ptr as u64, PAGE_4K_SIZE as u64) })
                || unsafe { !verify_area(env_ptr as u64, PAGE_4K_SIZE as u64) }
            {
                syscall_return!(SystemError::EFAULT.to_posix_errno() as u64, regs);
            } else {
                syscall_return!(
                    unsafe {
                        c_sys_execve(
                            path_ptr as *const u8,
                            argv_ptr as *const *const u8,
                            env_ptr as *const *const u8,
                            regs,
                        )
                    },
                    regs
                );
            }
        }

        SYS_RT_SIGRETURN => {
            syscall_return!(sys_rt_sigreturn(regs), regs);
        }
        // SYS_SCHED => {
        //     syscall_return!(sched(from_user) as u64, regs);
        // }
        _ => {}
    }
    syscall_return!(Syscall::handle(syscall_num, &args, from_user) as u64, regs);
}

/// 系统调用初始化
pub fn arch_syscall_init() -> Result<(), SystemError> {
    kinfo!("arch_syscall_init\n");
    unsafe { set_system_trap_gate(0x80, 0, syscall_int as *mut c_void) }; // 系统调用门
    return Ok(());
}

#[inline(always)]
pub fn sched(from_user: bool) -> i64 {
    cli();
    // 进行权限校验，拒绝用户态发起调度
    if from_user {
        return SystemError::EPERM.to_posix_errno() as i64;
    }
    // 根据调度结果统一进行切换
    let pcb = do_sched();

    if pcb.is_some() {
        switch_process(current_pcb(), pcb.unwrap());
    }
    sti();
    return 0;
}
