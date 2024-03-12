use core::{
    ffi::{c_char, c_int, c_void, CStr},
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{
    arch::{ipc::signal::SigSet, syscall::nr::*},
    driver::base::device::device_number::DeviceNumber,
    libs::{futex::constant::FutexFlag, rand::GRandFlags},
    mm::syscall::MremapFlags,
    net::syscall::MsgHdr,
    process::{
        fork::KernelCloneArgs,
        resource::{RLimit64, RUsage},
        ProcessManager,
    },
    syscall::user_access::check_and_clone_cstr,
};

use num_traits::FromPrimitive;
use system_error::SystemError;

use crate::{
    arch::{cpu::cpu_reset, interrupt::TrapFrame, MMArch},
    driver::base::block::SeekFrom,
    filesystem::vfs::{
        fcntl::FcntlCommand,
        file::FileMode,
        syscall::{ModeType, PosixKstat, SEEK_CUR, SEEK_END, SEEK_MAX, SEEK_SET},
        MAX_PATHLEN,
    },
    include::bindings::bindings::{PAGE_2M_SIZE, PAGE_4K_SIZE},
    kinfo,
    libs::align::page_align_up,
    mm::{verify_area, MemoryManagementArch, VirtAddr},
    net::syscall::SockAddr,
    process::{fork::CloneFlags, Pid},
    time::{
        syscall::{PosixTimeZone, PosixTimeval},
        TimeSpec,
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
        kinfo!("Initializing syscall...");
        let r = crate::arch::syscall::arch_syscall_init();
        kinfo!("Syscall init successfully!");

        return r;
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
                let path: &CStr = unsafe { CStr::from_ptr(args[0] as *const c_char) };
                let path: Result<&str, core::str::Utf8Error> = path.to_str();
                let res = if path.is_err() {
                    Err(SystemError::EINVAL)
                } else {
                    let path: &str = path.unwrap();

                    let flags = args[1];
                    let mode = args[2];

                    let open_flags: FileMode = FileMode::from_bits_truncate(flags as u32);
                    let mode = ModeType::from_bits(mode as u32).ok_or(SystemError::EINVAL)?;
                    Self::open(path, open_flags, mode, true)
                };
                res
            }

            SYS_OPENAT => {
                let dirfd = args[0] as i32;
                let path: &CStr = unsafe { CStr::from_ptr(args[1] as *const c_char) };
                let flags = args[2];
                let mode = args[3];

                let path: Result<&str, core::str::Utf8Error> = path.to_str();
                let res = if path.is_err() {
                    Err(SystemError::EINVAL)
                } else {
                    let path: &str = path.unwrap();

                    let open_flags: FileMode =
                        FileMode::from_bits(flags as u32).ok_or(SystemError::EINVAL)?;
                    let mode = ModeType::from_bits(mode as u32).ok_or(SystemError::EINVAL)?;
                    Self::openat(dirfd, path, open_flags, mode, true)
                };
                res
            }
            SYS_CLOSE => {
                let fd = args[0];
                Self::close(fd)
            }
            SYS_READ => {
                let fd = args[0] as i32;
                let buf_vaddr = args[1];
                let len = args[2];
                let from_user = frame.from_user();
                let mut user_buffer_writer =
                    UserBufferWriter::new(buf_vaddr as *mut u8, len, from_user)?;

                let user_buf = user_buffer_writer.buffer(0)?;
                Self::read(fd, user_buf)
            }
            SYS_WRITE => {
                let fd = args[0] as i32;
                let buf_vaddr = args[1];
                let len = args[2];
                let from_user = frame.from_user();
                let user_buffer_reader =
                    UserBufferReader::new(buf_vaddr as *const u8, len, from_user)?;

                let user_buf = user_buffer_reader.read_from_user(0)?;
                Self::write(fd, user_buf)
            }

            SYS_LSEEK => {
                let fd = args[0] as i32;
                let offset = args[1] as i64;
                let whence = args[2] as u32;

                let w = match whence {
                    SEEK_SET => Ok(SeekFrom::SeekSet(offset)),
                    SEEK_CUR => Ok(SeekFrom::SeekCurrent(offset)),
                    SEEK_END => Ok(SeekFrom::SeekEnd(offset)),
                    SEEK_MAX => Ok(SeekFrom::SeekEnd(0)),
                    _ => Err(SystemError::EINVAL),
                }?;

                Self::lseek(fd, w)
            }

            SYS_PREAD64 => {
                let fd = args[0] as i32;
                let buf_vaddr = args[1];
                let len = args[2];
                let offset = args[3];

                let mut user_buffer_writer =
                    UserBufferWriter::new(buf_vaddr as *mut u8, len, frame.from_user())?;
                let buf = user_buffer_writer.buffer(0)?;
                Self::pread(fd, buf, len, offset)
            }

            SYS_PWRITE64 => {
                let fd = args[0] as i32;
                let buf_vaddr = args[1];
                let len = args[2];
                let offset = args[3];

                let user_buffer_reader =
                    UserBufferReader::new(buf_vaddr as *const u8, len, frame.from_user())?;

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

            SYS_REBOOT => Self::reboot(),

            SYS_CHDIR => {
                // Closure for checking arguments
                let chdir_check = |arg0: usize| {
                    if arg0 == 0 {
                        return Err(SystemError::EFAULT);
                    }
                    let path_ptr = arg0 as *const c_char;
                    let virt_addr = VirtAddr::new(path_ptr as usize);
                    // 权限校验
                    if path_ptr.is_null()
                        || (frame.from_user()
                            && verify_area(virt_addr, PAGE_2M_SIZE as usize).is_err())
                    {
                        return Err(SystemError::EINVAL);
                    }
                    let dest_path: &CStr = unsafe { CStr::from_ptr(path_ptr) };
                    let dest_path: &str = dest_path.to_str().map_err(|_| SystemError::EINVAL)?;
                    if dest_path.len() == 0 {
                        return Err(SystemError::EINVAL);
                    } else if dest_path.len() > MAX_PATHLEN as usize {
                        return Err(SystemError::ENAMETOOLONG);
                    }

                    return Ok(dest_path);
                };

                let r = chdir_check(args[0])?;
                Self::chdir(r)
            }

            #[allow(unreachable_patterns)]
            SYS_GETDENTS64 | SYS_GETDENTS => {
                let fd = args[0] as i32;

                let buf_vaddr = args[1];
                let len = args[2];
                let virt_addr: VirtAddr = VirtAddr::new(buf_vaddr);
                // 判断缓冲区是否来自用户态，进行权限校验
                let res = if frame.from_user() && verify_area(virt_addr, len as usize).is_err() {
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
                if frame.from_user()
                    && (verify_area(virt_path_ptr, MAX_PATHLEN as usize).is_err()
                        || verify_area(virt_argv_ptr, PAGE_4K_SIZE as usize).is_err())
                    || verify_area(virt_env_ptr, PAGE_4K_SIZE as usize).is_err()
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
                let path_ptr = args[0] as *const c_char;
                let mode = args[1];
                let virt_path_ptr = VirtAddr::new(path_ptr as usize);
                let security_check = || {
                    if path_ptr.is_null()
                        || (frame.from_user()
                            && verify_area(virt_path_ptr, PAGE_2M_SIZE as usize).is_err())
                    {
                        return Err(SystemError::EINVAL);
                    }
                    let path: &CStr = unsafe { CStr::from_ptr(path_ptr) };
                    let path: &str = path.to_str().map_err(|_| SystemError::EINVAL)?.trim();

                    if path == "" {
                        return Err(SystemError::EINVAL);
                    }
                    return Ok(path);
                };

                let path = security_check();
                if path.is_err() {
                    Err(path.unwrap_err())
                } else {
                    Self::mkdir(path.unwrap(), mode)
                }
            }

            SYS_NANOSLEEP => {
                let req = args[0] as *const TimeSpec;
                let rem = args[1] as *mut TimeSpec;
                let virt_req = VirtAddr::new(req as usize);
                let virt_rem = VirtAddr::new(rem as usize);
                if frame.from_user()
                    && (verify_area(virt_req, core::mem::size_of::<TimeSpec>() as usize).is_err()
                        || verify_area(virt_rem, core::mem::size_of::<TimeSpec>() as usize)
                            .is_err())
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
                let pathname = args[1] as *const c_char;
                let flags = args[2] as u32;
                let virt_pathname = VirtAddr::new(pathname as usize);
                if frame.from_user() && verify_area(virt_pathname, PAGE_4K_SIZE as usize).is_err() {
                    Err(SystemError::EFAULT)
                } else if pathname.is_null() {
                    Err(SystemError::EFAULT)
                } else {
                    let get_path = || {
                        let pathname: &CStr = unsafe { CStr::from_ptr(pathname) };

                        let pathname: &str = pathname.to_str().map_err(|_| SystemError::EINVAL)?;
                        if pathname.len() >= MAX_PATHLEN {
                            return Err(SystemError::ENAMETOOLONG);
                        }
                        return Ok(pathname.trim());
                    };
                    let pathname = get_path();
                    if pathname.is_err() {
                        Err(pathname.unwrap_err())
                    } else {
                        // kdebug!("sys unlinkat: dirfd: {}, pathname: {}", dirfd, pathname.as_ref().unwrap());
                        Self::unlinkat(dirfd, pathname.unwrap(), flags)
                    }
                }
            }

            #[cfg(target_arch = "x86_64")]
            SYS_RMDIR => {
                let pathname = args[0] as *const u8;
                Self::rmdir(pathname)
            }

            #[cfg(target_arch = "x86_64")]
            SYS_UNLINK => {
                let pathname = args[0] as *const u8;
                Self::unlink(pathname)
            }
            SYS_KILL => {
                let pid = Pid::new(args[0]);
                let sig = args[1] as c_int;
                // kdebug!("KILL SYSCALL RECEIVED");
                Self::kill(pid, sig)
            }

            SYS_RT_SIGACTION => {
                let sig = args[0] as c_int;
                let act = args[1];
                let old_act = args[2];
                Self::sigaction(sig, act, old_act, frame.from_user())
            }

            SYS_GETPID => Self::getpid().map(|pid| pid.into()),

            SYS_SCHED => Self::sched(frame.from_user()),
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

            SYS_SOCKET => Self::socket(args[0], args[1], args[2]),
            SYS_SETSOCKOPT => {
                let optval = args[3] as *const u8;
                let optlen = args[4] as usize;
                let virt_optval = VirtAddr::new(optval as usize);
                // 验证optval的地址是否合法
                if verify_area(virt_optval, optlen as usize).is_err() {
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
                    if verify_area(virt_optval, PAGE_4K_SIZE as usize).is_err() {
                        // 地址空间超出了用户空间的范围，不合法
                        return Err(SystemError::EFAULT);
                    }

                    // 验证optlen的地址是否合法
                    if verify_area(virt_optlen, core::mem::size_of::<u32>() as usize).is_err() {
                        // 地址空间超出了用户空间的范围，不合法
                        return Err(SystemError::EFAULT);
                    }
                    return Ok(());
                };
                let r = security_check();
                if r.is_err() {
                    Err(r.unwrap_err())
                } else {
                    Self::getsockopt(args[0], args[1], args[2], optval, optlen as *mut u32)
                }
            }

            SYS_CONNECT => {
                let addr = args[1] as *const SockAddr;
                let addrlen = args[2] as usize;
                let virt_addr = VirtAddr::new(addr as usize);
                // 验证addr的地址是否合法
                if verify_area(virt_addr, addrlen as usize).is_err() {
                    // 地址空间超出了用户空间的范围，不合法
                    Err(SystemError::EFAULT)
                } else {
                    Self::connect(args[0], addr, addrlen)
                }
            }
            SYS_BIND => {
                let addr = args[1] as *const SockAddr;
                let addrlen = args[2] as usize;
                let virt_addr = VirtAddr::new(addr as usize);
                // 验证addr的地址是否合法
                if verify_area(virt_addr, addrlen as usize).is_err() {
                    // 地址空间超出了用户空间的范围，不合法
                    Err(SystemError::EFAULT)
                } else {
                    Self::bind(args[0], addr, addrlen)
                }
            }

            SYS_SENDTO => {
                let buf = args[1] as *const u8;
                let len = args[2] as usize;
                let flags = args[3] as u32;
                let addr = args[4] as *const SockAddr;
                let addrlen = args[5] as usize;
                let virt_buf = VirtAddr::new(buf as usize);
                let virt_addr = VirtAddr::new(addr as usize);
                // 验证buf的地址是否合法
                if verify_area(virt_buf, len as usize).is_err() {
                    // 地址空间超出了用户空间的范围，不合法
                    Err(SystemError::EFAULT)
                } else if verify_area(virt_addr, addrlen as usize).is_err() {
                    // 地址空间超出了用户空间的范围，不合法
                    Err(SystemError::EFAULT)
                } else {
                    let data: &[u8] = unsafe { core::slice::from_raw_parts(buf, len) };
                    Self::sendto(args[0], data, flags, addr, addrlen)
                }
            }

            SYS_RECVFROM => {
                let buf = args[1] as *mut u8;
                let len = args[2] as usize;
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
                if r.is_err() {
                    Err(r.unwrap_err())
                } else {
                    let buf = unsafe { core::slice::from_raw_parts_mut(buf, len) };
                    Self::recvfrom(args[0], buf, flags, addr, addrlen as *mut u32)
                }
            }

            SYS_RECVMSG => {
                let msg = args[1] as *mut MsgHdr;
                let flags = args[2] as u32;

                let mut user_buffer_writer =
                    UserBufferWriter::new(msg, core::mem::size_of::<MsgHdr>(), frame.from_user())?;
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
                if r.is_err() {
                    Err(r.unwrap_err())
                } else {
                    let buf = unsafe { core::slice::from_raw_parts_mut(buf, size) };
                    Self::getcwd(buf).map(|ptr| ptr.data())
                }
            }

            SYS_GETPGID => Self::getpgid(Pid::new(args[0])).map(|pid| pid.into()),

            SYS_GETPPID => Self::getppid().map(|pid| pid.into()),
            SYS_FSTAT => {
                let fd = args[0] as i32;
                let kstat = args[1] as *mut PosixKstat;
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

                // kdebug!("FCNTL: fd: {}, cmd: {:?}, arg: {}, res: {:?}", fd, cmd, arg, res);
                res
            }

            SYS_FTRUNCATE => {
                let fd = args[0] as i32;
                let len = args[1];
                let res = Self::ftruncate(fd, len);
                // kdebug!("FTRUNCATE: fd: {}, len: {}, res: {:?}", fd, len, res);
                res
            }

            #[cfg(target_arch = "x86_64")]
            SYS_MKNOD => {
                let path = args[0];
                let flags = args[1];
                let dev_t = args[2];
                let flags: ModeType = ModeType::from_bits_truncate(flags as u32);
                Self::mknod(path as *const i8, flags, DeviceNumber::from(dev_t as u32))
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

                verify_area(uaddr, core::mem::size_of::<u32>())?;
                verify_area(uaddr2, core::mem::size_of::<u32>())?;

                let mut timespec = None;
                if utime != 0 && operation.contains(FutexFlag::FLAGS_HAS_TIMEOUT) {
                    let reader = UserBufferReader::new(
                        utime as *const TimeSpec,
                        core::mem::size_of::<TimeSpec>(),
                        true,
                    )?;

                    timespec = Some(*reader.read_one_from_user::<TimeSpec>(0)?);
                }

                Self::do_futex(uaddr, operation, val, timespec, uaddr2, utime as u32, val3)
            }

            SYS_READV => Self::readv(args[0] as i32, args[1], args[2]),
            SYS_WRITEV => Self::writev(args[0] as i32, args[1], args[2]),

            SYS_SET_TID_ADDRESS => Self::set_tid_address(args[0]),

            #[cfg(target_arch = "x86_64")]
            SYS_LSTAT => {
                let path: &CStr = unsafe { CStr::from_ptr(args[0] as *const c_char) };
                let path: Result<&str, core::str::Utf8Error> = path.to_str();
                let res = if path.is_err() {
                    Err(SystemError::EINVAL)
                } else {
                    let path: &str = path.unwrap();
                    let kstat = args[1] as *mut PosixKstat;
                    let vaddr = VirtAddr::new(kstat as usize);
                    match verify_area(vaddr, core::mem::size_of::<PosixKstat>()) {
                        Ok(_) => Self::lstat(path, kstat),
                        Err(e) => Err(e),
                    }
                };

                res
            }

            #[cfg(target_arch = "x86_64")]
            SYS_STAT => {
                let path: &CStr = unsafe { CStr::from_ptr(args[0] as *const c_char) };
                let path: Result<&str, core::str::Utf8Error> = path.to_str();
                let res = if path.is_err() {
                    Err(SystemError::EINVAL)
                } else {
                    let path: &str = path.unwrap();
                    let kstat = args[1] as *mut PosixKstat;
                    let vaddr = VirtAddr::new(kstat as usize);
                    match verify_area(vaddr, core::mem::size_of::<PosixKstat>()) {
                        Ok(_) => Self::stat(path, kstat),
                        Err(e) => Err(e),
                    }
                };

                res
            }

            SYS_EPOLL_CREATE => Self::epoll_create(args[0] as i32),
            SYS_EPOLL_CREATE1 => Self::epoll_create1(args[0]),

            SYS_EPOLL_CTL => Self::epoll_ctl(
                args[0] as i32,
                args[1],
                args[2] as i32,
                VirtAddr::new(args[3]),
            ),

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
                    frame.from_user(),
                )?;
                let fds = user_buffer_writer.buffer::<i32>(0)?;
                Self::socketpair(args[0], args[1], args[2], fds)
            }

            #[cfg(target_arch = "x86_64")]
            SYS_POLL => {
                kwarn!("SYS_POLL has not yet been implemented");
                Ok(0)
            }

            SYS_SETPGID => {
                kwarn!("SYS_SETPGID has not yet been implemented");
                Ok(0)
            }

            SYS_RT_SIGPROCMASK => {
                kwarn!("SYS_RT_SIGPROCMASK has not yet been implemented");
                Ok(0)
            }

            SYS_TKILL => {
                kwarn!("SYS_TKILL has not yet been implemented");
                Ok(0)
            }

            SYS_SIGALTSTACK => {
                kwarn!("SYS_SIGALTSTACK has not yet been implemented");
                Ok(0)
            }

            SYS_EXIT_GROUP => {
                kwarn!("SYS_EXIT_GROUP has not yet been implemented");
                Ok(0)
            }

            SYS_MADVISE => {
                // 这个太吵了，总是打印，先注释掉
                // kwarn!("SYS_MADVISE has not yet been implemented");
                Ok(0)
            }
            SYS_GETTID => Self::gettid().map(|tid| tid.into()),
            SYS_GETUID => Self::getuid(),

            SYS_SYSLOG => {
                let syslog_action_type = args[0];
                let buf_vaddr = args[1];
                let len = args[2];
                let from_user = frame.from_user();
                let mut user_buffer_writer =
                    UserBufferWriter::new(buf_vaddr as *mut u8, len, from_user)?;

                let user_buf = user_buffer_writer.buffer(0)?;
                Self::do_syslog(syslog_action_type, user_buf, len)
            }

            SYS_GETGID => Self::getgid(),
            SYS_SETUID => {
                kwarn!("SYS_SETUID has not yet been implemented");
                Ok(0)
            }
            SYS_SETGID => {
                kwarn!("SYS_SETGID has not yet been implemented");
                Ok(0)
            }
            SYS_GETEUID => Self::geteuid(),
            SYS_GETEGID => Self::getegid(),
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
                let pathname = args[1] as *const u8;
                let buf = args[2] as *mut u8;
                let bufsiz = args[3];
                Self::readlink_at(dirfd, pathname, buf, bufsiz)
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
                let timespec = args[1] as *mut TimeSpec;
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
                kwarn!("SYS_FCHOWN has not yet been implemented");
                Ok(0)
            }

            SYS_FSYNC => {
                kwarn!("SYS_FSYNC has not yet been implemented");
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

            SYS_SCHED_GETAFFINITY => {
                // todo: 这个系统调用还没有实现

                Err(SystemError::ENOSYS)
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

            SYS_SCHED_YIELD => Self::sched_yield(),

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
        let s = check_and_clone_cstr(s, Some(4096))?;
        let fr = (front_color & 0x00ff0000) >> 16;
        let fg = (front_color & 0x0000ff00) >> 8;
        let fb = front_color & 0x000000ff;
        let br = (back_color & 0x00ff0000) >> 16;
        let bg = (back_color & 0x0000ff00) >> 8;
        let bb = back_color & 0x000000ff;
        print!("\x1B[38;2;{fr};{fg};{fb};48;2;{br};{bg};{bb}m{s}\x1B[0m");
        return Ok(s.len());
    }

    pub fn reboot() -> Result<usize, SystemError> {
        unsafe { cpu_reset() };
    }
}
