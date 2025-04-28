use system_error::SystemError;

use crate::{
    filesystem::vfs::{mount::is_mountpoint_root, vcore::do_file_lookup_at},
    syscall::user_access::UserBufferWriter,
    time::PosixTimeSpec,
};
use alloc::sync::Arc;

use super::{
    fcntl::AtFlags,
    syscall::{ModeType, PosixStatx, PosixStatxMask, StxAttributes},
    IndexNode,
};

#[derive(Clone)]
pub struct KStat {
    pub result_mask: PosixStatxMask, // What fields the user got
    pub mode: ModeType,              // umode_t
    pub nlink: u32,
    pub blksize: u32, // Preferred I/O size
    pub attributes: StxAttributes,
    pub attributes_mask: StxAttributes,
    pub ino: u64,
    pub dev: i64,             // dev_t
    pub rdev: i64,            // dev_t
    pub uid: u32,             // kuid_t
    pub gid: u32,             // kgid_t
    pub size: usize,          // loff_t
    pub atime: PosixTimeSpec, // struct timespec64
    pub mtime: PosixTimeSpec, // struct timespec64
    pub ctime: PosixTimeSpec, // struct timespec64
    pub btime: PosixTimeSpec, // File creation time
    pub blocks: u64,
    pub mnt_id: u64,
    pub dio_mem_align: u32,
    pub dio_offset_align: u32,
}

impl Default for KStat {
    fn default() -> Self {
        Self {
            result_mask: PosixStatxMask::empty(),
            mode: ModeType::empty(),
            nlink: Default::default(),
            blksize: Default::default(),
            attributes: StxAttributes::empty(),
            attributes_mask: StxAttributes::empty(),
            ino: Default::default(),
            dev: Default::default(),
            rdev: Default::default(),
            uid: Default::default(),
            gid: Default::default(),
            size: Default::default(),
            atime: Default::default(),
            mtime: Default::default(),
            ctime: Default::default(),
            btime: Default::default(),
            blocks: Default::default(),
            mnt_id: Default::default(),
            dio_mem_align: Default::default(),
            dio_offset_align: Default::default(),
        }
    }
}

bitflags! {
    ///  https://code.dragonos.org.cn/xref/linux-6.6.21/include/linux/namei.h?fi=LOOKUP_FOLLOW#21
    pub struct LookUpFlags: u32 {
        /// follow links at the end
        const FOLLOW = 0x0001;
        /// require a directory
        const DIRECTORY = 0x0002;
        /// force terminal automount
        const AUTOMOUNT = 0x0004;
        /// accept empty path [user_... only]
        const EMPTY = 0x4000;
        /// follow mounts in the starting point
        const DOWN = 0x8000;
        /// follow mounts in the end
        const MOUNTPOINT = 0x0080;
        /// tell ->d_revalidate() to trust no cache
        const REVAL = 0x0020;
        /// RCU pathwalk mode; semi-internal
        const RCU = 0x0040;
        /// ... in open
        const OPEN = 0x0100;
        /// ... in object creation
        const CREATE = 0x0200;
        /// ... in exclusive creation
        const EXCL = 0x0400;
        /// ... in destination of rename()
        const RENAME_TARGET = 0x0800;
        /// internal use only
        const PARENT = 0x0010;
        /// No symlink crossing
        const NO_SYMLINKS = 0x010000;
        /// No nd_jump_link() crossing
        const NO_MAGICLINKS = 0x020000;
        /// No mountpoint crossing
        const NO_XDEV = 0x040000;
        /// No escaping from starting point
        const BENEATH = 0x080000;
        /// Treat dirfd as fs root
        const IN_ROOT = 0x100000;
        /// Only do cached lookup
        const CACHED = 0x200000;
        ///  LOOKUP_* flags which do scope-related checks based on the dirfd.
        const IS_SCOPED = LookUpFlags::BENEATH.bits | LookUpFlags::IN_ROOT.bits;
    }
}

