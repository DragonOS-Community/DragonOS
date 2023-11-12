const F_LINUX_SPECIFIC_BASE: u32 = 1024;

/// fcntl syscall command
///
/// for linux-specific fcntl commands, see:
/// https://opengrok.ringotek.cn/xref/linux-5.19.10/tools/include/uapi/linux/fcntl.h#8
#[derive(Debug, Copy, Clone, Eq, PartialEq, FromPrimitive, ToPrimitive)]
#[repr(u32)]
pub enum FcntlCommand {
    /// dup
    DupFd = 0,
    /// get close-on-exec
    GetFd = 1,
    /// set/clear close-on-exec
    SetFd = 2,
    /// get file flags
    GetFlags = 3,
    /// set file flags
    SetFlags = 4,
    /// get record locking info
    GetLock = 5,
    /// set record locking info (non-blocking)
    SetLock = 6,
    /// set record locking info (blocking)
    SetLockWait = 7,

    SetLease = F_LINUX_SPECIFIC_BASE + 0,
    GetLease = F_LINUX_SPECIFIC_BASE + 1,

    /// Request nofications on a directory.
    /// See below for events that may be notified.
    Notify = F_LINUX_SPECIFIC_BASE + 2,

    /// Cancel a blocking posix lock; internal use only until we expose an
    /// asynchronous lock api to userspace
    CancelLock = F_LINUX_SPECIFIC_BASE + 5,
    /// Create a file descriptor with FD_CLOEXEC set.
    DupFdCloexec = F_LINUX_SPECIFIC_BASE + 6,

    /// Set pipe page size array
    SetPipeSize = F_LINUX_SPECIFIC_BASE + 7,
    /// Get pipe page size array
    GetPipeSize = F_LINUX_SPECIFIC_BASE + 8,

    /// Set seals
    AddSeals = F_LINUX_SPECIFIC_BASE + 9,
    /// Get seals
    GetSeals = F_LINUX_SPECIFIC_BASE + 10,

    /**
     * Set/Get write life time hints. {GET,SET}_RW_HINT operate on the
     * underlying inode, while {GET,SET}_FILE_RW_HINT operate only on
     * the specific file.
     */
    GetRwHint = F_LINUX_SPECIFIC_BASE + 11,
    SetRwHint = F_LINUX_SPECIFIC_BASE + 12,
    GetFileRwHint = F_LINUX_SPECIFIC_BASE + 13,
    SetFileRwHint = F_LINUX_SPECIFIC_BASE + 14,
}

///  The constants AT_REMOVEDIR and AT_EACCESS have the same value.  AT_EACCESS is
///  meaningful only to faccessat, while AT_REMOVEDIR is meaningful only to
///  unlinkat.  The two functions do completely different things and therefore,
///  the flags can be allowed to overlap.  For example, passing AT_REMOVEDIR to
///  faccessat would be undefined behavior and thus treating it equivalent to
///  AT_EACCESS is valid undefined behavior.
#[derive(Debug, Copy, Clone, Eq, PartialEq, FromPrimitive, ToPrimitive)]
#[allow(non_camel_case_types)]
pub enum AtFlags {
    /// 特殊值，用于指示openat应使用当前工作目录。
    AtFdCwd = -100,
    /// 不要跟随符号链接。
    /// AT_SYMLINK_NOFOLLOW: 0x100
    AtSymlinkNoFollow = 0x100,
    /// AtEAccess: 使用有效ID进行访问测试，而不是实际ID。
    /// AtRemoveDir: 删除目录而不是取消链接文件。
    /// AT_EACCESS: 0x200
    /// AT_REMOVEDIR: 0x200
    AtEAccess_OR_AtRemoveDir = 0x200,

    /// 跟随符号链接。
    /// AT_SYMLINK_FOLLOW: 0x400
    AtSymlinkFollow = 0x400,
    /// 禁止终端自动挂载遍历。
    /// AT_NO_AUTOMOUNT: 0x800
    AtNoAutomount = 0x800,
    /// 允许空的相对路径名。
    /// AT_EMPTY_PATH: 0x1000
    AtEmptyPath = 0x1000,
    /// statx()所需的同步类型。
    /// AT_STATX_SYNC_TYPE: 0x6000
    AtStatxSyncType = 0x6000,
    /// 执行与stat()相同的操作。
    /// AT_STATX_SYNC_AS_STAT: 0x0000
    AtStatxSyncAsStat = 0x0000,
    /// 强制将属性与服务器同步。
    /// AT_STATX_FORCE_SYNC: 0x2000
    AtStatxForceSync = 0x2000,
    /// 不要将属性与服务器同步。
    /// AT_STATX_DONT_SYNC: 0x4000
    AtStatxDontSync = 0x4000,
    /// 应用于整个子树。
    /// AT_RECURSIVE: 0x8000
    AtRecursive = 0x8000,
}

impl Into<i32> for AtFlags {
    fn into(self) -> i32 {
        self as i32
    }
}

/// for F_[GET|SET]FL
pub const FD_CLOEXEC: u32 = 1;
