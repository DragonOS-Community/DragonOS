use core::sync::atomic::{AtomicBool, Ordering};

use crate::{
    arch::syscall::nr::*,
    libs::rand::GRandFlags,
    process::{ProcessFlags, ProcessManager},
    sched::{schedule, SchedMode},
    syscall::user_access::check_and_clone_cstr,
};

use log::{info, warn};
use system_error::SystemError;
use table::{syscall_table, syscall_table_init};

use crate::arch::interrupt::TrapFrame;

use self::{misc::SysInfo, user_access::UserBufferWriter};

pub mod misc;
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
        defer::defer!({
            if ProcessManager::current_pcb()
                .flags()
                .contains(ProcessFlags::NEED_SCHEDULE)
            {
                schedule(SchedMode::SM_PREEMPT);
            }
        });

        // 首先尝试从syscall_table获取处理函数
        if let Some(handler) = syscall_table().get(syscall_num) {
            // 使用以下代码可以打印系统调用号和参数，方便调试

            // let show = ProcessManager::current_pid().data() >= 12;
            let show = false;
            if show {
                log::debug!(
                    "pid: {} Syscall {} called with args {}",
                    ProcessManager::current_pid().data(),
                    handler.name,
                    handler.args_string(args)
                );
            }

            let r = handler.inner_handle.handle(args, frame);
            if show {
                log::debug!(
                    "pid: {} Syscall {} returned {:?}",
                    ProcessManager::current_pid().data(),
                    handler.name,
                    r
                );
            }
            return r;
        }

        // 如果找不到，fallback到原有逻辑
        let r = match syscall_num {
            SYS_PUT_STRING => {
                Self::put_string(args[0] as *const u8, args[1] as u32, args[2] as u32)
            }

            SYS_SBRK => {
                let incr = args[0] as isize;
                crate::mm::syscall::sys_sbrk::sys_sbrk(incr)
            }

            SYS_CLOCK => Self::clock(),

            SYS_SCHED => {
                warn!("syscall sched");
                schedule(SchedMode::SM_NONE);
                Ok(0)
            }

            // 目前为了适配musl-libc,以下系统调用先这样写着
            SYS_GETRANDOM => {
                let flags = GRandFlags::from_bits(args[2] as u8).ok_or(SystemError::EINVAL)?;
                Self::get_random(args[0] as *mut u8, args[1], flags)
            }

            #[cfg(target_arch = "x86_64")]
            SYS_POLL => {
                let fds = args[0];
                let nfds = args[1] as u32;
                let timeout = args[2] as i32;
                Self::poll(fds, nfds, timeout)
            }

            SYS_PPOLL => Self::ppoll(args[0], args[1] as u32, args[2], args[3]),

            SYS_SYSLOG => {
                let syslog_action_type = args[0];
                let buf_vaddr = args[1];
                let len = args[2];
                let from_user = frame.is_from_user();
                let mut user_buffer_writer =
                    UserBufferWriter::new(buf_vaddr as *mut u8, len, from_user)?;

                let user_buf = user_buffer_writer.buffer(0)?;
                Self::do_syslog(syslog_action_type, user_buf, len)
            }

            SYS_SYSINFO => {
                let info = args[0] as *mut SysInfo;
                Self::sysinfo(info)
            }

            SYS_FSYNC => {
                warn!("SYS_FSYNC has not yet been implemented");
                Ok(0)
            }

            SYS_RSEQ => {
                use crate::mm::VirtAddr;
                use crate::process::rseq;
                let rseq_ptr = VirtAddr::new(args[0]);
                let rseq_len = args[1] as u32;
                let flags = args[2] as i32;
                let sig = args[3] as u32;
                rseq::sys_rseq(rseq_ptr, rseq_len, flags, sig)
            }

            #[cfg(target_arch = "x86_64")]
            SYS_EVENTFD => {
                let initval = args[0] as u32;
                Self::sys_eventfd(initval, 0)
            }
            SYS_EVENTFD2 => {
                let initval = args[0] as u32;
                let flags = args[1] as u32;
                Self::sys_eventfd(initval, flags)
            }

            SYS_BPF => {
                let cmd = args[0] as u32;
                let attr = args[1] as *mut u8;
                let size = args[2] as u32;
                Self::sys_bpf(cmd, attr, size)
            }
            SYS_PERF_EVENT_OPEN => {
                let attr = args[0] as *const u8;
                let pid = args[1] as i32;
                let cpu = args[2] as i32;
                let group_fd = args[3] as i32;
                let flags = args[4] as u32;
                Self::sys_perf_event_open(attr, pid, cpu, group_fd, flags)
            }

            _ => {
                log::error!(
                    "Unsupported syscall ID: {} -> {}, args: {:?}",
                    syscall_num,
                    syscall_number_to_str(syscall_num),
                    args
                );
                Err(SystemError::ENOSYS)
            }
        };

        return r;
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