impl From<AtFlags> for LookUpFlags {
    fn from(value: AtFlags) -> Self {
        let mut lookup_flags = LookUpFlags::empty();

        if !value.contains(AtFlags::AT_SYMLINK_NOFOLLOW) {
            lookup_flags |= LookUpFlags::FOLLOW;
        }

        if !value.contains(AtFlags::AT_NO_AUTOMOUNT) {
            lookup_flags |= LookUpFlags::AUTOMOUNT;
        }

        if value.contains(AtFlags::AT_EMPTY_PATH) {
            lookup_flags |= LookUpFlags::EMPTY;
        }

        lookup_flags
    }
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/fs/stat.c#232
#[inline(never)]
pub fn vfs_statx(
    dfd: i32,
    filename: &str,
    flags: AtFlags,
    request_mask: PosixStatxMask,
) -> Result<KStat, SystemError> {
    let lookup_flags: LookUpFlags = flags.into();

    // Validate flags - only allowed flags are AT_SYMLINK_NOFOLLOW, AT_NO_AUTOMOUNT, AT_EMPTY_PATH, AT_STATX_SYNC_TYPE
    if flags.intersects(
        !(AtFlags::AT_SYMLINK_NOFOLLOW
            | AtFlags::AT_NO_AUTOMOUNT
            | AtFlags::AT_EMPTY_PATH
            | AtFlags::AT_STATX_SYNC_TYPE),
    ) {
        return Err(SystemError::EINVAL);
    }
    let inode = do_file_lookup_at(dfd, filename, lookup_flags)?;

    let mut kstat = vfs_getattr(&inode, request_mask, flags)?;
    if is_mountpoint_root(&inode) {
        kstat
            .attributes
            .insert(StxAttributes::STATX_ATTR_MOUNT_ROOT);
    }
    kstat
        .attributes_mask
        .insert(StxAttributes::STATX_ATTR_MOUNT_ROOT);

    // todo: 添加 https://code.dragonos.org.cn/xref/linux-6.6.21/fs/stat.c#266 这里的逻辑

    Ok(kstat)
}

/// 获取文件的增强基本属性
///
/// # 参数
/// - `path`: 目标文件路径
/// - `stat`: 用于返回统计信息的结构体
/// - `request_mask`: PosixStatxMask标志位，指示调用者需要哪些属性
/// - `query_flags`: 查询模式(AT_STATX_SYNC_TYPE)
///
/// # 描述
/// 向文件系统请求文件的属性。调用者必须通过request_mask和query_flags指定需要的信息。
///
/// 如果文件是远程的：
/// - 可以通过传递AT_STATX_FORCE_SYNC强制文件系统从后端存储更新属性
/// - 可以通过传递AT_STATX_DONT_SYNC禁止更新
///
/// request_mask中必须设置相应的位来指示调用者需要检索哪些属性。
/// 未请求的属性也可能被返回，但其值可能是近似的，如果是远程文件，
/// 可能没有与服务器同步。
///
/// # 返回值
/// 成功时返回0，失败时返回负的错误码
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/fs/stat.c#165
#[inline(never)]
pub fn vfs_getattr(
    inode: &Arc<dyn IndexNode>,
    request_mask: PosixStatxMask,
    mut at_flags: AtFlags,
) -> Result<KStat, SystemError> {
    if at_flags.contains(AtFlags::AT_GETATTR_NOSEC) {
        return Err(SystemError::EPERM);
    }

    let mut kstat = KStat::default();
    kstat.result_mask |= PosixStatxMask::STATX_BASIC_STATS;
    at_flags &= AtFlags::AT_STATX_SYNC_TYPE;

    let metadata = inode.metadata()?;
    if metadata.atime.is_empty() {
        kstat.result_mask.remove(PosixStatxMask::STATX_ATIME);
    }

    // todo: 添加automount和dax属性

    kstat.blksize = metadata.blk_size as u32;
    if request_mask.contains(PosixStatxMask::STATX_MODE)
        || request_mask.contains(PosixStatxMask::STATX_TYPE)
    {
        kstat.mode = metadata.mode;
    }
    if request_mask.contains(PosixStatxMask::STATX_NLINK) {
        kstat.nlink = metadata.nlinks as u32;
    }
    if request_mask.contains(PosixStatxMask::STATX_UID) {
        kstat.uid = metadata.uid as u32;
    }
    if request_mask.contains(PosixStatxMask::STATX_GID) {
        kstat.gid = metadata.gid as u32;
    }
    if request_mask.contains(PosixStatxMask::STATX_ATIME) {
        kstat.atime.tv_sec = metadata.atime.tv_sec;
        kstat.atime.tv_nsec = metadata.atime.tv_nsec;
    }
    if request_mask.contains(PosixStatxMask::STATX_MTIME) {
        kstat.mtime.tv_sec = metadata.mtime.tv_sec;
        kstat.mtime.tv_nsec = metadata.mtime.tv_nsec;
    }
    if request_mask.contains(PosixStatxMask::STATX_CTIME) {
        // ctime是文件上次修改状态的时间
        kstat.ctime.tv_sec = metadata.ctime.tv_sec;
        kstat.ctime.tv_nsec = metadata.ctime.tv_nsec;
    }
    if request_mask.contains(PosixStatxMask::STATX_INO) {
        kstat.ino = metadata.inode_id.into() as u64;
    }
    if request_mask.contains(PosixStatxMask::STATX_SIZE) {
        kstat.size = metadata.size as usize;
    }
    if request_mask.contains(PosixStatxMask::STATX_BLOCKS) {
        kstat.blocks = metadata.blocks as u64;
    }

    if request_mask.contains(PosixStatxMask::STATX_BTIME) {
        // btime是文件创建时间
        kstat.btime.tv_sec = metadata.btime.tv_sec;
        kstat.btime.tv_nsec = metadata.btime.tv_nsec;
    }
    if request_mask.contains(PosixStatxMask::STATX_ALL) {
        kstat.attributes = StxAttributes::STATX_ATTR_APPEND;
        kstat.attributes_mask |=
            StxAttributes::STATX_ATTR_AUTOMOUNT | StxAttributes::STATX_ATTR_DAX;
        kstat.dev = metadata.dev_id as i64;
        kstat.rdev = metadata.raw_dev.data() as i64;
    }

    // 把文件类型加入mode里面 （todo: 在具体的文件系统里面去实现这个操作。这里只是权宜之计）
    kstat.mode |= metadata.file_type.into();

    return Ok(kstat);
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/fs/stat.c#660
pub(super) fn do_statx(
    dfd: i32,
    filename: &str,
    flags: u32,
    mask: u32,
    user_kstat_ptr: usize,
) -> Result<(), SystemError> {
    let mask = PosixStatxMask::from_bits_truncate(mask);
    if mask.contains(PosixStatxMask::STATX_RESERVED) {
        return Err(SystemError::EINVAL);
    }

    let flags = AtFlags::from_bits_truncate(flags as i32);
    if flags.contains(AtFlags::AT_STATX_SYNC_TYPE) {
        return Err(SystemError::EINVAL);
    }

    let kstat = vfs_statx(dfd, filename, flags, mask)?;
    cp_statx(kstat, user_kstat_ptr)
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/fs/stat.c#622
#[inline(never)]
fn cp_statx(kstat: KStat, user_buf_ptr: usize) -> Result<(), SystemError> {
    let mut userbuf = UserBufferWriter::new(
        user_buf_ptr as *mut PosixStatx,
        size_of::<PosixStatx>(),
        true,
    )?;
    let mut statx: PosixStatx = PosixStatx::new();

    // Copy fields from KStat to PosixStatx
    statx.stx_mask = kstat.result_mask & !PosixStatxMask::STATX_CHANGE_COOKIE;
    statx.stx_blksize = kstat.blksize;
    statx.stx_attributes = kstat.attributes & !StxAttributes::STATX_ATTR_CHANGE_MONOTONIC;
    statx.stx_nlink = kstat.nlink;
    statx.stx_uid = kstat.uid;
    statx.stx_gid = kstat.gid;
    statx.stx_mode = kstat.mode;
    statx.stx_inode = kstat.ino;
    statx.stx_size = kstat.size as i64;
    statx.stx_blocks = kstat.blocks;
    statx.stx_attributes_mask = kstat.attributes_mask;

    // Copy time fields
    statx.stx_atime = kstat.atime;
    statx.stx_btime = kstat.btime;
    statx.stx_ctime = kstat.ctime;
    statx.stx_mtime = kstat.mtime;

    // Convert device numbers
    statx.stx_rdev_major = ((kstat.rdev >> 32) & 0xffff_ffff) as u32; // MAJOR equivalent
    statx.stx_rdev_minor = (kstat.rdev & 0xffff_ffff) as u32; // MINOR equivalent
    statx.stx_dev_major = ((kstat.dev >> 32) & 0xffff_ffff) as u32; // MAJOR equivalent
    statx.stx_dev_minor = (kstat.dev & 0xffff_ffff) as u32; // MINOR equivalent

    statx.stx_mnt_id = kstat.mnt_id;
    statx.stx_dio_mem_align = kstat.dio_mem_align;
    statx.stx_dio_offset_align = kstat.dio_offset_align;

    // Write to user space
    userbuf.copy_one_to_user(&statx, 0)?;
    Ok(())
}

// 注意！这个结构体定义的貌似不太对，需要修改！
#[repr(C)]
#[derive(Clone, Copy)]
/// # 文件信息结构体
pub struct PosixKstat {
    /// 硬件设备ID
    pub dev_id: u64,
    /// inode号
    pub inode: u64,
    /// 硬链接数
    pub nlink: u64,
    /// 文件权限
    pub mode: ModeType,
    /// 所有者用户ID
    pub uid: i32,
    /// 所有者组ID
    pub gid: i32,
    /// 设备ID
    pub rdev: i64,
    /// 文件大小
    pub size: i64,
    /// 文件系统块大小
    pub blcok_size: i64,
    /// 分配的512B块数
    pub blocks: u64,
    /// 最后访问时间
    pub atime: PosixTimeSpec,
    /// 最后修改时间
    pub mtime: PosixTimeSpec,
    /// 最后状态变化时间
    pub ctime: PosixTimeSpec,
    /// 用于填充结构体大小的空白数据
    pub _pad: [i8; 24],
}
impl PosixKstat {
    pub(super) fn new() -> Self {
        Self {
            inode: 0,
            dev_id: 0,
            mode: ModeType::empty(),
            nlink: 0,
            uid: 0,
            gid: 0,
            rdev: 0,
            size: 0,
            atime: PosixTimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            mtime: PosixTimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            ctime: PosixTimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            blcok_size: 0,
            blocks: 0,
            _pad: Default::default(),
        }
    }
}
