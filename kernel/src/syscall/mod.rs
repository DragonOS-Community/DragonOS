use core::{
    ffi::c_int,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{
    arch::syscall::nr::*,
    filesystem::vfs::syscall::PosixStatfs,
    libs::{futex::constant::FutexFlag, rand::GRandFlags},
    mm::page::PAGE_4K_SIZE,
    net::syscall::MsgHdr,
    process::{ProcessFlags, ProcessManager},
    sched::{schedule, SchedMode},
    syscall::user_access::check_and_clone_cstr,
};

use log::{info, warn};
use num_traits::FromPrimitive;
use system_error::SystemError;
use table::{syscall_table, syscall_table_init};

use crate::{
    arch::interrupt::TrapFrame,
    filesystem::vfs::{
        fcntl::{AtFlags, FcntlCommand},
        syscall::{ModeType, UtimensFlags},
    },
    mm::{verify_area, VirtAddr},
    net::syscall::SockAddr,
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

            #[cfg(target_arch = "x86_64")]
            SYS_RENAME => {
                let oldname: *const u8 = args[0] as *const u8;
                let newname: *const u8 = args[1] as *const u8;
                Self::do_renameat2(
                    AtFlags::AT_FDCWD.bits(),
                    oldname,
                    AtFlags::AT_FDCWD.bits(),
                    newname,
                    0,
                )
            }

            #[cfg(target_arch = "x86_64")]
            SYS_RENAMEAT => {
                let oldfd = args[0] as i32;
                let oldname: *const u8 = args[1] as *const u8;
                let newfd = args[2] as i32;
                let newname: *const u8 = args[3] as *const u8;
                Self::do_renameat2(oldfd, oldname, newfd, newname, 0)
            }

            SYS_RENAMEAT2 => {
                let oldfd = args[0] as i32;
                let oldname: *const u8 = args[1] as *const u8;
                let newfd = args[2] as i32;
                let newname: *const u8 = args[3] as *const u8;
                let flags = args[4] as u32;
                Self::do_renameat2(oldfd, oldname, newfd, newname, flags)
            }

            SYS_OPENAT => {
                let dirfd = args[0] as i32;
                let path = args[1] as *const u8;
                let flags = args[2] as u32;
                let mode = args[3] as u32;

                Self::openat(dirfd, path, flags, mode, true)
            }

            SYS_LSEEK => {
                let fd = args[0] as i32;
                let offset = args[1] as i64;
                let whence = args[2] as u32;

                Self::lseek(fd, offset, whence)
            }

            SYS_PREAD64 => {
                let fd = args[0] as i32;
                let buf_vaddr = args[1];
                let len = args[2];
                let offset = args[3];

                let mut user_buffer_writer =
                    UserBufferWriter::new(buf_vaddr as *mut u8, len, frame.is_from_user())?;
                let buf = user_buffer_writer.buffer(0)?;
                Self::pread(fd, buf, len, offset)
            }

            SYS_PWRITE64 => {
                let fd = args[0] as i32;
                let buf_vaddr = args[1];
                let len = args[2];
                let offset = args[3];

                let user_buffer_reader =
                    UserBufferReader::new(buf_vaddr as *const u8, len, frame.is_from_user())?;

                let buf = user_buffer_reader.read_from_user(0)?;
                Self::pwrite(fd, buf, len, offset)
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

            SYS_CHDIR => {
                let r = args[0] as *const u8;
                Self::chdir(r)
            }
            SYS_FCHDIR => {
                let fd = args[0] as i32;
                Self::fchdir(fd)
            }

            #[allow(unreachable_patterns)]
            SYS_GETDENTS64 | SYS_GETDENTS => {
                let fd = args[0] as i32;

                let buf_vaddr = args[1];
                let len = args[2];
                let virt_addr: VirtAddr = VirtAddr::new(buf_vaddr);
                // 判断缓冲区是否来自用户态，进行权限校验
                let res = if frame.is_from_user() && verify_area(virt_addr, len).is_err() {
                    // 来自用户态，而buffer在内核态，这样的操作不被允许
                    Err(SystemError::EPERM)
                } else if buf_vaddr == 0 {
                    Err(SystemError::EFAULT)
                } else {
                    let buf: &mut [u8] = unsafe {
                        core::slice::from_raw_parts_mut::<'static, u8>(buf_vaddr as *mut u8, len)
                    };
                    Self::getdents(fd, buf)
                };

                res
            }

            #[cfg(target_arch = "x86_64")]
            SYS_MKDIR => {
                let path = args[0] as *const u8;
                let mode = args[1];

                Self::mkdir(path, mode)
            }

            SYS_MKDIRAT => {
                let dirfd = args[0] as i32;
                let path = args[1] as *const u8;
                let mode = args[2];
                Self::mkdir_at(dirfd, path, mode)
            }

            SYS_CLOCK => Self::clock(),
            SYS_UNLINKAT => {
                let dirfd = args[0] as i32;
                let path = args[1] as *const u8;
                let flags = args[2] as u32;
                Self::unlinkat(dirfd, path, flags)
            }

            #[cfg(target_arch = "x86_64")]
            SYS_RMDIR => {
                let path = args[0] as *const u8;
                Self::rmdir(path)
            }

            #[cfg(target_arch = "x86_64")]
            SYS_LINK => {
                let old = args[0] as *const u8;
                let new = args[1] as *const u8;
                return Self::link(old, new);
            }

            SYS_LINKAT => {
                let oldfd = args[0] as i32;
                let old = args[1] as *const u8;
                let newfd = args[2] as i32;
                let new = args[3] as *const u8;
                let flags = args[4] as i32;
                return Self::linkat(oldfd, old, newfd, new, flags);
            }

            #[cfg(target_arch = "x86_64")]
            SYS_UNLINK => {
                let path = args[0] as *const u8;
                Self::unlink(path)
            }

            SYS_SCHED => {
                warn!("syscall sched");
                schedule(SchedMode::SM_NONE);
                Ok(0)
            }
            SYS_DUP => {
                let oldfd: i32 = args[0] as c_int;
                Self::dup(oldfd)
            }

            #[cfg(target_arch = "x86_64")]
            SYS_DUP2 => {
                let oldfd: i32 = args[0] as c_int;
                let newfd: i32 = args[1] as c_int;
                Self::dup2(oldfd, newfd)
            }

            SYS_DUP3 => {
                let oldfd: i32 = args[0] as c_int;
                let newfd: i32 = args[1] as c_int;
                let flags: u32 = args[2] as u32;
                Self::dup3(oldfd, newfd, flags)
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
                    Self::connect(args[0], addr, addrlen)
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
                    Self::bind(args[0], addr, addrlen)
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
                    Self::sendto(args[0], data, flags, addr, addrlen)
                }
            }

            SYS_RECVFROM => {
                let buf = args[1] as *mut u8;
                let len = args[2];
                let flags = args[3] as u32;
                let addr = args[4] as *mut SockAddr;
                let addrlen = args[5] as *mut usize;
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
                    if verify_area(virt_addrlen, core::mem::size_of::<u32>()).is_err() {
                        // 地址空间超出了用户空间的范围，不合法
                        return Err(SystemError::EFAULT);
                    }

                    if verify_area(virt_addr, core::mem::size_of::<SockAddr>()).is_err() {
                        // 地址空间超出了用户空间的范围，不合法
                        return Err(SystemError::EFAULT);
                    }
                    return Ok(());
                };
                let r = security_check();
                if let Err(e) = r {
                    Err(e)
                } else {
                    let buf = unsafe { core::slice::from_raw_parts_mut(buf, len) };
                    Self::recvfrom(args[0], buf, flags, addr, addrlen as *mut u32)
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

            SYS_GETCWD => {
                let buf = args[0] as *mut u8;
                let size = args[1];
                let security_check = || {
                    verify_area(VirtAddr::new(buf as usize), size)?;
                    return Ok(());
                };
                let r = security_check();
                if let Err(e) = r {
                    Err(e)
                } else {
                    let buf = unsafe { core::slice::from_raw_parts_mut(buf, size) };
                    Self::getcwd(buf)
                }
            }

            SYS_FCNTL => {
                let fd = args[0] as i32;
                let cmd: Option<FcntlCommand> =
                    <FcntlCommand as FromPrimitive>::from_u32(args[1] as u32);
                let arg = args[2] as i32;
                let res = if let Some(cmd) = cmd {
                    Self::fcntl(fd, cmd, arg)
                } else {
                    Err(SystemError::EINVAL)
                };

                // debug!("FCNTL: fd: {}, cmd: {:?}, arg: {}, res: {:?}", fd, cmd, arg, res);
                res
            }

            SYS_FTRUNCATE => {
                let fd = args[0] as i32;
                let len = args[1];
                let res = Self::ftruncate(fd, len);
                // debug!("FTRUNCATE: fd: {}, len: {}, res: {:?}", fd, len, res);
                res
            }

            #[cfg(target_arch = "x86_64")]
            SYS_MKNOD => {
                let path = args[0];
                let flags = args[1];
                let dev_t = args[2];
                let flags: ModeType = ModeType::from_bits_truncate(flags as u32);
                Self::mknod(
                    path as *const u8,
                    flags,
                    crate::driver::base::device::device_number::DeviceNumber::from(dev_t as u32),
                )
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

            SYS_STATFS => {
                let path = args[0] as *const u8;
                let statfs = args[1] as *mut PosixStatfs;
                Self::statfs(path, statfs)
            }

            SYS_FSTATFS => {
                let fd = args[0] as i32;
                let statfs = args[1] as *mut PosixStatfs;
                Self::fstatfs(fd, statfs)
            }

            SYS_STATX => Self::statx(
                args[0] as i32,
                args[1],
                args[2] as u32,
                args[3] as u32,
                args[4],
            ),

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

            #[cfg(target_arch = "x86_64")]
            SYS_READLINK => {
                let path = args[0] as *const u8;
                let buf = args[1] as *mut u8;
                let bufsiz = args[2];
                Self::readlink(path, buf, bufsiz)
            }

            SYS_READLINKAT => {
                let dirfd = args[0] as i32;
                let path = args[1] as *const u8;
                let buf = args[2] as *mut u8;
                let bufsiz = args[3];
                Self::readlink_at(dirfd, path, buf, bufsiz)
            }

            #[cfg(target_arch = "x86_64")]
            SYS_ACCESS => {
                let pathname = args[0] as *const u8;
                let mode = args[1] as u32;
                Self::access(pathname, mode)
            }

            SYS_FACCESSAT => {
                let dirfd = args[0] as i32;
                let pathname = args[1] as *const u8;
                let mode = args[2] as u32;
                Self::faccessat2(dirfd, pathname, mode, 0)
            }

            SYS_FACCESSAT2 => {
                let dirfd = args[0] as i32;
                let pathname = args[1] as *const u8;
                let mode = args[2] as u32;
                let flags = args[3] as u32;
                Self::faccessat2(dirfd, pathname, mode, flags)
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

            SYS_FCHOWN => {
                let dirfd = args[0] as i32;
                let uid = args[1];
                let gid = args[2];
                Self::fchown(dirfd, uid, gid)
            }
            #[cfg(target_arch = "x86_64")]
            SYS_CHOWN => {
                let pathname = args[0] as *const u8;
                let uid = args[1];
                let gid = args[2];
                Self::chown(pathname, uid, gid)
            }
            #[cfg(target_arch = "x86_64")]
            SYS_LCHOWN => {
                let pathname = args[0] as *const u8;
                let uid = args[1];
                let gid = args[2];
                Self::lchown(pathname, uid, gid)
            }
            SYS_FCHOWNAT => {
                let dirfd = args[0] as i32;
                let pathname = args[1] as *const u8;
                let uid = args[2];
                let gid = args[3];
                let flag = args[4] as i32;
                Self::fchownat(dirfd, pathname, uid, gid, flag)
            }

            SYS_FSYNC => {
                warn!("SYS_FSYNC has not yet been implemented");
                Ok(0)
            }

            SYS_RSEQ => {
                warn!("SYS_RSEQ has not yet been implemented");
                Err(SystemError::ENOSYS)
            }

            #[cfg(target_arch = "x86_64")]
            SYS_CHMOD => {
                let pathname = args[0] as *const u8;
                let mode = args[1] as u32;
                Self::chmod(pathname, mode)
            }
            SYS_FCHMOD => {
                let fd = args[0] as i32;
                let mode = args[1] as u32;
                Self::fchmod(fd, mode)
            }
            SYS_FCHMODAT => {
                let dirfd = args[0] as i32;
                let pathname = args[1] as *const u8;
                let mode = args[2] as u32;
                Self::fchmodat(dirfd, pathname, mode)
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

            #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
            SYS_NEWFSTATAT => Self::newfstatat(args[0] as i32, args[1], args[2], args[3] as u32),

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

            SYS_UTIMENSAT => Self::sys_utimensat(
                args[0] as i32,
                args[1] as *const u8,
                args[2] as *const PosixTimeSpec,
                args[3] as u32,
            ),
            #[cfg(target_arch = "x86_64")]
            SYS_FUTIMESAT => {
                let flags = UtimensFlags::empty();
                Self::sys_utimensat(
                    args[0] as i32,
                    args[1] as *const u8,
                    args[2] as *const PosixTimeSpec,
                    flags.bits(),
                )
            }
            #[cfg(target_arch = "x86_64")]
            SYS_UTIMES => Self::sys_utimes(args[0] as *const u8, args[1] as *const PosixTimeval),
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
            _ => panic!("Unsupported syscall ID: {}", syscall_num),
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
