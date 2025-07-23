use core::mem::size_of;

use alloc::sync::Arc;

use log::warn;
use system_error::SystemError;

use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::vfs::file::FileDescriptorVec,
    libs::rwlock::RwLockWriteGuard,
    process::ProcessManager,
    syscall::{
        user_access::{check_and_clone_cstr, UserBufferWriter},
        Syscall,
    },
    time:: PosixTimeSpec,
};

use super::stat::{do_newfstatat, do_statx, vfs_fstat};
use super::{
    fcntl::{AtFlags, FcntlCommand, FD_CLOEXEC},
    file::{File, FileMode},
    open::{do_faccessat, do_fchmodat},
    utils::{rsplit_path, user_path_at},
    FileType, IndexNode, SuperBlock, MAX_PATHLEN, ROOT_INODE, VFS_MAX_FOLLOW_SYMLINK_TIMES,
};
mod link_utils;
mod open_utils;
mod rename_utils;
mod utimensat;
mod sys_chdir;
mod sys_close;
mod sys_fchdir;
mod sys_getcwd;
mod sys_getdents;
mod sys_ioctl;
mod sys_linkat;
mod sys_lseek;
mod sys_mkdirat;
mod sys_openat;
mod sys_pread64;
mod sys_pselect6;
mod sys_pwrite64;
mod sys_read;
mod sys_readv;
mod sys_renameat2;
mod sys_select;
mod sys_symlinkat;
mod sys_unlinkat;
mod sys_write;
mod sys_writev;
mod sys_utimensat;
mod sys_fchown;
mod sys_fchownat;
mod sys_fchmod;

mod epoll_utils;
mod sys_epoll_create1;
mod sys_epoll_ctl;
mod sys_epoll_pwait;

pub mod symlink_utils;
pub mod sys_mount;
pub mod sys_umount2;

#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
mod sys_fstat;

#[cfg(target_arch = "x86_64")]
mod sys_epoll_create;
#[cfg(target_arch = "x86_64")]
mod sys_epoll_wait;
#[cfg(target_arch = "x86_64")]
mod sys_link;
#[cfg(target_arch = "x86_64")]
mod sys_lstat;
#[cfg(target_arch = "x86_64")]
mod sys_mkdir;
#[cfg(target_arch = "x86_64")]
mod sys_open;
#[cfg(target_arch = "x86_64")]
mod sys_rename;
#[cfg(target_arch = "x86_64")]
mod sys_renameat;
#[cfg(target_arch = "x86_64")]
mod sys_rmdir;
#[cfg(target_arch = "x86_64")]
mod sys_stat;
#[cfg(target_arch = "x86_64")]
mod sys_symlink;
#[cfg(target_arch = "x86_64")]
mod sys_unlink;
#[cfg(target_arch = "x86_64")]
mod sys_utimes;
#[cfg(target_arch = "x86_64")]
mod sys_futimesat;
#[cfg(target_arch = "x86_64")]
mod sys_lchown;
#[cfg(target_arch = "x86_64")]
mod sys_chown;

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
#[derive(Clone, Copy)]
/// # 文件信息结构体X
pub struct PosixStatx {
    /* 0x00 */
    pub stx_mask: PosixStatxMask,
    /// 文件系统块大小
    pub stx_blksize: u32,
    /// Flags conveying information about the file [uncond]
    pub stx_attributes: StxAttributes,
    /* 0x10 */
    /// 硬链接数
    pub stx_nlink: u32,
    /// 所有者用户ID
    pub stx_uid: u32,
    /// 所有者组ID
    pub stx_gid: u32,
    /// 文件权限
    pub stx_mode: ModeType,

    /* 0x20 */
    /// inode号
    pub stx_inode: u64,
    /// 文件大小
    pub stx_size: i64,
    /// 分配的512B块数
    pub stx_blocks: u64,
    /// Mask to show what's supported in stx_attributes
    pub stx_attributes_mask: StxAttributes,

    /* 0x40 */
    /// 最后访问时间
    pub stx_atime: PosixTimeSpec,
    /// 文件创建时间
    pub stx_btime: PosixTimeSpec,
    /// 最后状态变化时间
    pub stx_ctime: PosixTimeSpec,
    /// 最后修改时间
    pub stx_mtime: PosixTimeSpec,

