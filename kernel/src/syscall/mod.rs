use core::{
    ffi::{c_int, c_void},
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{
    arch::{ipc::signal::SigSet, syscall::nr::*},
    filesystem::vfs::syscall::{PosixStatfs, PosixStatx},
    ipc::shm::{ShmCtlCmd, ShmFlags, ShmId, ShmKey},
    libs::{futex::constant::FutexFlag, rand::GRandFlags},
    mm::{page::PAGE_4K_SIZE, syscall::MremapFlags},
    net::syscall::MsgHdr,
    process::{
        fork::KernelCloneArgs,
        process_group::Pgid,
        resource::{RLimit64, RUsage},
        ProcessFlags, ProcessManager,
    },
    sched::{schedule, SchedMode},
    syscall::user_access::check_and_clone_cstr,
};

use log::{info, warn};
use num_traits::FromPrimitive;
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, MMArch},
    filesystem::vfs::{
        fcntl::{AtFlags, FcntlCommand},
        file::FileMode,
        syscall::{ModeType, PosixKstat, UtimensFlags},
        MAX_PATHLEN,
    },
    libs::align::page_align_up,
    mm::{verify_area, MemoryManagementArch, VirtAddr},
    net::syscall::SockAddr,
    process::{fork::CloneFlags, syscall::PosixOldUtsName, Pid},
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
        let r = match syscall_num {
            SYS_PUT_STRING => {
                Self::put_string(args[0] as *const u8, args[1] as u32, args[2] as u32)
            }
            #[cfg(target_arch = "x86_64")]
            SYS_OPEN => {
                let path = args[0] as *const u8;
                let flags = args[1] as u32;
                let mode = args[2] as u32;

                Self::open(path, flags, mode, true)
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
            SYS_CLOSE => {
                let fd = args[0];
                Self::close(fd)
            }
            SYS_READ => {
                let fd = args[0] as i32;
                let buf_vaddr = args[1];
                let len = args[2];
                let from_user = frame.is_from_user();
                let mut user_buffer_writer =
                    UserBufferWriter::new(buf_vaddr as *mut u8, len, from_user)?;

                let user_buf = user_buffer_writer.buffer(0)?;
                Self::read(fd, user_buf)
            }
            SYS_WRITE => {
                let fd = args[0] as i32;
                let buf_vaddr = args[1];
                let len = args[2];
                let from_user = frame.is_from_user();
                let user_buffer_reader =
                    UserBufferReader::new(buf_vaddr as *const u8, len, from_user)?;

                let user_buf = user_buffer_reader.read_from_user(0)?;
                Self::write(fd, user_buf)
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

            SYS_IOCTL => {
                let fd = args[0];
                let cmd = args[1];
                let data = args[2];
                Self::ioctl(fd, cmd as u32, data)
            }

            #[cfg(target_arch = "x86_64")]
            SYS_FORK => Self::fork(frame),
            #[cfg(target_arch = "x86_64")]
            SYS_VFORK => Self::vfork(frame),

            SYS_BRK => {
                let new_brk = VirtAddr::new(args[0]);
                Self::brk(new_brk).map(|vaddr| vaddr.data())
            }

            SYS_SBRK => {
                let increment = args[0] as isize;
                Self::sbrk(increment).map(|vaddr: VirtAddr| vaddr.data())
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

            SYS_EXECVE => {
                let path_ptr = args[0];
                let argv_ptr = args[1];
                let env_ptr = args[2];
                let virt_path_ptr = VirtAddr::new(path_ptr);
                let virt_argv_ptr = VirtAddr::new(argv_ptr);
                let virt_env_ptr = VirtAddr::new(env_ptr);
                // 权限校验
                if frame.is_from_user()
                    && (verify_area(virt_path_ptr, MAX_PATHLEN).is_err()
                        || verify_area(virt_argv_ptr, PAGE_4K_SIZE).is_err())
                    || verify_area(virt_env_ptr, PAGE_4K_SIZE).is_err()
                {
                    Err(SystemError::EFAULT)
                } else {
                    Self::execve(
                        path_ptr as *const u8,
                        argv_ptr as *const *const u8,
                        env_ptr as *const *const u8,
                        frame,
                    )
                    .map(|_| 0)
                }
            }
            SYS_WAIT4 => {
                let pid = args[0] as i32;
                let wstatus = args[1] as *mut i32;
                let options = args[2] as c_int;
                let rusage = args[3] as *mut c_void;
                // 权限校验
                // todo: 引入rusage之后，更正以下权限校验代码中，rusage的大小
                Self::wait4(pid.into(), wstatus, options, rusage)
            }

            SYS_EXIT => {
                let exit_code = args[0];
                Self::exit(exit_code)
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

            SYS_NANOSLEEP => {
                let req = args[0] as *const PosixTimeSpec;
                let rem = args[1] as *mut PosixTimeSpec;
                let virt_req = VirtAddr::new(req as usize);
                let virt_rem = VirtAddr::new(rem as usize);
                if frame.is_from_user()
                    && (verify_area(virt_req, core::mem::size_of::<PosixTimeSpec>()).is_err()
                        || verify_area(virt_rem, core::mem::size_of::<PosixTimeSpec>()).is_err())
                {
                    Err(SystemError::EFAULT)
                } else {
                    Self::nanosleep(req, rem)
                }
            }

            SYS_CLOCK => Self::clock(),
            #[cfg(target_arch = "x86_64")]
            SYS_PIPE => {
                let pipefd: *mut i32 = args[0] as *mut c_int;
                if pipefd.is_null() {
                    Err(SystemError::EFAULT)
                } else {
                    Self::pipe2(pipefd, FileMode::empty())
                }
            }

            SYS_PIPE2 => {
                let pipefd: *mut i32 = args[0] as *mut c_int;
                let arg1 = args[1];
                if pipefd.is_null() {
                    Err(SystemError::EFAULT)
                } else {
                    let flags = FileMode::from_bits_truncate(arg1 as u32);
                    Self::pipe2(pipefd, flags)
                }
            }

            SYS_UNLINKAT => {
                let dirfd = args[0] as i32;
                let path = args[1] as *const u8;
                let flags = args[2] as u32;
                Self::unlinkat(dirfd, path, flags)
            }

            #[cfg(target_arch = "x86_64")]
            SYS_SYMLINK => {
                let oldname = args[0] as *const u8;
                let newname = args[1] as *const u8;
                Self::symlink(oldname, newname)
            }

            SYS_SYMLINKAT => {
                let oldname = args[0] as *const u8;
                let newdfd = args[1] as i32;
                let newname = args[2] as *const u8;
                Self::symlinkat(oldname, newdfd, newname)
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
            SYS_KILL => {
                let pid = args[0] as i32;
                let sig = args[1] as c_int;
                // debug!("KILL SYSCALL RECEIVED");
                Self::kill(pid, sig)
            }

            SYS_RT_SIGACTION => {
                let sig = args[0] as c_int;
                let act = args[1];
                let old_act = args[2];
                Self::sigaction(sig, act, old_act, frame.is_from_user())
            }

            SYS_GETPID => Self::getpid().map(|pid| pid.into()),

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
            SYS_MMAP => {
                let len = page_align_up(args[1]);
                let virt_addr = VirtAddr::new(args[0]);
                if verify_area(virt_addr, len).is_err() {
                    Err(SystemError::EFAULT)
                } else {
                    Self::mmap(
                        VirtAddr::new(args[0]),
                        len,
                        args[2],
                        args[3],
                        args[4] as i32,
                        args[5],
                    )
                }
            }
            SYS_MREMAP => {
                let old_vaddr = VirtAddr::new(args[0]);
                let old_len = args[1];
                let new_len = args[2];
                let mremap_flags = MremapFlags::from_bits_truncate(args[3] as u8);
                let new_vaddr = VirtAddr::new(args[4]);

                Self::mremap(old_vaddr, old_len, new_len, mremap_flags, new_vaddr)
            }
            SYS_MUNMAP => {
                let addr = args[0];
                let len = page_align_up(args[1]);
                if addr & (MMArch::PAGE_SIZE - 1) != 0 {
                    // The addr argument is not a multiple of the page size
                    Err(SystemError::EINVAL)
                } else {
                    Self::munmap(VirtAddr::new(addr), len)
                }
            }
            SYS_MPROTECT => {
                let addr = args[0];
                let len = page_align_up(args[1]);
                if addr & (MMArch::PAGE_SIZE - 1) != 0 {
                    // The addr argument is not a multiple of the page size
                    Err(SystemError::EINVAL)
                } else {
                    Self::mprotect(VirtAddr::new(addr), len, args[2])
                }
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
                    Self::getcwd(buf).map(|ptr| ptr.data())
                }
            }

            SYS_GETPGID => Self::getpgid(Pid::new(args[0])).map(|pgid| pgid.into()),

            SYS_GETPPID => Self::getppid().map(|pid| pid.into()),

            #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
            SYS_FSTAT => {
                let fd = args[0] as i32;
                let kstat: *mut PosixKstat = args[1] as *mut PosixKstat;
                let vaddr = VirtAddr::new(kstat as usize);
                // FIXME 由于c中的verify_area与rust中的verify_area重名，所以在引入时加了前缀区分
                // TODO 应该将用了c版本的verify_area都改为rust的verify_area
                match verify_area(vaddr, core::mem::size_of::<PosixKstat>()) {
                    Ok(_) => Self::fstat(fd, kstat),
                    Err(e) => Err(e),
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

            SYS_CLONE => {
                let parent_tid = VirtAddr::new(args[2]);
                let child_tid = VirtAddr::new(args[3]);

                // 地址校验
                verify_area(parent_tid, core::mem::size_of::<i32>())?;
                verify_area(child_tid, core::mem::size_of::<i32>())?;

                let mut clone_args = KernelCloneArgs::new();
                clone_args.flags = CloneFlags::from_bits_truncate(args[0] as u64);
                clone_args.stack = args[1];
                clone_args.parent_tid = parent_tid;
                clone_args.child_tid = child_tid;
                clone_args.tls = args[4];
                Self::clone(frame, clone_args)
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

            SYS_READV => Self::readv(args[0] as i32, args[1], args[2]),
            SYS_WRITEV => Self::writev(args[0] as i32, args[1], args[2]),

            SYS_SET_TID_ADDRESS => Self::set_tid_address(args[0]),

            #[cfg(target_arch = "x86_64")]
            SYS_LSTAT => {
                let path = args[0] as *const u8;
                let kstat = args[1] as *mut PosixKstat;
                Self::lstat(path, kstat)
            }

            #[cfg(target_arch = "x86_64")]
            SYS_STAT => {
                let path = args[0] as *const u8;
                let kstat = args[1] as *mut PosixKstat;
                Self::stat(path, kstat)
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

            SYS_STATX => {
                let fd = args[0] as i32;
                let path = args[1] as *const u8;
                let flags = args[2] as u32;
                let mask = args[3] as u32;
                let kstat = args[4] as *mut PosixStatx;

                Self::do_statx(fd, path, flags, mask, kstat)
            }

            #[cfg(target_arch = "x86_64")]
            SYS_EPOLL_CREATE => Self::epoll_create(args[0] as i32),
            SYS_EPOLL_CREATE1 => Self::epoll_create1(args[0]),

            SYS_EPOLL_CTL => Self::epoll_ctl(
                args[0] as i32,
                args[1],
                args[2] as i32,
                VirtAddr::new(args[3]),
            ),

            #[cfg(target_arch = "x86_64")]
            SYS_EPOLL_WAIT => Self::epoll_wait(
                args[0] as i32,
                VirtAddr::new(args[1]),
                args[2] as i32,
                args[3] as i32,
            ),

            SYS_EPOLL_PWAIT => {
                let epfd = args[0] as i32;
                let epoll_event = VirtAddr::new(args[1]);
                let max_events = args[2] as i32;
                let timespec = args[3] as i32;
                let sigmask_addr = args[4] as *mut SigSet;

                if sigmask_addr.is_null() {
                    return Self::epoll_wait(epfd, epoll_event, max_events, timespec);
                }
                let sigmask_reader =
                    UserBufferReader::new(sigmask_addr, core::mem::size_of::<SigSet>(), true)?;
                let mut sigmask = *sigmask_reader.read_one_from_user::<SigSet>(0)?;

                Self::epoll_pwait(
                    args[0] as i32,
                    VirtAddr::new(args[1]),
                    args[2] as i32,
                    args[3] as i32,
                    &mut sigmask,
                )
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

            SYS_SETPGID => {
                let pid = Pid::new(args[0]);
                let pgid = Pgid::new(args[1]);
                Self::setpgid(pid, pgid)
            }

            SYS_RT_SIGPROCMASK => {
                let how = args[0] as i32;
                let nset = args[1];
                let oset = args[2];
                let sigsetsize = args[3];
                Self::rt_sigprocmask(how, nset, oset, sigsetsize)
            }

            SYS_TKILL => {
                warn!("SYS_TKILL has not yet been implemented");
                Ok(0)
            }

            SYS_SIGALTSTACK => {
                warn!("SYS_SIGALTSTACK has not yet been implemented");
                Ok(0)
            }

            SYS_EXIT_GROUP => {
                warn!("SYS_EXIT_GROUP has not yet been implemented");
                Ok(0)
            }

            SYS_MADVISE => {
                let addr = args[0];
                let len = page_align_up(args[1]);
                if addr & (MMArch::PAGE_SIZE - 1) != 0 {
                    Err(SystemError::EINVAL)
                } else {
                    Self::madvise(VirtAddr::new(addr), len, args[2])
                }
            }

            SYS_GETTID => Self::gettid().map(|tid| tid.into()),

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

            SYS_GETUID => Self::getuid(),
            SYS_GETGID => Self::getgid(),
            SYS_SETUID => Self::setuid(args[0]),
            SYS_SETGID => Self::setgid(args[0]),

            SYS_GETEUID => Self::geteuid(),
            SYS_GETEGID => Self::getegid(),
            SYS_SETRESUID => Self::seteuid(args[1]),
            SYS_SETRESGID => Self::setegid(args[1]),

            SYS_SETFSUID => Self::setfsuid(args[0]),
            SYS_SETFSGID => Self::setfsgid(args[0]),

            SYS_SETSID => Self::setsid(),
            SYS_GETSID => Self::getsid(Pid::new(args[0])),

            SYS_GETRUSAGE => {
                let who = args[0] as c_int;
                let rusage = args[1] as *mut RUsage;
                Self::get_rusage(who, rusage)
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

            SYS_PRLIMIT64 => {
                let pid = args[0];
                let pid = Pid::new(pid);
                let resource = args[1];
                let new_limit = args[2] as *const RLimit64;
                let old_limit = args[3] as *mut RLimit64;

                Self::prlimit64(pid, resource, new_limit, old_limit)
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
                Ok(0)
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

            #[cfg(target_arch = "x86_64")]
            SYS_GETRLIMIT => {
                let resource = args[0];
                let rlimit = args[1] as *mut RLimit64;

                Self::prlimit64(
                    ProcessManager::current_pcb().pid(),
                    resource,
                    core::ptr::null::<RLimit64>(),
                    rlimit,
                )
            }

            SYS_FADVISE64 => {
                // todo: 这个系统调用还没有实现

                Err(SystemError::ENOSYS)
            }

            SYS_MOUNT => {
                let source = args[0] as *const u8;
                let target = args[1] as *const u8;
                let filesystemtype = args[2] as *const u8;
                let mountflags = args[3];
                let data = args[4] as *const u8; // 额外的mount参数，实现自己的mountdata来获取
                return Self::mount(source, target, filesystemtype, mountflags, data);
            }

            SYS_UMOUNT2 => {
                let target = args[0] as *const u8;
                let flags = args[1] as i32;
                Self::umount2(target, flags)?;
                return Ok(0);
            }

            #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
            SYS_NEWFSTATAT => {
                // todo: 这个系统调用还没有实现

                Err(SystemError::ENOSYS)
            }

            // SYS_SCHED_YIELD => Self::sched_yield(),
            SYS_UNAME => {
                let name = args[0] as *mut PosixOldUtsName;
                Self::uname(name)
            }
            SYS_PRCTL => {
                // todo: 这个系统调用还没有实现

                Err(SystemError::EINVAL)
            }

            #[cfg(target_arch = "x86_64")]
            SYS_ALARM => {
                let second = args[0] as u32;
                Self::alarm(second)
            }

            SYS_SHMGET => {
                let key = ShmKey::new(args[0]);
                let size = args[1];
                let shmflg = ShmFlags::from_bits_truncate(args[2] as u32);

                Self::shmget(key, size, shmflg)
            }
            SYS_SHMAT => {
                let id = ShmId::new(args[0]);
                let vaddr = VirtAddr::new(args[1]);
                let shmflg = ShmFlags::from_bits_truncate(args[2] as u32);

                Self::shmat(id, vaddr, shmflg)
            }
            SYS_SHMDT => {
                let vaddr = VirtAddr::new(args[0]);
                Self::shmdt(vaddr)
            }
            SYS_SHMCTL => {
                let id = ShmId::new(args[0]);
                let cmd = ShmCtlCmd::from(args[1]);
                let user_buf = args[2] as *const u8;
                let from_user = frame.is_from_user();

                Self::shmctl(id, cmd, user_buf, from_user)
            }
            SYS_MSYNC => {
                let start = page_align_up(args[0]);
                let len = page_align_up(args[1]);
                let flags = args[2];
                Self::msync(VirtAddr::new(start), len, flags)
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
            SYS_UNSHARE => Self::sys_unshare(args[0] as u64),
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
            SYS_RESTART_SYSCALL => Self::restart_syscall(),
            SYS_RT_SIGPENDING => Self::rt_sigpending(args[0], args[1]),
            _ => panic!("Unsupported syscall ID: {}", syscall_num),
        };

        if ProcessManager::current_pcb()
            .flags()
            .contains(ProcessFlags::NEED_SCHEDULE)
        {
            schedule(SchedMode::SM_PREEMPT);
        }

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
