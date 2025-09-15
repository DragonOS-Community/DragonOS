use core::{
    ffi::c_int,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{
    arch::syscall::nr::*,
    libs::{futex::constant::FutexFlag, rand::GRandFlags},
    mm::page::PAGE_4K_SIZE,
    net::posix::{MsgHdr, SockAddr},
    process::{ProcessFlags, ProcessManager},
    sched::{schedule, SchedMode},
    syscall::user_access::check_and_clone_cstr,
};

use log::{info, warn};
use system_error::SystemError;
use table::{syscall_table, syscall_table_init};

use crate::{
    arch::interrupt::TrapFrame,
    mm::{verify_area, VirtAddr},
    time::{
        syscall::{PosixTimeZone, PosixTimeval},
        PosixTimeSpec,
    },
};

use self::{
    misc::SysInfo,
    user_access::{UserBufferReader, UserBufferWriter},
};

pub mod misc;
pub mod table;
pub mod user_access;

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

            // let show = ProcessManager::current_pid().data() >= 8;
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

            SYS_REBOOT => {
                let magic1 = args[0] as u32;
                let magic2 = args[1] as u32;
                let cmd = args[2] as u32;
                let arg = args[3];
                Self::reboot(magic1, magic2, cmd, arg)
            }

            SYS_CLOCK => Self::clock(),

            SYS_SCHED => {
                warn!("syscall sched");
                schedule(SchedMode::SM_NONE);
                Ok(0)
            }

            SYS_SOCKET => Self::socket(args[0], args[1], args[2]),
            SYS_SETSOCKOPT => {
                let optval = args[3] as *const u8;
                let optlen = args[4];
                let virt_optval = VirtAddr::new(optval as usize);
                // 验证optval的地址是否合法
                if verify_area(virt_optval, optlen).is_err() {
                    // 地址空间超出了用户空间的范围，不合法
                    Err(SystemError::EFAULT)
                } else {
                    let data: &[u8] = unsafe { core::slice::from_raw_parts(optval, optlen) };
                    Self::setsockopt(args[0], args[1], args[2], data)
                }
            }
            SYS_GETSOCKOPT => {
                let optval = args[3] as *mut u8;
                let optlen = args[4] as *mut usize;
                let virt_optval = VirtAddr::new(optval as usize);
                let virt_optlen = VirtAddr::new(optlen as usize);
                let security_check = || {
                    // 验证optval的地址是否合法
                    if verify_area(virt_optval, PAGE_4K_SIZE).is_err() {
                        // 地址空间超出了用户空间的范围，不合法
                        return Err(SystemError::EFAULT);
                    }

                    // 验证optlen的地址是否合法
                    if verify_area(virt_optlen, core::mem::size_of::<u32>()).is_err() {
                        // 地址空间超出了用户空间的范围，不合法
                        return Err(SystemError::EFAULT);
                    }
                    return Ok(());
                };
                let r = security_check();
                if let Err(e) = r {
                    Err(e)
                } else {
                    Self::getsockopt(args[0], args[1], args[2], optval, optlen as *mut u32)
                }
            }

            SYS_CONNECT => {
                let addr = args[1] as *const SockAddr;
                let addrlen = args[2];
                let virt_addr = VirtAddr::new(addr as usize);
                // 验证addr的地址是否合法
                if verify_area(virt_addr, addrlen).is_err() {
                    // 地址空间超出了用户空间的范围，不合法
                    Err(SystemError::EFAULT)
                } else {
                    Self::connect(args[0], addr, addrlen as u32)
                }
            }

            SYS_BIND => {
                let addr = args[1] as *const SockAddr;
                let addrlen = args[2];
                let virt_addr = VirtAddr::new(addr as usize);
                // 验证addr的地址是否合法
                if verify_area(virt_addr, addrlen).is_err() {
                    // 地址空间超出了用户空间的范围，不合法
                    Err(SystemError::EFAULT)
                } else {
                    Self::bind(args[0], addr, addrlen as u32)
                }
            }

            SYS_SENDTO => {
                let buf = args[1] as *const u8;
                let len = args[2];
                let flags = args[3] as u32;
                let addr = args[4] as *const SockAddr;
                let addrlen = args[5];
                let virt_buf = VirtAddr::new(buf as usize);
                let virt_addr = VirtAddr::new(addr as usize);
                // 验证buf的地址是否合法
                if verify_area(virt_buf, len).is_err() || verify_area(virt_addr, addrlen).is_err() {
                    // 地址空间超出了用户空间的范围，不合法
                    Err(SystemError::EFAULT)
                } else {
                    let data: &[u8] = unsafe { core::slice::from_raw_parts(buf, len) };
                    Self::sendto(args[0], data, flags, addr, addrlen as u32)
                }
            }

            SYS_RECVFROM => {
                let buf = args[1] as *mut u8;
                let len = args[2];
                let flags = args[3] as u32;
                let addr = args[4] as *mut SockAddr;
                let addrlen = args[5] as *mut u32;
                let virt_buf = VirtAddr::new(buf as usize);
                let virt_addrlen = VirtAddr::new(addrlen as usize);
                let virt_addr = VirtAddr::new(addr as usize);
                let security_check = || {
                    // 验证buf的地址是否合法
                    if verify_area(virt_buf, len).is_err() {
                        // 地址空间超出了用户空间的范围，不合法
                        return Err(SystemError::EFAULT);
                    }

                    // 验证addrlen的地址是否合法
                    if verify_area(virt_addrlen, core::mem::size_of::<usize>()).is_err() {
                        // 地址空间超出了用户空间的范围，不合法
                        return Err(SystemError::EFAULT);
                    }

                    if verify_area(virt_addr, core::mem::size_of::<SockAddr>()).is_err() {
                        // 地址空间超出了用户空间的范围，不合法
                        return Err(SystemError::EFAULT);
                    }
                    return Ok(());
                };
                if let Err(e) = security_check() {
                    Err(e)
                } else {
                    let buf = unsafe { core::slice::from_raw_parts_mut(buf, len) };
                    Self::recvfrom(args[0], buf, flags, addr, addrlen)
                }
            }

            SYS_RECVMSG => {
                let msg = args[1] as *mut MsgHdr;
                let flags = args[2] as u32;

                let mut user_buffer_writer = UserBufferWriter::new(
                    msg,
                    core::mem::size_of::<MsgHdr>(),
                    frame.is_from_user(),
                )?;
                let buffer = user_buffer_writer.buffer::<MsgHdr>(0)?;

                let msg = &mut buffer[0];
                Self::recvmsg(args[0], msg, flags)
            }

            SYS_LISTEN => Self::listen(args[0], args[1]),
            SYS_SHUTDOWN => Self::shutdown(args[0], args[1]),
            SYS_ACCEPT => Self::accept(args[0], args[1] as *mut SockAddr, args[2] as *mut u32),
            SYS_ACCEPT4 => Self::accept4(
                args[0],
                args[1] as *mut SockAddr,
                args[2] as *mut u32,
                args[3] as u32,
            ),
            SYS_GETSOCKNAME => {
                Self::getsockname(args[0], args[1] as *mut SockAddr, args[2] as *mut u32)
            }
            SYS_GETPEERNAME => {
                Self::getpeername(args[0], args[1] as *mut SockAddr, args[2] as *mut u32)
            }
            SYS_GETTIMEOFDAY => {
                let timeval = args[0] as *mut PosixTimeval;
                let timezone_ptr = args[1] as *mut PosixTimeZone;
                Self::gettimeofday(timeval, timezone_ptr)
            }

            SYS_FUTEX => {
                let uaddr = VirtAddr::new(args[0]);
                let operation = FutexFlag::from_bits(args[1] as u32).ok_or(SystemError::ENOSYS)?;
                let val = args[2] as u32;
                let utime = args[3];
                let uaddr2 = VirtAddr::new(args[4]);
                let val3 = args[5] as u32;

                let mut timespec = None;
                if utime != 0 && operation.contains(FutexFlag::FLAGS_HAS_TIMEOUT) {
                    let reader = UserBufferReader::new(
                        utime as *const PosixTimeSpec,
                        core::mem::size_of::<PosixTimeSpec>(),
                        true,
                    )?;

                    timespec = Some(*reader.read_one_from_user::<PosixTimeSpec>(0)?);
                }

                Self::do_futex(uaddr, operation, val, timespec, uaddr2, utime as u32, val3)
            }

            SYS_SET_ROBUST_LIST => {
                let head = args[0];
                let head_uaddr = VirtAddr::new(head);
                let len = args[1];

                let ret = Self::set_robust_list(head_uaddr, len);
                return ret;
            }

            SYS_GET_ROBUST_LIST => {
                let pid = args[0];
                let head = args[1];
                let head_uaddr = VirtAddr::new(head);
                let len_ptr = args[2];
                let len_ptr_uaddr = VirtAddr::new(len_ptr);

                let ret = Self::get_robust_list(pid, head_uaddr, len_ptr_uaddr);
                return ret;
            }

            // 目前为了适配musl-libc,以下系统调用先这样写着
            SYS_GETRANDOM => {
                let flags = GRandFlags::from_bits(args[2] as u8).ok_or(SystemError::EINVAL)?;
                Self::get_random(args[0] as *mut u8, args[1], flags)
            }

            SYS_SOCKETPAIR => {
                let mut user_buffer_writer = UserBufferWriter::new(
                    args[3] as *mut c_int,
                    core::mem::size_of::<[c_int; 2]>(),
                    frame.is_from_user(),
                )?;
                let fds = user_buffer_writer.buffer::<i32>(0)?;
                Self::socketpair(args[0], args[1], args[2], fds)
            }

            #[cfg(target_arch = "x86_64")]
            SYS_POLL => {
                let fds = args[0];
                let nfds = args[1] as u32;
                let timeout = args[2] as i32;
                Self::poll(fds, nfds, timeout)
            }

            SYS_PPOLL => Self::ppoll(args[0], args[1] as u32, args[2], args[3]),

            SYS_TKILL => {
                warn!("SYS_TKILL has not yet been implemented");
                Ok(0)
            }

            SYS_SIGALTSTACK => {
                warn!("SYS_SIGALTSTACK has not yet been implemented");
                Ok(0)
            }

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

            SYS_CLOCK_GETTIME => {
                let clockid = args[0] as i32;
                let timespec = args[1] as *mut PosixTimeSpec;
                Self::clock_gettime(clockid, timespec)
            }

            SYS_SYSINFO => {
                let info = args[0] as *mut SysInfo;
                Self::sysinfo(info)
            }

            SYS_UMASK => {
                let mask = args[0] as u32;
                Self::umask(mask)
            }

            SYS_FSYNC => {
                warn!("SYS_FSYNC has not yet been implemented");
                Ok(0)
            }

            SYS_RSEQ => {
                warn!("SYS_RSEQ has not yet been implemented");
                Err(SystemError::ENOSYS)
            }

            SYS_SCHED_YIELD => Self::do_sched_yield(),

            SYS_SCHED_GETAFFINITY => {
                let pid = args[0] as i32;
                let size = args[1];
                let set_vaddr = args[2];

                let mut user_buffer_writer =
                    UserBufferWriter::new(set_vaddr as *mut u8, size, frame.is_from_user())?;
                let set: &mut [u8] = user_buffer_writer.buffer(0)?;

                Self::getaffinity(pid, set)
            }

            SYS_FADVISE64 => {
                // todo: 这个系统调用还没有实现

                Err(SystemError::ENOSYS)
            }

            // SYS_SCHED_YIELD => Self::sched_yield(),
            SYS_PRCTL => {
                // todo: 这个系统调用还没有实现

                Err(SystemError::EINVAL)
            }

            #[cfg(target_arch = "x86_64")]
            SYS_ALARM => {
                let second = args[0] as u32;
                Self::alarm(second)
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
            #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
            SYS_SETRLIMIT => Ok(0),

            SYS_RT_SIGTIMEDWAIT => {
                log::warn!("SYS_RT_SIGTIMEDWAIT has not yet been implemented");
                Ok(0)
            }
            _ => {
                panic!(
                    "Unsupported syscall ID: {} -> {}, args: {:?}",
                    syscall_num,
                    syscall_number_to_str(syscall_num),
                    args
                );
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