    /* 0x80 */
    /// 主设备ID
    pub stx_rdev_major: u32,
    /// 次设备ID
    pub stx_rdev_minor: u32,
    /// 主硬件设备ID
    pub stx_dev_major: u32,
    /// 次硬件设备ID
    pub stx_dev_minor: u32,

    /* 0x90 */
    pub stx_mnt_id: u64,
    pub stx_dio_mem_align: u32,
    pub stx_dio_offset_align: u32,
}

impl PosixStatx {
    #[inline(never)]
    pub(super) fn new() -> Self {
        Self {
            stx_mask: PosixStatxMask::STATX_BASIC_STATS,
            stx_blksize: 0,
            stx_attributes: StxAttributes::STATX_ATTR_APPEND,
            stx_nlink: 0,
            stx_uid: 0,
            stx_gid: 0,
            stx_mode: ModeType { bits: 0 },
            stx_inode: 0,
            stx_size: 0,
            stx_blocks: 0,
            stx_attributes_mask: StxAttributes::STATX_ATTR_APPEND,
            stx_atime: PosixTimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            stx_btime: PosixTimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            stx_ctime: PosixTimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            stx_mtime: PosixTimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            stx_rdev_major: 0,
            stx_rdev_minor: 0,
            stx_dev_major: 0,
            stx_dev_minor: 0,
            stx_mnt_id: 0,
            stx_dio_mem_align: 0,
            stx_dio_offset_align: 0,
        }
    }
}

bitflags! {
    pub struct PosixStatxMask: u32{
        ///  Want stx_mode & S_IFMT
        const STATX_TYPE = 0x00000001;

        /// Want stx_mode & ~S_IFMT
        const STATX_MODE = 0x00000002;

        /// Want stx_nlink
        const STATX_NLINK = 0x00000004;

        /// Want stx_uid
        const STATX_UID = 0x00000008;

        /// Want stx_gid
        const STATX_GID = 0x00000010;

        /// Want stx_atime
        const STATX_ATIME = 0x00000020;

        /// Want stx_mtime
        const STATX_MTIME = 0x00000040;

        /// Want stx_ctime
        const STATX_CTIME = 0x00000080;

        /// Want stx_ino
        const STATX_INO = 0x00000100;

        /// Want stx_size
        const STATX_SIZE = 0x00000200;

        /// Want stx_blocks
        const STATX_BLOCKS = 0x00000400;

        /// [All of the above]
        const STATX_BASIC_STATS = 0x000007ff;

        /// Want stx_btime
        const STATX_BTIME = 0x00000800;

        /// The same as STATX_BASIC_STATS | STATX_BTIME.
        /// It is deprecated and should not be used.
        const STATX_ALL = 0x00000fff;

        /// Want stx_mnt_id (since Linux 5.8)
        const STATX_MNT_ID = 0x00001000;

        /// Want stx_dio_mem_align and stx_dio_offset_align
        /// (since Linux 6.1; support varies by filesystem)
        const STATX_DIOALIGN = 0x00002000;

        /// Reserved for future struct statx expansion
        const STATX_RESERVED = 0x80000000;

        /// Want/got stx_change_attr
        const STATX_CHANGE_COOKIE = 0x40000000;
    }
}

bitflags! {
    pub struct StxAttributes: u64 {
        /// 文件被文件系统压缩
        const STATX_ATTR_COMPRESSED = 0x00000004;
        /// 文件被标记为不可修改
        const STATX_ATTR_IMMUTABLE = 0x00000010;
        /// 文件是只追加写入的
        const STATX_ATTR_APPEND = 0x00000020;
        /// 文件不会被备份
        const STATX_ATTR_NODUMP = 0x00000040;
        /// 文件需要密钥才能在文件系统中解密
        const STATX_ATTR_ENCRYPTED = 0x00000800;
        /// 目录是自动挂载触发器
        const STATX_ATTR_AUTOMOUNT = 0x00001000;
        /// 目录是挂载点的根目录
        const STATX_ATTR_MOUNT_ROOT = 0x00002000;
        /// 文件受到 Verity 保护
        const STATX_ATTR_VERITY = 0x00100000;
        /// 文件当前处于 DAX 状态 CPU直接访问
        const STATX_ATTR_DAX = 0x00200000;
        /// version monotonically increases
        const STATX_ATTR_CHANGE_MONOTONIC = 0x8000000000000000;
    }
}

