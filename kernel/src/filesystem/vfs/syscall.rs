use crate::filesystem::overlayfs::OverlayMountData;
use crate::filesystem::vfs::FileSystemMakerData;
use core::mem::size_of;

use alloc::{string::String, sync::Arc, vec::Vec};
use log::warn;
use system_error::SystemError;

use crate::producefs;
use crate::syscall::user_access::UserBufferReader;
use crate::{
    driver::base::{block::SeekFrom, device::device_number::DeviceNumber},
    filesystem::vfs::{file::FileDescriptorVec, vcore as Vcore},
    libs::rwlock::RwLockWriteGuard,
    mm::{verify_area, VirtAddr},
    process::ProcessManager,
    syscall::{
        user_access::{self, check_and_clone_cstr, UserBufferWriter},
        Syscall,
    },
    time::{syscall::PosixTimeval, PosixTimeSpec},
};

use super::stat::{do_newfstatat, do_statx, PosixKstat};
use super::vcore::do_symlinkat;
use super::{
    fcntl::{AtFlags, FcntlCommand, FD_CLOEXEC},
    file::{File, FileMode},
    open::{
        do_faccessat, do_fchmodat, do_fchownat, do_sys_open, do_utimensat, do_utimes, ksys_fchown,
    },
    utils::{rsplit_path, user_path_at},
    vcore::{do_mkdir_at, do_remove_dir, do_unlink_at},
    Dirent, FileType, IndexNode, SuperBlock, FSMAKER, MAX_PATHLEN, ROOT_INODE,
    VFS_MAX_FOLLOW_SYMLINK_TIMES,
};

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

