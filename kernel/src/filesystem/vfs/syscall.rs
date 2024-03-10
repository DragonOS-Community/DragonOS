use core::ffi::CStr;

use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    driver::base::{block::SeekFrom, device::device_number::DeviceNumber},
    filesystem::vfs::file::FileDescriptorVec,
    kerror,
    libs::rwlock::RwLockWriteGuard,
    mm::{verify_area, VirtAddr},
    process::ProcessManager,
    syscall::{
        user_access::{check_and_clone_cstr, UserBufferReader, UserBufferWriter},
        Syscall,
    },
    time::TimeSpec,
};

use super::{
    core::{do_mkdir, do_remove_dir, do_unlink_at},
    fcntl::{AtFlags, FcntlCommand, FD_CLOEXEC},
    file::{File, FileMode},
    open::{do_faccessat, do_fchmodat, do_sys_open},
    utils::{rsplit_path, user_path_at},
    Dirent, FileType, IndexNode, MAX_PATHLEN, ROOT_INODE, VFS_MAX_FOLLOW_SYMLINK_TIMES,
};
// use crate::kdebug;

pub const SEEK_SET: u32 = 0;
pub const SEEK_CUR: u32 = 1;
pub const SEEK_END: u32 = 2;
pub const SEEK_MAX: u32 = 3;

bitflags! {
    /// 文件类型和权限
    #[repr(C)]
    pub struct ModeType: u32 {
        /// 掩码
        const S_IFMT = 0o0_170_000;
        /// 文件类型
        const S_IFSOCK = 0o140000;
        const S_IFLNK = 0o120000;
        const S_IFREG = 0o100000;
        const S_IFBLK = 0o060000;
        const S_IFDIR = 0o040000;
        const S_IFCHR = 0o020000;
        const S_IFIFO = 0o010000;

        const S_ISUID = 0o004000;
        const S_ISGID = 0o002000;
        const S_ISVTX = 0o001000;
        /// 文件用户权限
        const S_IRWXU = 0o0700;
        const S_IRUSR = 0o0400;
        const S_IWUSR = 0o0200;
        const S_IXUSR = 0o0100;
        /// 文件组权限
        const S_IRWXG = 0o0070;
        const S_IRGRP = 0o0040;
        const S_IWGRP = 0o0020;
        const S_IXGRP = 0o0010;
        /// 文件其他用户权限
        const S_IRWXO = 0o0007;
        const S_IROTH = 0o0004;
        const S_IWOTH = 0o0002;
        const S_IXOTH = 0o0001;

        /// 0o777
        const S_IRWXUGO = Self::S_IRWXU.bits | Self::S_IRWXG.bits | Self::S_IRWXO.bits;
        /// 0o7777
        const S_IALLUGO = Self::S_ISUID.bits | Self::S_ISGID.bits | Self::S_ISVTX.bits| Self::S_IRWXUGO.bits;
        /// 0o444
        const S_IRUGO = Self::S_IRUSR.bits | Self::S_IRGRP.bits | Self::S_IROTH.bits;
        /// 0o222
        const S_IWUGO = Self::S_IWUSR.bits | Self::S_IWGRP.bits | Self::S_IWOTH.bits;
        /// 0o111
        const S_IXUGO = Self::S_IXUSR.bits | Self::S_IXGRP.bits | Self::S_IXOTH.bits;


    }
}

#[repr(C)]
/// # 文件信息结构体
pub struct PosixKstat {
    /// 硬件设备ID
    dev_id: u64,
    /// inode号
    inode: u64,
    /// 硬链接数
    nlink: u64,
    /// 文件权限
    mode: ModeType,
    /// 所有者用户ID
    uid: i32,
    /// 所有者组ID
    gid: i32,
    /// 设备ID
    rdev: i64,
    /// 文件大小
    size: i64,
    /// 文件系统块大小
    blcok_size: i64,
    /// 分配的512B块数
    blocks: u64,
    /// 最后访问时间
    atime: TimeSpec,
    /// 最后修改时间
    mtime: TimeSpec,
    /// 最后状态变化时间
    ctime: TimeSpec,
    /// 用于填充结构体大小的空白数据
    pub _pad: [i8; 24],
}
impl PosixKstat {
    fn new() -> Self {
        Self {
            inode: 0,
            dev_id: 0,
            mode: ModeType { bits: 0 },
            nlink: 0,
            uid: 0,
            gid: 0,
            rdev: 0,
            size: 0,
            atime: TimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            mtime: TimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            ctime: TimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            blcok_size: 0,
            blocks: 0,
            _pad: Default::default(),
        }
    }
}