bitflags! {
    pub struct UtimensFlags: u32 {
        /// 不需要解释符号链接
        const AT_SYMLINK_NOFOLLOW = 0x100;
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PosixStatfs {
    f_type: u64,
    f_bsize: u64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_fsid: u64,
    f_namelen: u64,
    f_frsize: u64,
    f_flags: u64,
    f_spare: [u64; 4],
}

impl From<SuperBlock> for PosixStatfs {
    fn from(super_block: SuperBlock) -> Self {
        Self {
            f_type: super_block.magic.bits,
            f_bsize: super_block.bsize,
            f_blocks: super_block.blocks,
            f_bfree: super_block.bfree,
            f_bavail: super_block.bavail,
            f_files: super_block.files,
            f_ffree: super_block.ffree,
            f_fsid: super_block.fsid,
            f_namelen: super_block.namelen,
            f_frsize: super_block.frsize,
            f_flags: super_block.flags,
            f_spare: [0u64; 4],
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

#[allow(dead_code)]
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
        let resolve = OpenHowResolve::from_bits_truncate(posix_open_how.resolve);
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
    /// @brief 根据提供的文件描述符的fd，复制对应的文件结构体，并返回新复制的文件结构体对应的fd
    pub fn dup(oldfd: i32) -> Result<usize, SystemError> {
        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();

        let old_file = fd_table_guard
            .get_file_by_fd(oldfd)
            .ok_or(SystemError::EBADF)?;

        let new_file = old_file.try_clone().ok_or(SystemError::EBADF)?;
        // dup默认非cloexec
        new_file.set_close_on_exec(false);
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

    pub fn dup3(oldfd: i32, newfd: i32, flags: u32) -> Result<usize, SystemError> {
        let flags = FileMode::from_bits_truncate(flags);
        if (flags.bits() & !FileMode::O_CLOEXEC.bits()) != 0 {
            return Err(SystemError::EINVAL);
        }

        if oldfd == newfd {
            return Err(SystemError::EINVAL);
        }

        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();
        return Self::do_dup3(oldfd, newfd, flags, &mut fd_table_guard);
    }

    fn do_dup2(
        oldfd: i32,
        newfd: i32,
        fd_table_guard: &mut RwLockWriteGuard<'_, FileDescriptorVec>,
    ) -> Result<usize, SystemError> {
        Self::do_dup3(oldfd, newfd, FileMode::empty(), fd_table_guard)
    }

    fn do_dup3(
        oldfd: i32,
        newfd: i32,
        flags: FileMode,
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
            if fd_table_guard.drop_fd(newfd).is_err() {
                // An I/O error occurred while attempting to close fildes2.
                return Err(SystemError::EIO);
            }
        }

        let old_file = fd_table_guard
            .get_file_by_fd(oldfd)
            .ok_or(SystemError::EBADF)?;
        let new_file = old_file.try_clone().ok_or(SystemError::EBADF)?;

        if flags.contains(FileMode::O_CLOEXEC) {
            new_file.set_close_on_exec(true);
        } else {
            new_file.set_close_on_exec(false);
        }
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
        // debug!("fcntl ({cmd:?}) fd: {fd}, arg={arg}");
        match cmd {
            FcntlCommand::DupFd | FcntlCommand::DupFdCloexec => {
                if arg < 0 || arg as usize >= FileDescriptorVec::PROCESS_MAX_FD {
                    return Err(SystemError::EBADF);
                }
                let arg = arg as usize;
                for i in arg..FileDescriptorVec::PROCESS_MAX_FD {
                    let binding = ProcessManager::current_pcb().fd_table();
                    let mut fd_table_guard = binding.write();
                    if fd_table_guard.get_file_by_fd(i as i32).is_none() {
                        if cmd == FcntlCommand::DupFd {
                            return Self::do_dup2(fd, i as i32, &mut fd_table_guard);
                        } else {
                            return Self::do_dup3(
                                fd,
                                i as i32,
                                FileMode::O_CLOEXEC,
                                &mut fd_table_guard,
                            );
                        }
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

                    if file.close_on_exec() {
                        return Ok(FD_CLOEXEC as usize);
                    } else {
                        return Ok(0);
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
                        file.set_close_on_exec(true);
                    } else {
                        file.set_close_on_exec(false);
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
                    return Ok(file.mode().bits() as usize);
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
                    file.set_mode(mode)?;
                    return Ok(0);
                }

                return Err(SystemError::EBADF);
            }
            _ => {
                // TODO: unimplemented
                // 未实现的命令，返回0，不报错。

                warn!("fcntl: unimplemented command: {:?}, defaults to 0.", cmd);
                return Err(SystemError::ENOSYS);
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
            let r = file.ftruncate(len).map(|_| 0);
            return r;
        }

        return Err(SystemError::EBADF);
    }

    pub fn statfs(path: *const u8, user_statfs: *mut PosixStatfs) -> Result<usize, SystemError> {
        let mut writer = UserBufferWriter::new(user_statfs, size_of::<PosixStatfs>(), true)?;
        let fd = open_utils::do_open(
            path,
            FileMode::O_RDONLY.bits(),
            ModeType::empty().bits(),
            true,
        )?;
        let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))
            .unwrap()
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let pcb = ProcessManager::current_pcb();
        let (_inode_begin, remain_path) = user_path_at(&pcb, fd as i32, &path)?;
        let inode = ROOT_INODE().lookup_follow_symlink(&remain_path, MAX_PATHLEN)?;
        let statfs = PosixStatfs::from(inode.fs().super_block());
        writer.copy_one_to_user(&statfs, 0)?;
        return Ok(0);
    }

    pub fn fstatfs(fd: i32, user_statfs: *mut PosixStatfs) -> Result<usize, SystemError> {
        let mut writer = UserBufferWriter::new(user_statfs, size_of::<PosixStatfs>(), true)?;
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        let file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;
        drop(fd_table_guard);
        let statfs = PosixStatfs::from(file.inode().fs().super_block());
        writer.copy_one_to_user(&statfs, 0)?;
        return Ok(0);
    }

    #[inline(never)]
    pub fn statx(
        dfd: i32,
        filename_ptr: usize,
        flags: u32,
        mask: u32,
        user_kstat_ptr: usize,
    ) -> Result<usize, SystemError> {
        if user_kstat_ptr == 0 {
            return Err(SystemError::EFAULT);
        }

        let filename = check_and_clone_cstr(filename_ptr as *const u8, Some(MAX_PATHLEN))?;
        let filename_str = filename.to_str().map_err(|_| SystemError::EINVAL)?;

        do_statx(dfd, filename_str, flags, mask, user_kstat_ptr).map(|_| 0)
    }

    #[inline(never)]
    pub fn newfstatat(
        dfd: i32,
        filename_ptr: usize,
        user_stat_buf_ptr: usize,
        flags: u32,
    ) -> Result<usize, SystemError> {
        if user_stat_buf_ptr == 0 {
            return Err(SystemError::EFAULT);
        }

        let filename = check_and_clone_cstr(filename_ptr as *const u8, Some(MAX_PATHLEN))?;
        let filename_str = filename.to_str().map_err(|_| SystemError::EINVAL)?;

        do_newfstatat(dfd, filename_str, user_stat_buf_ptr, flags).map(|_| 0)
    }

    #[inline(never)]
    pub fn newfstat(fd: i32, user_stat_buf_ptr: usize) -> Result<usize, SystemError> {
        if user_stat_buf_ptr == 0 {
            return Err(SystemError::EFAULT);
        }
        let stat = vfs_fstat(fd)?;
        // log::debug!("newfstat fd: {}, stat.size: {:?}",fd,stat.size);
        super::stat::cp_new_stat(stat, user_stat_buf_ptr).map(|_| 0)
    }

    pub fn mknod(
        path: *const u8,
        mode: ModeType,
        dev_t: DeviceNumber,
    ) -> Result<usize, SystemError> {
        let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let path = path.as_str().trim();

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

    pub fn readlink_at(
        dirfd: i32,
        path: *const u8,
        user_buf: *mut u8,
        buf_size: usize,
    ) -> Result<usize, SystemError> {
        let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let path = path.as_str().trim();
        let mut user_buf = UserBufferWriter::new(user_buf, buf_size, true)?;

        let (inode, path) = user_path_at(&ProcessManager::current_pcb(), dirfd, path)?;

        let inode = inode.lookup(path.as_str())?;
        if inode.metadata()?.file_type != FileType::SymLink {
            return Err(SystemError::EINVAL);
        }

        let ubuf = user_buf.buffer::<u8>(0).unwrap();

        let file = File::new(inode, FileMode::O_RDONLY)?;

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
}