bitflags! {
    pub struct UmountFlag: i32 {
        const DEFAULT = 0;          /* Default call to umount. */
        const MNT_FORCE = 1;        /* Force unmounting.  */
        const MNT_DETACH = 2;       /* Just detach from the tree.  */
        const MNT_EXPIRE = 4;       /* Mark for expiry.  */
        const UMOUNT_NOFOLLOW = 8;  /* Don't follow symlink on umount.  */
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
        path: *const u8,
        o_flags: u32,
        mode: u32,
        follow_symlink: bool,
    ) -> Result<usize, SystemError> {
        let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;

        let open_flags: FileMode = FileMode::from_bits(o_flags).ok_or(SystemError::EINVAL)?;
        let mode = ModeType::from_bits(mode).ok_or(SystemError::EINVAL)?;
        return do_sys_open(
            AtFlags::AT_FDCWD.bits(),
            &path,
            open_flags,
            mode,
            follow_symlink,
        );
    }

    pub fn openat(
        dirfd: i32,
        path: *const u8,
        o_flags: u32,
        mode: u32,
        follow_symlink: bool,
    ) -> Result<usize, SystemError> {
        let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;

        let open_flags: FileMode = FileMode::from_bits(o_flags).ok_or(SystemError::EINVAL)?;
        let mode = ModeType::from_bits(mode).ok_or(SystemError::EINVAL)?;
        return do_sys_open(dirfd, &path, open_flags, mode, follow_symlink);
    }

    /// @brief 关闭文件
    ///
    /// @param fd 文件描述符编号
    ///
    /// @return 成功返回0，失败返回错误码
    pub fn close(fd: usize) -> Result<usize, SystemError> {
        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();
        let _file = fd_table_guard.drop_fd(fd as i32)?;
        drop(fd_table_guard);
        Ok(0)
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
        let r = file.inode().ioctl(cmd, data, &file.private_data.lock());
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

        return file.read(buf.len(), buf);
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
        return file.write(buf.len(), buf);
    }

    /// @brief 调整文件操作指针的位置
    ///
    /// @param fd 文件描述符编号
    /// @param seek 调整的方式
    ///
    /// @return Ok(usize) 调整后，文件访问指针相对于文件头部的偏移量
    /// @return Err(SystemError) 调整失败，返回posix错误码
    pub fn lseek(fd: i32, offset: i64, seek: u32) -> Result<usize, SystemError> {
        let seek = match seek {
            SEEK_SET => Ok(SeekFrom::SeekSet(offset)),
            SEEK_CUR => Ok(SeekFrom::SeekCurrent(offset)),
            SEEK_END => Ok(SeekFrom::SeekEnd(offset)),
            SEEK_MAX => Ok(SeekFrom::SeekEnd(0)),
            _ => Err(SystemError::EINVAL),
        }?;

        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        let file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;

        // drop guard 以避免无法调度的问题
        drop(fd_table_guard);
        return file.lseek(seek);
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

        return file.pread(offset, len, buf);
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

        return file.pwrite(offset, len, buf);
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
    pub fn chdir(path: *const u8) -> Result<usize, SystemError> {
        if path.is_null() {
            return Err(SystemError::EFAULT);
        }

        let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;

        let proc = ProcessManager::current_pcb();
        // Copy path to kernel space to avoid some security issues
        let mut new_path = String::from("");
        if !path.is_empty() {
            let cwd = match path.as_bytes()[0] {
                b'/' => String::from("/"),
                _ => proc.basic().cwd(),
            };
            let mut cwd_vec: Vec<_> = cwd.split('/').filter(|&x| !x.is_empty()).collect();
            let path_split = path.split('/').filter(|&x| !x.is_empty());
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
                new_path.push('/');
                new_path.push_str(seg);
            }
            if new_path.is_empty() {
                new_path = String::from("/");
            }
        }
        let inode =
            match ROOT_INODE().lookup_follow_symlink(&new_path, VFS_MAX_FOLLOW_SYMLINK_TIMES) {
                Err(_) => {
                    return Err(SystemError::ENOENT);
                }
                Ok(i) => i,
            };
        let metadata = inode.metadata()?;
        if metadata.file_type == FileType::Dir {
            proc.basic_mut().set_cwd(new_path);
            proc.fs_struct_mut().set_pwd(inode);
            return Ok(0);
        } else {
            return Err(SystemError::ENOTDIR);
        }
    }

    pub fn fchdir(fd: i32) -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let file = pcb
            .fd_table()
            .read()
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;
        let inode = file.inode();
        if inode.metadata()?.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        let path = inode.absolute_path()?;
        pcb.basic_mut().set_cwd(path);
        pcb.fs_struct_mut().set_pwd(inode);
        return Ok(0);
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

        let res = file.readdir(dirent).map(|x| x as usize);

        return res;
    }

    /// @brief 创建文件夹
    ///
    /// @param path(r8) 路径 / mode(r9) 模式
    ///
    /// @return uint64_t 负数错误码 / 0表示成功
    pub fn mkdir(path: *const u8, mode: usize) -> Result<usize, SystemError> {
        let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;

        do_mkdir_at(
            AtFlags::AT_FDCWD.bits(),
            &path,
            FileMode::from_bits_truncate(mode as u32),
        )?;
        return Ok(0);
    }

    pub fn mkdir_at(dirfd: i32, path: *const u8, mode: usize) -> Result<usize, SystemError> {
        let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        do_mkdir_at(dirfd, &path, FileMode::from_bits_truncate(mode as u32))?;
        return Ok(0);
    }

    /// **创建硬连接的系统调用**
    ///    
    /// ## 参数
    ///
    /// - 'oldfd': 用于解析源文件路径的文件描述符
    /// - 'old': 源文件路径
    /// - 'newfd': 用于解析新文件路径的文件描述符
    /// - 'new': 新文件将创建的路径
    /// - 'flags': 标志位，仅以位或方式包含AT_EMPTY_PATH和AT_SYMLINK_FOLLOW
    ///
    ///
    pub fn do_linkat(
        oldfd: i32,
        old: &str,
        newfd: i32,
        new: &str,
        flags: AtFlags,
    ) -> Result<usize, SystemError> {
        // flag包含其他未规定值时返回EINVAL
        if !(AtFlags::AT_EMPTY_PATH | AtFlags::AT_SYMLINK_FOLLOW).contains(flags) {
            return Err(SystemError::EINVAL);
        }
        // TODO AT_EMPTY_PATH标志启用时，进行调用者CAP_DAC_READ_SEARCH或相似的检查
        let symlink_times = if flags.contains(AtFlags::AT_SYMLINK_FOLLOW) {
            0_usize
        } else {
            VFS_MAX_FOLLOW_SYMLINK_TIMES
        };
        let pcb = ProcessManager::current_pcb();

        // 得到源路径的inode
        let old_inode: Arc<dyn IndexNode> = if old.is_empty() {
            if flags.contains(AtFlags::AT_EMPTY_PATH) {
                // 在AT_EMPTY_PATH启用时，old可以为空，old_inode实际为oldfd所指文件，但该文件不能为目录。
                let binding = pcb.fd_table();
                let fd_table_guard = binding.read();
                let file = fd_table_guard
                    .get_file_by_fd(oldfd)
                    .ok_or(SystemError::EBADF)?;
                let old_inode = file.inode();
                old_inode
            } else {
                return Err(SystemError::ENONET);
            }
        } else {
            let (old_begin_inode, old_remain_path) = user_path_at(&pcb, oldfd, old)?;
            old_begin_inode.lookup_follow_symlink(&old_remain_path, symlink_times)?
        };

        // old_inode为目录时返回EPERM
        if old_inode.metadata().unwrap().file_type == FileType::Dir {
            return Err(SystemError::EPERM);
        }

        // 得到新创建节点的父节点
        let (new_begin_inode, new_remain_path) = user_path_at(&pcb, newfd, new)?;
        let (new_name, new_parent_path) = rsplit_path(&new_remain_path);
        let new_parent =
            new_begin_inode.lookup_follow_symlink(new_parent_path.unwrap_or("/"), symlink_times)?;

        // 被调用者利用downcast_ref判断两inode是否为同一文件系统
        return new_parent.link(new_name, &old_inode).map(|_| 0);
    }

    pub fn link(old: *const u8, new: *const u8) -> Result<usize, SystemError> {
        let get_path = |cstr: *const u8| -> Result<String, SystemError> {
            let res = check_and_clone_cstr(cstr, Some(MAX_PATHLEN))?
                .into_string()
                .map_err(|_| SystemError::EINVAL)?;

            if res.len() >= MAX_PATHLEN {
                return Err(SystemError::ENAMETOOLONG);
            }
            if res.is_empty() {
                return Err(SystemError::ENOENT);
            }
            Ok(res)
        };
        let old = get_path(old)?;
        let new = get_path(new)?;
        return Self::do_linkat(
            AtFlags::AT_FDCWD.bits(),
            &old,
            AtFlags::AT_FDCWD.bits(),
            &new,
            AtFlags::empty(),
        );
    }

    pub fn linkat(
        oldfd: i32,
        old: *const u8,
        newfd: i32,
        new: *const u8,
        flags: i32,
    ) -> Result<usize, SystemError> {
        let old = check_and_clone_cstr(old, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let new = check_and_clone_cstr(new, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        if old.len() >= MAX_PATHLEN || new.len() >= MAX_PATHLEN {
            return Err(SystemError::ENAMETOOLONG);
        }
        // old 根据flags & AtFlags::AT_EMPTY_PATH判空
        if new.is_empty() {
            return Err(SystemError::ENOENT);
        }
        let flags = AtFlags::from_bits(flags).ok_or(SystemError::EINVAL)?;
        Self::do_linkat(oldfd, &old, newfd, &new, flags)
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
    pub fn unlinkat(dirfd: i32, path: *const u8, flags: u32) -> Result<usize, SystemError> {
        let flags = AtFlags::from_bits(flags as i32).ok_or(SystemError::EINVAL)?;

        let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;

        if flags.contains(AtFlags::AT_REMOVEDIR) {
            // debug!("rmdir");
            match do_remove_dir(dirfd, &path) {
                Err(err) => {
                    return Err(err);
                }
                Ok(_) => {
                    return Ok(0);
                }
            }
        }

        match do_unlink_at(dirfd, &path) {
            Err(err) => {
                return Err(err);
            }
            Ok(_) => {
                return Ok(0);
            }
        }
    }

    pub fn rmdir(path: *const u8) -> Result<usize, SystemError> {
        let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        return do_remove_dir(AtFlags::AT_FDCWD.bits(), &path).map(|v| v as usize);
    }

    pub fn unlink(path: *const u8) -> Result<usize, SystemError> {
        let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        return do_unlink_at(AtFlags::AT_FDCWD.bits(), &path).map(|v| v as usize);
    }

    pub fn symlink(oldname: *const u8, newname: *const u8) -> Result<usize, SystemError> {
        return do_symlinkat(oldname, AtFlags::AT_FDCWD.bits(), newname);
    }

    pub fn symlinkat(
        oldname: *const u8,
        newdfd: i32,
        newname: *const u8,
    ) -> Result<usize, SystemError> {
        return do_symlinkat(oldname, newdfd, newname);
    }

    /// # 修改文件名
    ///
    ///
    /// ## 参数
    ///
    /// - oldfd: 源文件夹文件描述符
    /// - filename_from: 源文件路径
    /// - newfd: 目标文件夹文件描述符
    /// - filename_to: 目标文件路径
    /// - flags: 标志位
    ///
    ///
    /// ## 返回值
    /// - Ok(返回值类型): 返回值的说明
    /// - Err(错误值类型): 错误的说明
    ///
    pub fn do_renameat2(
        oldfd: i32,
        filename_from: *const u8,
        newfd: i32,
        filename_to: *const u8,
        _flags: u32,
    ) -> Result<usize, SystemError> {
        let filename_from = check_and_clone_cstr(filename_from, Some(MAX_PATHLEN))
            .unwrap()
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let filename_to = check_and_clone_cstr(filename_to, Some(MAX_PATHLEN))
            .unwrap()
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        // 文件名过长
        if filename_from.len() > MAX_PATHLEN || filename_to.len() > MAX_PATHLEN {
            return Err(SystemError::ENAMETOOLONG);
        }

        //获取pcb，文件节点
        let pcb = ProcessManager::current_pcb();
        let (_old_inode_begin, old_remain_path) = user_path_at(&pcb, oldfd, &filename_from)?;
        let (_new_inode_begin, new_remain_path) = user_path_at(&pcb, newfd, &filename_to)?;
        //获取父目录
        let (old_filename, old_parent_path) = rsplit_path(&old_remain_path);
        let old_parent_inode = ROOT_INODE()
            .lookup_follow_symlink(old_parent_path.unwrap_or("/"), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
        let (new_filename, new_parent_path) = rsplit_path(&new_remain_path);
        let new_parent_inode = ROOT_INODE()
            .lookup_follow_symlink(new_parent_path.unwrap_or("/"), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
        old_parent_inode.move_to(old_filename, &new_parent_inode, new_filename)?;
        return Ok(0);
    }

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
        let metadata = file.metadata()?;
        kstat.size = metadata.size;
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
        match file.file_type() {
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
        let mut writer = UserBufferWriter::new(usr_kstat, size_of::<PosixKstat>(), true)?;
        let kstat = Self::do_fstat(fd)?;

        writer.copy_one_to_user(&kstat, 0)?;
        return Ok(0);
    }

    pub fn stat(path: *const u8, user_kstat: *mut PosixKstat) -> Result<usize, SystemError> {
        let fd = Self::open(
            path,
            FileMode::O_RDONLY.bits(),
            ModeType::empty().bits(),
            true,
        )?;
        let r = Self::fstat(fd as i32, user_kstat);
        Self::close(fd).ok();
        return r;
    }

    pub fn lstat(path: *const u8, user_kstat: *mut PosixKstat) -> Result<usize, SystemError> {
        let fd = Self::open(
            path,
            FileMode::O_RDONLY.bits(),
            ModeType::empty().bits(),
            false,
        )?;
        let r = Self::fstat(fd as i32, user_kstat);
        Self::close(fd).ok();
        return r;
    }

    pub fn statfs(path: *const u8, user_statfs: *mut PosixStatfs) -> Result<usize, SystemError> {
        let mut writer = UserBufferWriter::new(user_statfs, size_of::<PosixStatfs>(), true)?;
        let fd = Self::open(
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

    pub fn writev(fd: i32, iov: usize, count: usize) -> Result<usize, SystemError> {
        // IoVecs会进行用户态检验
        let iovecs = unsafe { IoVecs::from_user(iov as *const IoVec, count, false) }?;

        let data = iovecs.gather();

        Self::write(fd, &data)
    }

    pub fn readv(fd: i32, iov: usize, count: usize) -> Result<usize, SystemError> {
        // IoVecs会进行用户态检验
        let mut iovecs = unsafe { IoVecs::from_user(iov as *const IoVec, count, true) }?;

        let mut data = vec![0; iovecs.0.iter().map(|x| x.len()).sum()];

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

    pub fn fchmod(fd: i32, mode: u32) -> Result<usize, SystemError> {
        let _mode = ModeType::from_bits(mode).ok_or(SystemError::EINVAL)?;
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        let _file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;

        // fchmod没完全实现，因此不修改文件的权限
        // todo: 实现fchmod
        warn!("fchmod not fully implemented");
        return Ok(0);
    }

    pub fn chown(pathname: *const u8, uid: usize, gid: usize) -> Result<usize, SystemError> {
        let pathname = user_access::check_and_clone_cstr(pathname, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        return do_fchownat(
            AtFlags::AT_FDCWD.bits(),
            &pathname,
            uid,
            gid,
            AtFlags::AT_STATX_SYNC_AS_STAT,
        );
    }

    pub fn lchown(pathname: *const u8, uid: usize, gid: usize) -> Result<usize, SystemError> {
        let pathname = user_access::check_and_clone_cstr(pathname, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        return do_fchownat(
            AtFlags::AT_FDCWD.bits(),
            &pathname,
            uid,
            gid,
            AtFlags::AT_SYMLINK_NOFOLLOW,
        );
    }

    pub fn fchownat(
        dirfd: i32,
        pathname: *const u8,
        uid: usize,
        gid: usize,
        flags: i32,
    ) -> Result<usize, SystemError> {
        let pathname = user_access::check_and_clone_cstr(pathname, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let pathname = pathname.as_str().trim();
        let flags = AtFlags::from_bits_truncate(flags);
        return do_fchownat(dirfd, pathname, uid, gid, flags);
    }

    pub fn fchown(fd: i32, uid: usize, gid: usize) -> Result<usize, SystemError> {
        return ksys_fchown(fd, uid, gid);
    }

    /// #挂载文件系统
    ///
    /// 用于挂载文件系统,目前仅支持ramfs挂载
    ///
    /// ## 参数:
    ///
    /// - source       挂载设备(暂时不支持)
    /// - target       挂载目录
    /// - filesystemtype   文件系统
    /// - mountflags     挂载选项（暂未实现）
    /// - data        带数据挂载
    ///
    /// ## 返回值
    /// - Ok(0): 挂载成功
    /// - Err(SystemError) :挂载过程中出错
    pub fn mount(
        _source: *const u8,
        target: *const u8,
        filesystemtype: *const u8,
        _mountflags: usize,
        data: *const u8,
    ) -> Result<usize, SystemError> {
        let target = user_access::check_and_clone_cstr(target, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;

        let fstype_str = user_access::check_and_clone_cstr(filesystemtype, Some(MAX_PATHLEN))?;
        let fstype_str = fstype_str.to_str().map_err(|_| SystemError::EINVAL)?;

        let fstype = producefs!(FSMAKER, fstype_str, data)?;

        Vcore::do_mount(fstype, &target)?;

        return Ok(0);
    }

    // 想法：可以在VFS中实现一个文件系统分发器，流程如下：
    // 1. 接受从上方传来的文件类型字符串
    // 2. 将传入值与启动时准备好的字符串数组逐个比较（probe）
    // 3. 直接在函数内调用构造方法并直接返回文件系统对象

    /// src/linux/mount.c `umount` & `umount2`
    ///
    /// [umount(2) — Linux manual page](https://www.man7.org/linux/man-pages/man2/umount.2.html)
    pub fn umount2(target: *const u8, flags: i32) -> Result<(), SystemError> {
        let target = user_access::check_and_clone_cstr(target, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        Vcore::do_umount2(
            AtFlags::AT_FDCWD.bits(),
            &target,
            UmountFlag::from_bits(flags).ok_or(SystemError::EINVAL)?,
        )?;
        return Ok(());
    }

    pub fn sys_utimensat(
        dirfd: i32,
        pathname: *const u8,
        times: *const PosixTimeSpec,
        flags: u32,
    ) -> Result<usize, SystemError> {
        let pathname = if pathname.is_null() {
            None
        } else {
            let pathname = check_and_clone_cstr(pathname, Some(MAX_PATHLEN))?
                .into_string()
                .map_err(|_| SystemError::EINVAL)?;
            Some(pathname)
        };
        let flags = UtimensFlags::from_bits(flags).ok_or(SystemError::EINVAL)?;
        let times = if times.is_null() {
            None
        } else {
            let times_reader = UserBufferReader::new(times, size_of::<PosixTimeSpec>() * 2, true)?;
            let times = times_reader.read_from_user::<PosixTimeSpec>(0)?;
            Some([times[0], times[1]])
        };
        do_utimensat(dirfd, pathname, times, flags)
    }

    pub fn sys_utimes(
        pathname: *const u8,
        times: *const PosixTimeval,
    ) -> Result<usize, SystemError> {
        let pathname = check_and_clone_cstr(pathname, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let times = if times.is_null() {
            None
        } else {
            let times_reader = UserBufferReader::new(times, size_of::<PosixTimeval>() * 2, true)?;
            let times = times_reader.read_from_user::<PosixTimeval>(0)?;
            Some([times[0], times[1]])
        };
        do_utimes(&pathname, times)
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

        let mut slices: Vec<&mut [u8]> = Vec::with_capacity(iovs.len());

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
