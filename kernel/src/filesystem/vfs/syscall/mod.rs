use crate::{syscall::user_access::check_and_clone_cstr, time::PosixTimeSpec};

use super::{fcntl::AtFlags, file::FileMode, SuperBlock};
mod dup2;
mod faccessat2;
mod link_utils;
mod newfstat;
mod open_utils;
mod readlink_at;
mod rename_utils;
mod sys_chdir;
mod sys_close;
mod sys_dup;
mod sys_dup3;
mod sys_faccessat;
mod sys_faccessat2;
mod sys_fchdir;
mod sys_fchmod;
mod sys_fchmodat;
mod sys_fchown;
mod sys_fchownat;
mod sys_fcntl;
mod sys_fstatfs;
mod sys_ftruncate;
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
mod sys_readlinkat;
mod sys_readv;
mod sys_renameat2;
mod sys_select;
mod sys_statfs;
mod sys_statx;
mod sys_symlinkat;
mod sys_truncate;
mod sys_unlinkat;
mod sys_utimensat;
mod sys_write;
mod sys_writev;
mod utimensat;

mod epoll_utils;
mod sys_epoll_create1;
mod sys_epoll_ctl;
mod sys_epoll_pwait;

pub mod symlink_utils;
mod sys_fsync;
pub mod sys_mount;
mod sys_sync;
pub mod sys_umount2;

#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
mod sys_fstat;

#[cfg(target_arch = "x86_64")]
mod sys_access;
#[cfg(target_arch = "x86_64")]
mod sys_chmod;
#[cfg(target_arch = "x86_64")]
mod sys_chown;
#[cfg(target_arch = "x86_64")]
mod sys_dup2;
#[cfg(target_arch = "x86_64")]
mod sys_epoll_create;
#[cfg(target_arch = "x86_64")]
mod sys_epoll_wait;

#[cfg(target_arch = "x86_64")]
mod sys_futimesat;
#[cfg(target_arch = "x86_64")]
mod sys_lchown;
#[cfg(target_arch = "x86_64")]
mod sys_link;
#[cfg(target_arch = "x86_64")]
mod sys_lstat;
#[cfg(target_arch = "x86_64")]
mod sys_mkdir;
#[cfg(target_arch = "x86_64")]
mod sys_mknod;
#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
mod sys_newfstatat;
#[cfg(target_arch = "x86_64")]
mod sys_open;
#[cfg(target_arch = "x86_64")]
mod sys_readlink;
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

mod sys_fgetxattr;
mod sys_fsetxattr;
mod sys_getxattr;
mod sys_lgetxattr;
mod sys_lsetxattr;
mod sys_setxattr;
mod xattr_utils;

pub const SEEK_SET: u32 = 0;
pub const SEEK_CUR: u32 = 1;
pub const SEEK_END: u32 = 2;
pub const SEEK_MAX: u32 = 3;

// 扩展属性操作标志
pub const XATTR_CREATE: i32 = 0x1; // 设置值，如果属性不存在则创建，已存在返回则失败
pub const XATTR_REPLACE: i32 = 0x2; // 设置值，如果属性已存在则替换，不存在返回则失败

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