///
///  Arguments for how openat2(2) should open the target path. If only @flags and
///  @mode are non-zero, then openat2(2) operates very similarly to openat(2).
///
///  However, unlike openat(2), unknown or invalid bits in @flags result in
///  -EINVAL rather than being silently ignored. @mode must be zero unless one of
///  {O_CREAT, O_TMPFILE} are set.
///
/// ## 成员变量
///
/// - flags: O_* flags.
/// - mode: O_CREAT/O_TMPFILE file mode.
/// - resolve: RESOLVE_* flags.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct PosixOpenHow {
    pub flags: u64,
    pub mode: u64,
    pub resolve: u64,
}

impl PosixOpenHow {
    #[allow(dead_code)]
    pub fn new(flags: u64, mode: u64, resolve: u64) -> Self {
        Self {
            flags,
            mode,
            resolve,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct OpenHow {
    pub o_flags: FileMode,
    pub mode: ModeType,
    pub resolve: OpenHowResolve,
}

impl OpenHow {
    pub fn new(mut o_flags: FileMode, mut mode: ModeType, resolve: OpenHowResolve) -> Self {
        if !o_flags.contains(FileMode::O_CREAT) {
            mode = ModeType::empty();
        }

        if o_flags.contains(FileMode::O_PATH) {
            o_flags = o_flags.intersection(FileMode::O_PATH_FLAGS);
        }

        Self {
            o_flags,
            mode,
            resolve,
        }
    }
}

impl From<PosixOpenHow> for OpenHow {
    fn from(posix_open_how: PosixOpenHow) -> Self {
        let o_flags = FileMode::from_bits_truncate(posix_open_how.flags as u32);
        let mode = ModeType::from_bits_truncate(posix_open_how.mode as u32);
        let resolve = OpenHowResolve::from_bits_truncate(posix_open_how.resolve as u64);
        return Self::new(o_flags, mode, resolve);
    }
}

bitflags! {
    pub struct OpenHowResolve: u64{
        /// Block mount-point crossings
        ///     (including bind-mounts).
        const RESOLVE_NO_XDEV = 0x01;

        /// Block traversal through procfs-style
        ///     "magic-links"
        const RESOLVE_NO_MAGICLINKS = 0x02;

        /// Block traversal through all symlinks
        ///     (implies OEXT_NO_MAGICLINKS)
        const RESOLVE_NO_SYMLINKS = 0x04;
        /// Block "lexical" trickery like
        ///     "..", symlinks, and absolute
        const RESOLVE_BENEATH = 0x08;
        /// Make all jumps to "/" and ".."
        ///     be scoped inside the dirfd
        ///     (similar to chroot(2)).
        const RESOLVE_IN_ROOT = 0x10;
        // Only complete if resolution can be
        // 			completed through cached lookup. May
        // 			return -EAGAIN if that's not
        // 			possible.
        const RESOLVE_CACHED = 0x20;
    }
}
impl Syscall {
    /// @brief 为当前进程打开一个文件
    ///
    /// @param path 文件路径
    /// @param o_flags 打开文件的标志位
    ///
    /// @return 文件描述符编号，或者是错误码
    pub fn open(
        path: &str,
        flags: FileMode,
        mode: ModeType,
        follow_symlink: bool,
    ) -> Result<usize, SystemError> {
        return do_sys_open(AtFlags::AT_FDCWD.bits(), path, flags, mode, follow_symlink);
    }

    pub fn openat(
        dirfd: i32,
        path: &str,
        o_flags: FileMode,
        mode: ModeType,
        follow_symlink: bool,
    ) -> Result<usize, SystemError> {
        return do_sys_open(dirfd, path, o_flags, mode, follow_symlink);
    }

    /// @brief 关闭文件
    ///
    /// @param fd 文件描述符编号
    ///
    /// @return 成功返回0，失败返回错误码
    pub fn close(fd: usize) -> Result<usize, SystemError> {
        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();

        let res = fd_table_guard.drop_fd(fd as i32).map(|_| 0);

        return res;
    }

    /// @brief 发送命令到文件描述符对应的设备，
    ///
    /// @param fd 文件描述符编号
    /// @param cmd 设备相关的请求类型
    ///
    /// @return Ok(usize) 成功返回0
    /// @return Err(SystemError) 读取失败，返回posix错误码
    pub fn ioctl(fd: usize, cmd: u32, data: usize) -> Result<usize, SystemError> {
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();

        let file = fd_table_guard
            .get_file_by_fd(fd as i32)
            .ok_or(SystemError::EBADF)?;

        // drop guard 以避免无法调度的问题
        drop(fd_table_guard);
        let file = file.lock_no_preempt();
        let r = file.inode().ioctl(cmd, data, &file.private_data);
        return r;
    }

    /// @brief 根据文件描述符，读取文件数据。尝试读取的数据长度与buf的长度相同。
    ///
    /// @param fd 文件描述符编号
    /// @param buf 输出缓冲区
    ///
    /// @return Ok(usize) 成功读取的数据的字节数
    /// @return Err(SystemError) 读取失败，返回posix错误码
    pub fn read(fd: i32, buf: &mut [u8]) -> Result<usize, SystemError> {
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();

        let file = fd_table_guard.get_file_by_fd(fd);
        if file.is_none() {
            return Err(SystemError::EBADF);
        }
        // drop guard 以避免无法调度的问题
        drop(fd_table_guard);
        let file = file.unwrap();

        return file.lock_no_preempt().read(buf.len(), buf);
    }

    /// @brief 根据文件描述符，向文件写入数据。尝试写入的数据长度与buf的长度相同。
    ///
    /// @param fd 文件描述符编号
    /// @param buf 输入缓冲区
    ///
    /// @return Ok(usize) 成功写入的数据的字节数
    /// @return Err(SystemError) 写入失败，返回posix错误码
    pub fn write(fd: i32, buf: &[u8]) -> Result<usize, SystemError> {
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();

        let file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;

        // drop guard 以避免无法调度的问题
        drop(fd_table_guard);
        return file.lock_no_preempt().write(buf.len(), buf);
    }

    /// @brief 调整文件操作指针的位置
    ///
    /// @param fd 文件描述符编号
    /// @param seek 调整的方式
    ///
    /// @return Ok(usize) 调整后，文件访问指针相对于文件头部的偏移量
    /// @return Err(SystemError) 调整失败，返回posix错误码
    pub fn lseek(fd: i32, seek: SeekFrom) -> Result<usize, SystemError> {
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        let file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;

        // drop guard 以避免无法调度的问题
        drop(fd_table_guard);
        return file.lock_no_preempt().lseek(seek);
    }

    /// # sys_pread64 系统调用的实际执行函数
    ///
    /// ## 参数
    /// - `fd`: 文件描述符
    /// - `buf`: 读出缓冲区
    /// - `len`: 要读取的字节数
    /// - `offset`: 文件偏移量
    pub fn pread(fd: i32, buf: &mut [u8], len: usize, offset: usize) -> Result<usize, SystemError> {
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();

        let file = fd_table_guard.get_file_by_fd(fd);
        if file.is_none() {
            return Err(SystemError::EBADF);
        }
        // drop guard 以避免无法调度的问题
        drop(fd_table_guard);
        let file = file.unwrap();

        return file.lock_no_preempt().pread(offset, len, buf);
    }

    /// # sys_pwrite64 系统调用的实际执行函数
    ///
    /// ## 参数
    /// - `fd`: 文件描述符
    /// - `buf`: 写入缓冲区
    /// - `len`: 要写入的字节数
    /// - `offset`: 文件偏移量
    pub fn pwrite(fd: i32, buf: &[u8], len: usize, offset: usize) -> Result<usize, SystemError> {
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();

        let file = fd_table_guard.get_file_by_fd(fd);
        if file.is_none() {
            return Err(SystemError::EBADF);
        }
        // drop guard 以避免无法调度的问题
        drop(fd_table_guard);
        let file = file.unwrap();

        return file.lock_no_preempt().pwrite(offset, len, buf);
    }

    /// @brief 切换工作目录
    ///
    /// @param dest_path 目标路径
    ///
    /// @return   返回码  描述  
    ///      0       |          成功  
    ///         
    ///   EACCESS    |        权限不足        
    ///
    ///    ELOOP     | 解析path时遇到路径循环
    ///
    /// ENAMETOOLONG |       路径名过长       
    ///
    ///    ENOENT    |  目标文件或目录不存在  
    ///
    ///    ENODIR    |  检索期间发现非目录项  
    ///
    ///    ENOMEM    |      系统内存不足      
    ///
    ///    EFAULT    |       错误的地址      
    ///  
    /// ENAMETOOLONG |        路径过长        
    pub fn chdir(dest_path: &str) -> Result<usize, SystemError> {
        let proc = ProcessManager::current_pcb();
        // Copy path to kernel space to avoid some security issues
        let path = dest_path.to_string();
        let mut new_path = String::from("");
        if path.len() > 0 {
            let cwd = match path.as_bytes()[0] {
                b'/' => String::from("/"),
                _ => proc.basic().cwd(),
            };
            let mut cwd_vec: Vec<_> = cwd.split("/").filter(|&x| x != "").collect();
            let path_split = path.split("/").filter(|&x| x != "");
            for seg in path_split {
                if seg == ".." {
                    cwd_vec.pop();
                } else if seg == "." {
                    // 当前目录
                } else {
                    cwd_vec.push(seg);
                }
            }
            //proc.basic().set_path(String::from(""));
            for seg in cwd_vec {
                new_path.push_str("/");
                new_path.push_str(seg);
            }
            if new_path == "" {
                new_path = String::from("/");
            }
        }
        let inode =
            match ROOT_INODE().lookup_follow_symlink(&new_path, VFS_MAX_FOLLOW_SYMLINK_TIMES) {
                Err(e) => {
                    kerror!("Change Directory Failed, Error = {:?}", e);
                    return Err(SystemError::ENOENT);
                }
                Ok(i) => i,
            };
        let metadata = inode.metadata()?;
        if metadata.file_type == FileType::Dir {
            proc.basic_mut().set_cwd(String::from(new_path));
            return Ok(0);
        } else {
            return Err(SystemError::ENOTDIR);
        }
    }

    /// @brief 获取当前进程的工作目录路径
    ///
    /// @param buf 指向缓冲区的指针
    /// @param size 缓冲区的大小
    ///
    /// @return 成功，返回的指针指向包含工作目录路径的字符串
    /// @return 错误，没有足够的空间
    pub fn getcwd(buf: &mut [u8]) -> Result<VirtAddr, SystemError> {
        let proc = ProcessManager::current_pcb();
        let cwd = proc.basic().cwd();

        let cwd_bytes = cwd.as_bytes();
        let cwd_len = cwd_bytes.len();
        if cwd_len + 1 > buf.len() {
            return Err(SystemError::ENOMEM);
        }
        buf[..cwd_len].copy_from_slice(cwd_bytes);
        buf[cwd_len] = 0;

        return Ok(VirtAddr::new(buf.as_ptr() as usize));
    }

    /// @brief 获取目录中的数据
    ///
    /// TODO: 这个函数的语义与Linux不一致，需要修改！！！
    ///
    /// @param fd 文件描述符号
    /// @param buf 输出缓冲区
    ///
    /// @return 成功返回读取的字节数，失败返回错误码
    pub fn getdents(fd: i32, buf: &mut [u8]) -> Result<usize, SystemError> {
        let dirent =
            unsafe { (buf.as_mut_ptr() as *mut Dirent).as_mut() }.ok_or(SystemError::EFAULT)?;

        if fd < 0 || fd as usize > FileDescriptorVec::PROCESS_MAX_FD {
            return Err(SystemError::EBADF);
        }

        // 获取fd
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        let file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;

        // drop guard 以避免无法调度的问题
        drop(fd_table_guard);

        let res = file.lock_no_preempt().readdir(dirent).map(|x| x as usize);

        return res;
    }

    /// @brief 创建文件夹
    ///
    /// @param path(r8) 路径 / mode(r9) 模式
    ///
    /// @return uint64_t 负数错误码 / 0表示成功
    pub fn mkdir(path: &str, mode: usize) -> Result<usize, SystemError> {
        return do_mkdir(path, FileMode::from_bits_truncate(mode as u32)).map(|x| x as usize);
    }

    /// **删除文件夹、取消文件的链接、删除文件的系统调用**
    ///
    /// ## 参数
    ///
    /// - `dirfd`：文件夹的文件描述符.目前暂未实现
    /// - `pathname`：文件夹的路径
    /// - `flags`：标志位
    ///
    ///
    pub fn unlinkat(dirfd: i32, pathname: &str, flags: u32) -> Result<usize, SystemError> {
        let flags = AtFlags::from_bits(flags as i32).ok_or(SystemError::EINVAL)?;

        if flags.contains(AtFlags::AT_REMOVEDIR) {
            // kdebug!("rmdir");
            match do_remove_dir(dirfd, &pathname) {
                Err(err) => {
                    kerror!("Failed to Remove Directory, Error Code = {:?}", err);
                    return Err(err);
                }
                Ok(_) => {
                    return Ok(0);
                }
            }
        }

        match do_unlink_at(dirfd, &pathname) {
            Err(err) => {
                kerror!("Failed to Remove Directory, Error Code = {:?}", err);
                return Err(err);
            }
            Ok(_) => {
                return Ok(0);
            }
        }
    }

    pub fn rmdir(pathname: *const u8) -> Result<usize, SystemError> {
        let pathname: String = check_and_clone_cstr(pathname, Some(MAX_PATHLEN))?;
        if pathname.len() >= MAX_PATHLEN {
            return Err(SystemError::ENAMETOOLONG);
        }
        let pathname = pathname.as_str().trim();
        return do_remove_dir(AtFlags::AT_FDCWD.bits(), pathname).map(|v| v as usize);
    }

    pub fn unlink(pathname: *const u8) -> Result<usize, SystemError> {
        if pathname.is_null() {
            return Err(SystemError::EFAULT);
        }
        let ureader = UserBufferReader::new(pathname, MAX_PATHLEN, true)?;

        let buf: &[u8] = ureader.buffer(0).unwrap();

        let pathname: &CStr = CStr::from_bytes_until_nul(buf).map_err(|_| SystemError::EINVAL)?;

        let pathname: &str = pathname.to_str().map_err(|_| SystemError::EINVAL)?;
        if pathname.len() >= MAX_PATHLEN {
            return Err(SystemError::ENAMETOOLONG);
        }
        let pathname = pathname.trim();

        return do_unlink_at(AtFlags::AT_FDCWD.bits(), pathname).map(|v| v as usize);
    }

    /// @brief 根据提供的文件描述符的fd，复制对应的文件结构体，并返回新复制的文件结构体对应的fd
    pub fn dup(oldfd: i32) -> Result<usize, SystemError> {
        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();

        let old_file = fd_table_guard
            .get_file_by_fd(oldfd)
            .ok_or(SystemError::EBADF)?;

        let new_file = old_file
            .lock_no_preempt()
            .try_clone()
            .ok_or(SystemError::EBADF)?;
        // 申请文件描述符，并把文件对象存入其中
        let res = fd_table_guard.alloc_fd(new_file, None).map(|x| x as usize);
        return res;
    }

    /// 根据提供的文件描述符的fd，和指定新fd，复制对应的文件结构体，
    /// 并返回新复制的文件结构体对应的fd.
    /// 如果新fd已经打开，则会先关闭新fd.
    ///
    /// ## 参数
    ///
    /// - `oldfd`：旧文件描述符
    /// - `newfd`：新文件描述符
    ///
    /// ## 返回值
    ///
    /// - 成功：新文件描述符
    /// - 失败：错误码
    pub fn dup2(oldfd: i32, newfd: i32) -> Result<usize, SystemError> {
        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();
        return Self::do_dup2(oldfd, newfd, &mut fd_table_guard);
    }

    fn do_dup2(
        oldfd: i32,
        newfd: i32,
        fd_table_guard: &mut RwLockWriteGuard<'_, FileDescriptorVec>,
    ) -> Result<usize, SystemError> {
        // 确认oldfd, newid是否有效
        if !(FileDescriptorVec::validate_fd(oldfd) && FileDescriptorVec::validate_fd(newfd)) {
            return Err(SystemError::EBADF);
        }

        if oldfd == newfd {
            // 若oldfd与newfd相等
            return Ok(newfd as usize);
        }
        let new_exists = fd_table_guard.get_file_by_fd(newfd).is_some();
        if new_exists {
            // close newfd
            if let Err(_) = fd_table_guard.drop_fd(newfd) {
                // An I/O error occurred while attempting to close fildes2.
                return Err(SystemError::EIO);
            }
        }

        let old_file = fd_table_guard
            .get_file_by_fd(oldfd)
            .ok_or(SystemError::EBADF)?;
        let new_file = old_file
            .lock_no_preempt()
            .try_clone()
            .ok_or(SystemError::EBADF)?;
        // 申请文件描述符，并把文件对象存入其中
        let res = fd_table_guard
            .alloc_fd(new_file, Some(newfd))
            .map(|x| x as usize);
        return res;
    }

    /// # fcntl
    ///
    /// ## 参数
    ///
    /// - `fd`：文件描述符
    /// - `cmd`：命令
    /// - `arg`：参数
    pub fn fcntl(fd: i32, cmd: FcntlCommand, arg: i32) -> Result<usize, SystemError> {
        match cmd {
            FcntlCommand::DupFd => {
                if arg < 0 || arg as usize >= FileDescriptorVec::PROCESS_MAX_FD {
                    return Err(SystemError::EBADF);
                }
                let arg = arg as usize;
                for i in arg..FileDescriptorVec::PROCESS_MAX_FD {
                    let binding = ProcessManager::current_pcb().fd_table();
                    let mut fd_table_guard = binding.write();
                    if fd_table_guard.get_file_by_fd(i as i32).is_none() {
                        return Self::do_dup2(fd, i as i32, &mut fd_table_guard);
                    }
                }
                return Err(SystemError::EMFILE);
            }
            FcntlCommand::GetFd => {
                // Get file descriptor flags.
                let binding = ProcessManager::current_pcb().fd_table();
                let fd_table_guard = binding.read();
                if let Some(file) = fd_table_guard.get_file_by_fd(fd) {
                    // drop guard 以避免无法调度的问题
                    drop(fd_table_guard);

                    if file.lock().close_on_exec() {
                        return Ok(FD_CLOEXEC as usize);
                    }
                }
                return Err(SystemError::EBADF);
            }
            FcntlCommand::SetFd => {
                // Set file descriptor flags.
                let binding = ProcessManager::current_pcb().fd_table();
                let fd_table_guard = binding.write();

                if let Some(file) = fd_table_guard.get_file_by_fd(fd) {
                    // drop guard 以避免无法调度的问题
                    drop(fd_table_guard);
                    let arg = arg as u32;
                    if arg & FD_CLOEXEC != 0 {
                        file.lock().set_close_on_exec(true);
                    } else {
                        file.lock().set_close_on_exec(false);
                    }
                    return Ok(0);
                }
                return Err(SystemError::EBADF);
            }

            FcntlCommand::GetFlags => {
                // Get file status flags.
                let binding = ProcessManager::current_pcb().fd_table();
                let fd_table_guard = binding.read();

                if let Some(file) = fd_table_guard.get_file_by_fd(fd) {
                    // drop guard 以避免无法调度的问题
                    drop(fd_table_guard);
                    return Ok(file.lock_no_preempt().mode().bits() as usize);
                }

                return Err(SystemError::EBADF);
            }
            FcntlCommand::SetFlags => {
                // Set file status flags.
                let binding = ProcessManager::current_pcb().fd_table();
                let fd_table_guard = binding.write();

                if let Some(file) = fd_table_guard.get_file_by_fd(fd) {
                    let arg = arg as u32;
                    let mode = FileMode::from_bits(arg).ok_or(SystemError::EINVAL)?;
                    // drop guard 以避免无法调度的问题
                    drop(fd_table_guard);
                    file.lock_no_preempt().set_mode(mode)?;
                    return Ok(0);
                }

                return Err(SystemError::EBADF);
            }
            _ => {
                // TODO: unimplemented
                // 未实现的命令，返回0，不报错。

                // kwarn!("fcntl: unimplemented command: {:?}, defaults to 0.", cmd);
                return Ok(0);
            }
        }
    }

    /// # ftruncate
    ///
    /// ## 描述
    ///
    /// 改变文件大小.
    /// 如果文件大小大于原来的大小，那么文件的内容将会被扩展到指定的大小，新的空间将会用0填充.
    /// 如果文件大小小于原来的大小，那么文件的内容将会被截断到指定的大小.
    ///
    /// ## 参数
    ///
    /// - `fd`：文件描述符
    /// - `len`：文件大小
    ///
    /// ## 返回值
    ///
    /// 如果成功，返回0，否则返回错误码.
    pub fn ftruncate(fd: i32, len: usize) -> Result<usize, SystemError> {
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();

        if let Some(file) = fd_table_guard.get_file_by_fd(fd) {
            // drop guard 以避免无法调度的问题
            drop(fd_table_guard);
            let r = file.lock_no_preempt().ftruncate(len).map(|_| 0);
            return r;
        }

        return Err(SystemError::EBADF);
    }

    fn do_fstat(fd: i32) -> Result<PosixKstat, SystemError> {
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        let file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;
        // drop guard 以避免无法调度的问题
        drop(fd_table_guard);

        let mut kstat = PosixKstat::new();
        // 获取文件信息
        let metadata = file.lock().metadata()?;
        kstat.size = metadata.size as i64;
        kstat.dev_id = metadata.dev_id as u64;
        kstat.inode = metadata.inode_id.into() as u64;
        kstat.blcok_size = metadata.blk_size as i64;
        kstat.blocks = metadata.blocks as u64;

        kstat.atime.tv_sec = metadata.atime.tv_sec;
        kstat.atime.tv_nsec = metadata.atime.tv_nsec;
        kstat.mtime.tv_sec = metadata.mtime.tv_sec;
        kstat.mtime.tv_nsec = metadata.mtime.tv_nsec;
        kstat.ctime.tv_sec = metadata.ctime.tv_sec;
        kstat.ctime.tv_nsec = metadata.ctime.tv_nsec;

        kstat.nlink = metadata.nlinks as u64;
        kstat.uid = metadata.uid as i32;
        kstat.gid = metadata.gid as i32;
        kstat.rdev = metadata.raw_dev.data() as i64;
        kstat.mode = metadata.mode;
        match file.lock().file_type() {
            FileType::File => kstat.mode.insert(ModeType::S_IFREG),
            FileType::Dir => kstat.mode.insert(ModeType::S_IFDIR),
            FileType::BlockDevice => kstat.mode.insert(ModeType::S_IFBLK),
            FileType::CharDevice => kstat.mode.insert(ModeType::S_IFCHR),
            FileType::SymLink => kstat.mode.insert(ModeType::S_IFLNK),
            FileType::Socket => kstat.mode.insert(ModeType::S_IFSOCK),
            FileType::Pipe => kstat.mode.insert(ModeType::S_IFIFO),
            FileType::KvmDevice => kstat.mode.insert(ModeType::S_IFCHR),
            FileType::FramebufferDevice => kstat.mode.insert(ModeType::S_IFCHR),
        }

        return Ok(kstat);
    }

    pub fn fstat(fd: i32, usr_kstat: *mut PosixKstat) -> Result<usize, SystemError> {
        let kstat = Self::do_fstat(fd)?;
        if usr_kstat.is_null() {
            return Err(SystemError::EFAULT);
        }
        unsafe {
            *usr_kstat = kstat;
        }
        return Ok(0);
    }

    pub fn stat(path: &str, user_kstat: *mut PosixKstat) -> Result<usize, SystemError> {
        let fd = Self::open(path, FileMode::O_RDONLY, ModeType::empty(), true)?;
        let r = Self::fstat(fd as i32, user_kstat);
        Self::close(fd).ok();
        return r;
    }

    pub fn lstat(path: &str, user_kstat: *mut PosixKstat) -> Result<usize, SystemError> {
        let fd = Self::open(path, FileMode::O_RDONLY, ModeType::empty(), false)?;
        let r = Self::fstat(fd as i32, user_kstat);
        Self::close(fd).ok();
        return r;
    }

    pub fn mknod(
        path_ptr: *const i8,
        mode: ModeType,
        dev_t: DeviceNumber,
    ) -> Result<usize, SystemError> {
        // 安全检验
        let len = unsafe { CStr::from_ptr(path_ptr).to_bytes().len() };
        let user_buffer = UserBufferReader::new(path_ptr, len, true)?;
        let buf = user_buffer.read_from_user::<u8>(0)?;
        let path = core::str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?;

        // 文件名过长
        if path.len() > MAX_PATHLEN as usize {
            return Err(SystemError::ENAMETOOLONG);
        }

        let inode: Result<Arc<dyn IndexNode>, SystemError> =
            ROOT_INODE().lookup_follow_symlink(path, VFS_MAX_FOLLOW_SYMLINK_TIMES);

        if inode.is_ok() {
            return Err(SystemError::EEXIST);
        }

        let (filename, parent_path) = rsplit_path(path);

        // 查找父目录
        let parent_inode: Arc<dyn IndexNode> = ROOT_INODE()
            .lookup_follow_symlink(parent_path.unwrap_or("/"), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
        // 创建nod
        parent_inode.mknod(filename, mode, dev_t)?;

        return Ok(0);
    }

    pub fn writev(fd: i32, iov: usize, count: usize) -> Result<usize, SystemError> {
        // IoVecs会进行用户态检验
        let iovecs = unsafe { IoVecs::from_user(iov as *const IoVec, count, false) }?;

        let data = iovecs.gather();

        Self::write(fd, &data)
    }

    pub fn readv(fd: i32, iov: usize, count: usize) -> Result<usize, SystemError> {
        // IoVecs会进行用户态检验
        let mut iovecs = unsafe { IoVecs::from_user(iov as *const IoVec, count, true) }?;

        let mut data = Vec::new();
        data.resize(iovecs.0.iter().map(|x| x.len()).sum(), 0);

        let len = Self::read(fd, &mut data)?;

        iovecs.scatter(&data[..len]);

        return Ok(len);
    }

    pub fn readlink_at(
        dirfd: i32,
        path: *const u8,
        user_buf: *mut u8,
        buf_size: usize,
    ) -> Result<usize, SystemError> {
        let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?;
        let mut user_buf = UserBufferWriter::new(user_buf, buf_size, true)?;

        if path.len() == 0 {
            return Err(SystemError::EINVAL);
        }

        let (inode, path) = user_path_at(&ProcessManager::current_pcb(), dirfd, &path)?;

        let inode = inode.lookup(path.as_str())?;
        if inode.metadata()?.file_type != FileType::SymLink {
            return Err(SystemError::EINVAL);
        }

        let ubuf = user_buf.buffer::<u8>(0).unwrap();

        let mut file = File::new(inode, FileMode::O_RDONLY)?;

        let len = file.read(buf_size, ubuf)?;

        return Ok(len);
    }

    pub fn readlink(
        path: *const u8,
        user_buf: *mut u8,
        buf_size: usize,
    ) -> Result<usize, SystemError> {
        return Self::readlink_at(AtFlags::AT_FDCWD.bits(), path, user_buf, buf_size);
    }

    pub fn access(pathname: *const u8, mode: u32) -> Result<usize, SystemError> {
        return do_faccessat(
            AtFlags::AT_FDCWD.bits(),
            pathname,
            ModeType::from_bits(mode).ok_or(SystemError::EINVAL)?,
            0,
        );
    }

    pub fn faccessat2(
        dirfd: i32,
        pathname: *const u8,
        mode: u32,
        flags: u32,
    ) -> Result<usize, SystemError> {
        return do_faccessat(
            dirfd,
            pathname,
            ModeType::from_bits(mode).ok_or(SystemError::EINVAL)?,
            flags,
        );
    }

    pub fn chmod(pathname: *const u8, mode: u32) -> Result<usize, SystemError> {
        return do_fchmodat(
            AtFlags::AT_FDCWD.bits(),
            pathname,
            ModeType::from_bits(mode).ok_or(SystemError::EINVAL)?,
        );
    }

    pub fn fchmodat(dirfd: i32, pathname: *const u8, mode: u32) -> Result<usize, SystemError> {
        return do_fchmodat(
            dirfd,
            pathname,
            ModeType::from_bits(mode).ok_or(SystemError::EINVAL)?,
        );
    }

    pub fn fchmod(fd: i32, mode: u32) -> Result<usize, SystemError> {
        let _mode = ModeType::from_bits(mode).ok_or(SystemError::EINVAL)?;
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        let _file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;

        // fchmod没完全实现，因此不修改文件的权限
        // todo: 实现fchmod
        kwarn!("fchmod not fully implemented");
        return Ok(0);
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct IoVec {
    /// 缓冲区的起始地址
    pub iov_base: *mut u8,
    /// 缓冲区的长度
    pub iov_len: usize,
}

/// 用于存储多个来自用户空间的IoVec
///
/// 由于目前内核中的文件系统还不支持分散读写，所以暂时只支持将用户空间的IoVec聚合成一个缓冲区，然后进行操作。
/// TODO：支持分散读写
#[derive(Debug)]
pub struct IoVecs(Vec<&'static mut [u8]>);

impl IoVecs {
    /// 从用户空间的IoVec中构造IoVecs
    ///
    /// @param iov 用户空间的IoVec
    /// @param iovcnt 用户空间的IoVec的数量
    /// @param readv 是否为readv系统调用
    ///
    /// @return 构造成功返回IoVecs，否则返回错误码
    pub unsafe fn from_user(
        iov: *const IoVec,
        iovcnt: usize,
        _readv: bool,
    ) -> Result<Self, SystemError> {
        // 检查iov指针所在空间是否合法
        verify_area(
            VirtAddr::new(iov as usize),
            iovcnt * core::mem::size_of::<IoVec>(),
        )
        .map_err(|_| SystemError::EFAULT)?;

        // 将用户空间的IoVec转换为引用（注意：这里的引用是静态的，因为用户空间的IoVec不会被释放）
        let iovs: &[IoVec] = core::slice::from_raw_parts(iov, iovcnt);

        let mut slices: Vec<&mut [u8]> = vec![];
        slices.reserve(iovs.len());

        for iov in iovs.iter() {
            if iov.iov_len == 0 {
                continue;
            }

            verify_area(
                VirtAddr::new(iov.iov_base as usize),
                iovcnt * core::mem::size_of::<IoVec>(),
            )
            .map_err(|_| SystemError::EFAULT)?;

            slices.push(core::slice::from_raw_parts_mut(iov.iov_base, iov.iov_len));
        }

        return Ok(Self(slices));
    }

    /// @brief 将IoVecs中的数据聚合到一个缓冲区中
    ///
    /// @return 返回聚合后的缓冲区
    pub fn gather(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        for slice in self.0.iter() {
            buf.extend_from_slice(slice);
        }
        return buf;
    }

    /// @brief 将给定的数据分散写入到IoVecs中
    pub fn scatter(&mut self, data: &[u8]) {
        let mut data: &[u8] = data;
        for slice in self.0.iter_mut() {
            let len = core::cmp::min(slice.len(), data.len());
            if len == 0 {
                continue;
            }

            slice[..len].copy_from_slice(&data[..len]);
            data = &data[len..];
        }
    }

    /// @brief 创建与IoVecs等长的缓冲区
    ///
    /// @param set_len 是否设置返回的Vec的len。
    /// 如果为true，则返回的Vec的len为所有IoVec的长度之和;
    /// 否则返回的Vec的len为0，capacity为所有IoVec的长度之和.
    ///
    /// @return 返回创建的缓冲区
    pub fn new_buf(&self, set_len: bool) -> Vec<u8> {
        let total_len: usize = self.0.iter().map(|slice| slice.len()).sum();
        let mut buf: Vec<u8> = Vec::with_capacity(total_len);

        if set_len {
            buf.resize(total_len, 0);
        }
        return buf;
    }
}
