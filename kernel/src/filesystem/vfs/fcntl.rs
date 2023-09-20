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

/// for F_[GET|SET]FL
pub const FD_CLOEXEC: u32 = 1;
