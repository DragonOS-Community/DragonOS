use core::sync::atomic::{AtomicBool, Ordering};

use crate::{
    arch::syscall::nr::*,
    process::{ProcessFlags, ProcessManager},
    sched::{schedule, SchedMode},
    syscall::user_access::check_and_clone_cstr,
};

use log::{info, warn};
use system_error::SystemError;
use table::{syscall_table, syscall_table_init};

use crate::arch::interrupt::TrapFrame;

use self::user_access::UserBufferWriter;

pub mod misc;
mod sys_getrandom;
mod sys_sysinfo;
pub mod table;
pub mod user_access;
pub mod user_buffer;

// 与linux不一致的调用，在linux基础上累加
pub const SYS_PUT_STRING: usize = 100000;
pub const SYS_SBRK: usize = 100001;
/// todo: 该系统调用与Linux不一致，将来需要删除该系统调用！！！ 删的时候记得改C版本的libc
pub const SYS_CLOCK: usize = 100002;
pub const SYS_SCHED: usize = 100003;

#[derive(Debug)]
pub struct Syscall;

impl Syscall {
    /// 初始化系统调用
    #[inline(never)]
    pub fn init() -> Result<(), SystemError> {
        static INIT_FLAG: AtomicBool = AtomicBool::new(false);
        let prev = INIT_FLAG.swap(true, Ordering::SeqCst);
        if prev {
            panic!("Cannot initialize syscall more than once!");
        }
        info!("Initializing syscall...");
        let r = crate::arch::syscall::arch_syscall_init();
        info!("Syscall init successfully!");

        return r;
    }
    /// 系统调用分发器，用于分发系统调用。
    ///
    /// 与[handle]不同，这个函数会捕获系统调用处理函数的panic，返回错误码。
    #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
    pub fn catch_handle(
        syscall_num: usize,
        args: &[usize],
        frame: &mut TrapFrame,
    ) -> Result<usize, SystemError> {
        use crate::debug::panic::kernel_catch_unwind;
        let res = kernel_catch_unwind(|| Self::handle(syscall_num, args, frame))?;
        res
    }
    /// @brief 系统调用分发器，用于分发系统调用。
    ///
    /// 这个函数内，需要根据系统调用号，调用对应的系统调用处理函数。
    /// 并且，对于用户态传入的指针参数，需要在本函数内进行越界检查，防止访问到内核空间。
    #[inline(never)]
    pub fn handle(
        syscall_num: usize,
        args: &[usize],
        frame: &mut TrapFrame,
    ) -> Result<usize, SystemError> {
        let mut seccomp_args = [args[0], args[1], args[2], args[3], args[4], args[5]];
        let current_pcb = ProcessManager::current_pcb();
        let nr = syscall_num as u64;
        let skip = current_pcb.ptrace_report_syscall(true, nr, &seccomp_args);
        if skip {
            // SYSEMU：跳过真实 syscall 执行，回传当前返回值寄存器
            return Ok(frame.get_syscall_return());
        }
        // ptrace syscall-enter-stop 可能改写 syscall 号/参数，从 frame 重取。
        let nr_raw = frame.get_orig_syscall_nr();
        if nr_raw == -1 {
            // 补一次 exit-stop 以与前面已发生的 entry-stop 配对，再跳过执行。
            let _ = current_pcb.ptrace_report_syscall(false, nr_raw as u64, &seccomp_args);
            return Ok(frame.get_syscall_return());
        }
        let nr_after_ptrace = nr_raw as usize;
        let args_after_ptrace = crate::process::seccomp::frame_syscall_args(frame);
        seccomp_args = args_after_ptrace;
        let mut seccomp_skipped = false;
        match crate::process::seccomp::secure_computing(nr_after_ptrace, &seccomp_args, frame)? {
            crate::process::seccomp::SeccompDecision::Allow => {}
            crate::process::seccomp::SeccompDecision::Skip(ret) => {
                frame.set_return_value(ret);
                seccomp_skipped = true;
            }
        }

        defer::defer!({
            if ProcessManager::current_pcb()
                .flags()
                .contains(ProcessFlags::NEED_SCHEDULE)
            {
                schedule(SchedMode::SM_PREEMPT);
            }
        });

        // seccomp 未拦截时执行真正的 syscall；拦截则跳过执行。
        if !seccomp_skipped {
            // 首先尝试从 syscall_table 获取处理函数
            if let Some(handler) = syscall_table().get(nr_after_ptrace) {
                let show = false;
                if show {
                    log::debug!(
                        "pid: {} Syscall {} called with args {}",
                        ProcessManager::current_pid().data(),
                        handler.name,
                        handler.args_string(&args_after_ptrace)
                    );
                }

                let r = handler.inner_handle.handle(&args_after_ptrace, frame);
                if show {
                    log::debug!(
                        "pid: {} Syscall {} returned {:?}",
                        ProcessManager::current_pid().data(),
                        handler.name,
                        r
                    );
                }

                // 把最终返回值写入返回值寄存器
                let rax_value: usize = match r {
                    Ok(v) => v,
                    Err(e) => e.to_posix_errno() as i64 as u64 as usize,
                };
                frame.set_return_value(rax_value);
            } else {
                // fallback：未注册或未知 syscall。
                let r = match nr_after_ptrace {
                    SYS_PUT_STRING => Self::put_string(
                        args_after_ptrace[0] as *const u8,
                        args_after_ptrace[1] as u32,
                        args_after_ptrace[2] as u32,
                    ),
                    SYS_SBRK => {
                        let incr = args_after_ptrace[0] as isize;
                        crate::mm::syscall::sys_sbrk::sys_sbrk(incr)
                    }
                    SYS_CLOCK => Self::clock(),
                    SYS_SCHED => {
                        warn!("syscall sched");
                        schedule(SchedMode::SM_NONE);
                        Ok(0)
                    }
                    SYS_SYSLOG => {
                        let syslog_action_type = args_after_ptrace[0];
                        let buf_vaddr = args_after_ptrace[1];
                        let len = args_after_ptrace[2];
                        let from_user = frame.is_from_user();
                        if len == 0 {
                            Self::do_syslog(syslog_action_type, &mut [], 0)
                        } else {
                            let mut writer =
                                UserBufferWriter::new(buf_vaddr as *mut u8, len, from_user)?;
                            let buf = writer.buffer(0)?;
                            Self::do_syslog(syslog_action_type, buf, len)
                        }
                    }
                    SYS_FSYNC => {
                        warn!("SYS_FSYNC has not yet been implemented");
                        Ok(0)
                    }
                    _ => {
                        log::error!(
                            "Unsupported syscall ID: {} -> {}, args: {:?}",
                            nr_after_ptrace,
                            syscall_number_to_str(nr_after_ptrace),
                            args_after_ptrace
                        );
                        Err(SystemError::ENOSYS)
                    }
                };
                let rax_value: usize = match &r {
                    Ok(v) => *v,
                    Err(e) => e.to_posix_errno() as i64 as u64 as usize,
                };
                frame.set_return_value(rax_value);
            }
        }

        // 统一的 ptrace syscall-exit-stop：所有路径都必须经过 exit-stop。
        let current_pcb = ProcessManager::current_pcb();
        let _ = current_pcb.ptrace_report_syscall(false, nr_after_ptrace as u64, &seccomp_args);

        // 返回返回值寄存器（ptrace 可能已 POKEUSER 改写）；
        return Ok(frame.get_syscall_return());
    }

    pub fn put_string(
        s: *const u8,
        front_color: u32,
        back_color: u32,
    ) -> Result<usize, SystemError> {
        // todo: 删除这个系统调用
        let s = check_and_clone_cstr(s, Some(4096))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let fr = (front_color & 0x00ff0000) >> 16;
        let fg = (front_color & 0x0000ff00) >> 8;
        let fb = front_color & 0x000000ff;
        let br = (back_color & 0x00ff0000) >> 16;
        let bg = (back_color & 0x0000ff00) >> 8;
        let bb = back_color & 0x000000ff;
        print!("\x1B[38;2;{fr};{fg};{fb};48;2;{br};{bg};{bb}m{s}\x1B[0m");
        return Ok(s.len());
    }
}

#[inline(never)]
pub fn syscall_init() -> Result<(), SystemError> {
    syscall_table_init()?;
    Ok(())
}
