pub mod append_lock;
pub mod fasync;
pub mod fcntl;
pub mod file;
pub mod flock;
pub mod iov;
pub mod mount;
pub mod open;
pub mod permission;
pub mod posix_lock;
pub mod stat;
pub mod syscall;
pub mod utils;
pub mod vcore;
pub mod writeback;

use alloc::{string::String, sync::Arc, vec::Vec};
use core::{
    any::Any,
    fmt::{Debug, Display, Write},
    sync::atomic::AtomicUsize,
};
use derive_builder::Builder;
use intertrait::CastFromSync;
use mount::MountFlags;
use system_error::SystemError;

use crate::{
    driver::base::{
        block::block_device::BlockDevice, char::CharDevice, device::device_number::DeviceNumber,
    },
    filesystem::{
        epoll::EPollItem,
        vfs::{file::File, permission::PermissionMask, syscall::RenameFlags},
    },
    ipc::pipe::LockedPipeInode,
    libs::{
        casting::DowncastArc,
        mutex::{Mutex, MutexGuard},
    },
    mm::{fault::PageFaultMessage, VirtRegion, VmFaultReason, VmFlags},
    net::socket::Socket,
    process::ProcessManager,
    syscall::user_buffer::UserBuffer,
    time::PosixTimeSpec,
};

pub use self::{file::FilePrivateData, mount::MountFS};
use self::{
    file::{FileFlags, FileMode},
    utils::DName,
    vcore::generate_inode_id,
};

use super::page_cache::PageCache;

/// vfs容许的最大的路径名称长度
pub const MAX_PATHLEN: usize = 4096;

/// 单个文件名的最大长度
pub const NAME_MAX: usize = 255;

// 定义inode号
int_like!(InodeId, AtomicInodeId, usize, AtomicUsize);

impl Display for InodeId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 文件的类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    /// 文件
    File,
    /// 文件夹
    Dir,
    /// 块设备
    BlockDevice,
    /// 字符设备
    CharDevice,
    /// 帧缓冲设备
    FramebufferDevice,
    /// kvm设备
    KvmDevice,
    /// 管道文件
    Pipe,
    /// 符号链接
    SymLink,
    /// 套接字
    Socket,
}

bitflags! {
    /// 文件类型和权限
    #[repr(C)]
    pub struct InodeMode: u32 {
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

impl From<FileType> for InodeMode {
    fn from(val: FileType) -> Self {
        match val {
            FileType::File => InodeMode::S_IFREG,
            FileType::Dir => InodeMode::S_IFDIR,
            FileType::BlockDevice => InodeMode::S_IFBLK,
            FileType::CharDevice => InodeMode::S_IFCHR,
            FileType::SymLink => InodeMode::S_IFLNK,
            FileType::Socket => InodeMode::S_IFSOCK,
            FileType::Pipe => InodeMode::S_IFIFO,
            FileType::KvmDevice => InodeMode::S_IFCHR,
            FileType::FramebufferDevice => InodeMode::S_IFCHR,
        }
    }
}

impl From<InodeMode> for FileType {
    fn from(mode: InodeMode) -> Self {
        // 提取文件类型部分
        match mode & InodeMode::S_IFMT {
            t if t == InodeMode::S_IFREG => FileType::File,
            t if t == InodeMode::S_IFDIR => FileType::Dir,
            t if t == InodeMode::S_IFBLK => FileType::BlockDevice,
            t if t == InodeMode::S_IFCHR => FileType::CharDevice,
            t if t == InodeMode::S_IFLNK => FileType::SymLink,
            t if t == InodeMode::S_IFSOCK => FileType::Socket,
            t if t == InodeMode::S_IFIFO => FileType::Pipe,
            // 默认情况，通常应该不会发生，因为 S_IFMT 应该覆盖所有情况
            _ => FileType::File,
        }
    }
}

bitflags! {
    pub struct InodeFlags: u32 {
        /// 写入时立即同步到磁盘
        const S_SYNC = (1 << 0);
        /// 不更新访问时间
        const S_NOATIME = (1 << 1);
        /// 只允许追加写入
        const S_APPEND = (1 << 2);
        /// 不可修改的文件
        const S_IMMUTABLE = (1 << 3);
        /// 目录已删除但仍被打开
        const S_DEAD = (1 << 4);
        /// 不计入磁盘配额
        const S_NOQUOTA = (1 << 5);
        /// 目录操作同步写入
        const S_DIRSYNC = (1 << 6);
        /// 不更新 ctime/mtime
        const S_NOCMTIME = (1 << 7);
        /// 交换文件，禁止截断（swapon已获取块映射）
        const S_SWAPFILE = (1 << 8);
        /// 文件系统内部使用的私有inode
        const S_PRIVATE = (1 << 9);
        /// 关联了IMA（完整性度量架构）结构
        const S_IMA = (1 << 10);
        /// 自动挂载点或引用目录
        const S_AUTOMOUNT = (1 << 11);
        /// 无suid或xattr安全属性
        const S_NOSEC = (1 << 12);
        /// 直接访问模式，绕过页缓存
        const S_DAX = (1 << 13);
        /// 加密文件（使用fs/crypto/）
        const S_ENCRYPTED = (1 << 14);
        /// 大小写不敏感的文件
        const S_CASEFOLD = (1 << 15);
        /// 完整性校验文件（使用fs/verity/）
        const S_VERITY = (1 << 16);
        /// 内核正在使用的文件（如cachefiles）
        const S_KERNEL_FILE = (1 << 17);
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum SpecialNodeData {
    /// 管道文件
    Pipe(Arc<LockedPipeInode>),
    /// 字符设备
    CharDevice(Arc<dyn CharDevice>),
    /// 块设备
    BlockDevice(Arc<dyn BlockDevice>),
    /// 指向其他 inode 的引用（用于 /proc/self/fd/N 这种魔法链接）
    Reference(Arc<dyn IndexNode>),
}

/* these are defined by POSIX and also present in glibc's dirent.h */
/// 完整含义请见 http://www.gnu.org/software/libc/manual/html_node/Directory-Entries.html
#[allow(dead_code)]
pub const DT_UNKNOWN: u16 = 0;
/// 命名管道，或者FIFO
pub const DT_FIFO: u16 = 1;
// 字符设备
pub const DT_CHR: u16 = 2;
// 目录
pub const DT_DIR: u16 = 4;
// 块设备
pub const DT_BLK: u16 = 6;
// 常规文件
pub const DT_REG: u16 = 8;
// 符号链接
pub const DT_LNK: u16 = 10;
// 是一个socket
pub const DT_SOCK: u16 = 12;
// 这个是抄Linux的，还不知道含义
#[allow(dead_code)]
pub const DT_WHT: u16 = 14;
#[allow(dead_code)]
pub const DT_MAX: u16 = 16;

/// VFS 允许的最大符号链接跟随次数。
///
/// Linux 6.6: MAXSYMLINKS = 40
///
/// 重要约定（兼容既有调用点）：
/// - `max_follow_times == 0` 表示 **完全禁用** symlink 跟随（旧行为：不会因为 symlink 而返回 ELOOP）
/// - `max_follow_times == 1` 表示"计数已耗尽"，此时若仍需要跟随 symlink，应返回 `ELOOP`
/// - `max_follow_times >= 2` 才允许继续跟随，并在每次跟随时递减
///
/// 因此这里取 41，以"保留 0 的禁用语义"同时实现"最多 40 次跟随"的 Linux 语义。
pub const VFS_MAX_FOLLOW_SYMLINK_TIMES: usize = 41;

impl FileType {
    pub fn get_file_type_num(&self) -> u16 {
        return match self {
            FileType::File => DT_REG,
            FileType::Dir => DT_DIR,
            FileType::BlockDevice => DT_BLK,
            FileType::CharDevice => DT_CHR,
            FileType::KvmDevice => DT_CHR,
            FileType::Pipe => DT_FIFO,
            FileType::SymLink => DT_LNK,
            FileType::Socket => DT_SOCK,
            FileType::FramebufferDevice => DT_CHR,
        };
    }
}

bitflags! {
    /// @brief inode的状态（由poll方法返回）
    pub struct PollStatus: u8 {
        const WRITE = 1u8 << 0;
        const READ = 1u8 << 1;
        const ERROR = 1u8 << 2;
    }
}

/// The pollable inode trait
pub trait PollableInode: Any + Sync + Send + Debug + CastFromSync {
    /// Return the poll status of the inode
    fn poll(&self, private_data: &FilePrivateData) -> Result<usize, SystemError>;
    /// Add an epoll item to the inode
    fn add_epitem(
        &self,
        epitem: Arc<EPollItem>,
        private_data: &FilePrivateData,
    ) -> Result<(), SystemError>;
    /// Remove epitems associated with the epoll
    fn remove_epitem(
        &self,
        epitm: &Arc<EPollItem>,
        private_data: &FilePrivateData,
    ) -> Result<(), SystemError>;

    /// Add a fasync item for SIGIO notification
    fn add_fasync(
        &self,
        _fasync_item: fasync::FAsyncItem,
        _private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        // Default implementation: not supported
        Err(SystemError::ENOSYS)
    }

    /// Remove a fasync item
    fn remove_fasync(
        &self,
        _file: &alloc::sync::Weak<file::File>,
        _private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        // Default implementation: not supported
        Err(SystemError::ENOSYS)
    }
}

pub trait IndexNode: Any + Sync + Send + Debug + CastFromSync {
    /// 是否为"流式"文件（不可 random access / 不可 seek）。
    ///
    /// 语义目标：把"pread/pwrite/lseek 应返回 ESPIPE"的判定收敛在 VFS 层，
    /// 避免在 syscall 层枚举 FileType 或做硬编码特判。
    ///
    /// 默认规则仅覆盖"天然流式"的基础类型：Pipe/Socket。
    /// 其它伪文件（eventfd/epollfd/...）应在各自 inode 中覆写此方法。
    fn is_stream(&self) -> bool {
        match self.metadata() {
            Ok(md) => matches!(md.file_type, FileType::Pipe | FileType::Socket),
            // 元数据都拿不到时，保守起见按不可 seek 处理，避免误放行 pread/pwrite。
            Err(_) => true,
        }
    }

    /// 是否支持 seek（lseek）。
    ///
    /// 默认：普通文件/目录/块设备可 seek；Pipe/Socket/CharDevice 不可 seek；
    /// 其它类型保守按可 seek（更接近现有行为：lseek 仅显式拒绝 Pipe/CharDevice）。
    fn supports_seek(&self) -> bool {
        if self.is_stream() {
            return false;
        }
        match self.metadata() {
            Ok(md) => !matches!(
                md.file_type,
                FileType::Pipe | FileType::Socket | FileType::CharDevice
            ),
            Err(_) => false,
        }
    }

    /// 是否允许 pread（随机读，不推进文件偏移）。
    ///
    /// 默认：对 stream 文件返回 false；对非 stream 默认允许。
    /// 伪文件（如 eventfd/epollfd）应覆写 `is_stream()` 或此方法以匹配 Linux 语义。
    fn supports_pread(&self) -> bool {
        !self.is_stream()
    }

    /// 是否允许 pwrite（随机写，不推进文件偏移）。
    fn supports_pwrite(&self) -> bool {
        !self.is_stream()
    }

    fn truncate_before_open(&self, flags: &FileFlags) -> bool {
        flags.contains(FileFlags::O_TRUNC)
    }

    fn mmap(&self, _start: usize, _len: usize, _offset: usize) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn check_mmap_file(
        &self,
        _file: &Arc<File>,
        _len: usize,
        _offset: usize,
        _vm_flags: VmFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn mmap_effective_file(&self, file: &Arc<File>) -> Result<Arc<File>, SystemError> {
        Ok(file.clone())
    }

    fn mmap_file(
        &self,
        _file: &Arc<File>,
        start: usize,
        len: usize,
        offset: usize,
        _vm_flags: VmFlags,
    ) -> Result<(), SystemError> {
        self.mmap(start, len, offset)
    }

    fn read_sync(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn write_sync(&self, _offset: usize, _buf: &[u8]) -> Result<usize, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// @brief 打开文件
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    /// Adjust per-open file mode bits after `open()` initialized private data.
    ///
    /// This models Linux helpers such as `nonseekable_open()` and
    /// `stream_open()` without making VFS syscalls know filesystem-specific
    /// protocol flags.
    fn adjust_file_mode_after_open(&self, _data: &FilePrivateData, _mode: &mut FileMode) {}

    /// @brief 关闭文件
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn flush_file(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _lock_owner: u64,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    /// @brief 释放最后一个 open file description 引用
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回"不支持"
        return Err(SystemError::ENOSYS);
    }

    /// @brief 在inode的指定偏移量开始，读取指定大小的数据
    ///
    /// @param offset 起始位置在Inode中的偏移量
    /// @param len 要读取的字节数
    /// @param buf 缓冲区. 请注意，必须满足@buf.len()>=@len
    /// @param _data 各文件系统系统所需私有信息
    ///
    /// @return 成功：Ok(读取的字节数)
    ///         失败：Err(Posix错误码)
    // TODO: data argument should be redesigned to avoid preempt issues
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError>;

    /// @brief 在inode的指定偏移量开始，写入指定大小的数据（从buf的第0byte开始写入）
    ///
    /// @param offset 起始位置在Inode中的偏移量
    /// @param len 要写入的字节数
    /// @param buf 缓冲区. 请注意，必须满足@buf.len()>=@len
    /// @param _data 各文件系统系统所需私有信息
    ///
    /// @return 成功：Ok(写入的字节数)
    ///         失败：Err(Posix错误码)
    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError>;

    /// 基于打开文件上下文执行 fallocate。
    ///
    /// 默认不模拟预分配；只有真正支持 fallocate 语义的文件系统应覆盖此方法。
    fn fallocate_file(
        &self,
        _mode: i32,
        _offset: usize,
        _len: usize,
        _lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        drop(data);
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    /// # 在inode的指定偏移量开始，读取指定大小的数据，忽略PageCache
    ///
    /// ## 参数
    ///
    /// - `offset`: 起始位置在Inode中的偏移量
    /// - `len`: 要读取的字节数
    /// - `buf`: 缓冲区
    /// - `data`: 各文件系统系统所需私有信息
    ///
    /// ## 返回值
    ///
    /// - `Ok(usize)``: Ok(读取的字节数)
    /// - `Err(SystemError)``: Err(Posix错误码)
    fn read_direct(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// # 在inode的指定偏移量开始，写入指定大小的数据，忽略PageCache
    ///
    /// ## 参数
    ///
    /// - `offset`: 起始位置在Inode中的偏移量
    /// - `len`: 要读取的字节数
    /// - `buf`: 缓冲区
    /// - `data`: 各文件系统系统所需私有信息
    ///
    /// ## 返回值
    ///
    /// - `Ok(usize)``: Ok(读取的字节数)
    /// - `Err(SystemError)``: Err(Posix错误码)
    fn write_direct(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// @brief 获取inode的元数据
    ///
    /// @return 成功：Ok(inode的元数据)
    ///         失败：Err(错误码)
    fn metadata(&self) -> Result<Metadata, SystemError> {
        // 若文件系统没有实现此方法，则返回"不支持"
        return Err(SystemError::ENOSYS);
    }

    /// @brief 设置inode的元数据
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn set_metadata(&self, _metadata: &Metadata) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回"不支持"
        return Err(SystemError::ENOSYS);
    }

    /// @brief 重新设置文件的大小
    ///
    /// 如果文件大小增加，则文件内容不变，但是文件的空洞部分会被填充为0
    /// 如果文件大小减小，则文件内容会被截断
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn resize(&self, _len: usize) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回"不支持"
        return Err(SystemError::ENOSYS);
    }

    /// 基于当前 files_struct lock owner 重新设置文件大小。
    ///
    /// 默认回退到 inode 级 resize；需要 mandatory-locking 协议语义的文件系统
    /// 可覆盖该方法。
    fn resize_with_lock_owner(&self, len: usize, _lock_owner: u64) -> Result<(), SystemError> {
        self.resize(len)
    }

    /// 基于打开文件上下文重新设置文件大小。
    ///
    /// 默认回退到 inode 级 resize；需要文件句柄语义的文件系统（如 FUSE）
    /// 可覆盖该方法，从 `FilePrivateData` 中取得 per-open 状态。
    fn resize_file(
        &self,
        len: usize,
        lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        drop(data);
        self.resize_with_lock_owner(len, lock_owner)
    }

    /// @brief 在当前目录下创建一个新的inode
    ///
    /// @param name 目录项的名字
    /// @param file_type 文件类型
    /// @param mode 权限
    ///
    /// @return 创建成功：返回Ok(新的inode的Arc指针)
    /// @return 创建失败：返回Err(错误码)
    fn create(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 若文件系统没有实现此方法，则默认调用其create_with_data方法。如果仍未实现，则会得到一个Err(-ENOSYS)的返回值
        return self.create_with_data(name, file_type, mode, 0);
    }

    /// @brief 在当前目录下创建一个新的inode，并传入一个简单的data字段，方便进行初始化。
    ///
    /// @param name 目录项的名字
    /// @param file_type 文件类型
    /// @param mode 权限
    /// @param data 用于初始化该inode的数据。（为0则表示忽略此字段）对于不同的文件系统来说，代表的含义可能不同。
    ///
    /// @return 创建成功：返回Ok(新的inode的Arc指针)
    /// @return 创建失败：返回Err(错误码)
    fn create_with_data(
        &self,
        _name: &str,
        _file_type: FileType,
        _mode: InodeMode,
        _data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 若文件系统没有实现此方法，则返回"不支持"
        return Err(SystemError::ENOSYS);
    }

    /// @brief 在当前目录下创建符号链接（name -> target）
    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let inode = self.create_with_data(name, FileType::SymLink, InodeMode::S_IRWXUGO, 0)?;
        let bytes = target.as_bytes();
        let len = bytes.len();
        inode.write_at(0, len, bytes, Mutex::new(FilePrivateData::Unused).lock())?;
        Ok(inode)
    }

    /// @brief 在当前目录下，创建一个名为Name的硬链接，指向另一个IndexNode
    ///
    /// @param name 硬链接的名称
    /// @param other 要被指向的IndexNode的Arc指针
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn link(&self, _name: &str, _other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回"不支持"
        return Err(SystemError::ENOSYS);
    }

    /// @brief 在当前目录下，删除一个名为Name的硬链接
    ///
    /// @param name 硬链接的名称
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn unlink(&self, _name: &str) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回"不支持"
        return Err(SystemError::ENOSYS);
    }

    /// @brief 删除文件夹
    ///
    /// @param name 文件夹名称
    ///
    /// @return 成功 Ok(())
    /// @return 失败 Err(错误码)
    fn rmdir(&self, _name: &str) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// 将指定的`old_name`子目录项移动到target目录下, 并予以`new_name`。
    ///
    /// # Behavior
    /// 如果old_name所指向的inode与target的相同，那么则直接**执行重命名的操作**。
    fn move_to(
        &self,
        _old_name: &str,
        _target: &Arc<dyn IndexNode>,
        _new_name: &str,
        _flag: RenameFlags,
    ) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回"不支持"
        return Err(SystemError::ENOSYS);
    }

    /// @brief 专用于 remote 权限模型下 access(2) 的检查
    fn check_access(&self, _mask: PermissionMask) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// @brief 寻找一个名为Name的inode
    ///
    /// @param name 要寻找的inode的名称
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn find(&self, _name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 若文件系统没有实现此方法，则返回"不支持"
        return Err(SystemError::ENOSYS);
    }

    /// @brief 根据inode号，获取子目录项的名字
    ///
    /// @param ino inode号
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn get_entry_name(&self, _ino: InodeId) -> Result<String, SystemError> {
        // 若文件系统没有实现此方法，则返回"不支持"
        return Err(SystemError::ENOSYS);
    }

    /// @brief 根据inode号，获取子目录项的名字和元数据
    ///
    /// @param ino inode号
    ///
    /// @return 成功：Ok(String, Metadata)
    ///         失败：Err(错误码)
    fn get_entry_name_and_metadata(&self, ino: InodeId) -> Result<(String, Metadata), SystemError> {
        // 如果有条件，请在文件系统中使用高效的方式实现本接口，而不是依赖这个低效率的默认实现。
        let name = self.get_entry_name(ino)?;
        let entry = self.find(&name)?;
        return Ok((name, entry.metadata()?));
    }

    /// @brief io control接口
    ///
    /// @param cmd 命令
    /// @param data 数据
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn ioctl(
        &self,
        _cmd: u32,
        _data: usize,
        _private_data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // 若文件系统没有实现此方法，则返回"不支持"
        return Err(SystemError::ENOSYS);
    }

    /// @brief 获取inode所在的文件系统的指针
    fn fs(&self) -> Arc<dyn FileSystem>;

    /// @brief 获取当前 inode 所在挂载点的挂载标志
    fn mount_flags(&self) -> MountFlags {
        MountFlags::empty()
    }

    /// @brief 本函数用于实现动态转换。
    /// 具体的文件系统在实现本函数时，最简单的方式就是：直接返回self
    fn as_any_ref(&self) -> &dyn Any;

    /// @brief 列出当前inode下的所有目录项的名字
    fn list(&self) -> Result<Vec<String>, SystemError> {
        Err(SystemError::ENOTDIR)
    }

    /// # mount - 挂载文件系统
    ///
    /// 将给定的文件系统挂载到当前的文件系统节点上。
    ///
    /// 该函数是`MountFS`结构体的实例方法，用于将一个新的文件系统挂载到调用它的`MountFS`实例上。
    ///
    /// ## 参数
    ///
    /// - `fs`: `Arc<dyn FileSystem>` - 要挂载的文件系统的共享引用。
    ///
    /// ## 返回值
    ///
    /// - `Ok(Arc<MountFS>)`: 新的挂载文件系统的共享引用。
    /// - `Err(SystemError)`: 挂载过程中出现的错误。
    ///
    /// ## 错误处理
    ///
    /// - 如果文件系统不是目录类型，则返回`SystemError::ENOTDIR`错误。
    /// - 如果当前路径已经是挂载点，则返回`SystemError::EBUSY`错误。
    ///
    /// ## 副作用
    ///
    /// - 该函数会在`MountFS`实例上创建一个新的挂载点。
    /// - 该函数会在全局的挂载列表中记录新的挂载关系。
    fn mount(
        &self,
        _fs: Arc<dyn FileSystem>,
        _mount_flags: MountFlags,
    ) -> Result<Arc<MountFS>, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// # mount_from - 从给定的目录挂载已有挂载信息的文件系统
    ///
    /// 这个函数将一个已有挂载信息的文件系统从给定的目录挂载到当前目录。
    ///
    /// ## 参数
    ///
    /// - `from`: Arc<dyn IndexNode> - 要挂载的目录的引用。
    ///
    /// ## 返回值
    ///
    /// - Ok(Arc<MountFS>): 挂载的新文件系统的引用。
    /// - Err(SystemError): 如果发生错误，返回系统错误。
    ///
    /// ## 错误处理
    ///
    /// - 如果给定的目录不是目录类型，返回`SystemError::ENOTDIR`。
    /// - 如果当前目录已经是挂载点的根目录，返回`SystemError::EBUSY`。
    ///
    /// ## 副作用
    ///
    /// - 系统初始化用，其他情况不应调用此函数
    fn mount_from(&self, _des: Arc<dyn IndexNode>) -> Result<Arc<MountFS>, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// # umount - 卸载当前Inode下的文件系统
    ///
    /// 该函数是特定于`MountFS`实现的，其他文件系统不应实现此函数。
    ///
    /// ## 参数
    ///
    /// 无
    ///
    /// ## 返回值
    ///
    /// - Ok(Arc<MountFS>): 卸载的文件系统的引用。
    /// - Err(SystemError): 如果发生错误，返回系统错误。
    ///
    /// ## 行为
    ///
    /// - 查找路径
    /// - 定位到父文件系统的挂载点
    /// - 将挂载点与子文件系统的根进行叠加
    /// - 判断是否为子文件系统的根
    /// - 调用父文件系统挂载点的`_umount`方法进行卸载
    fn umount(&self) -> Result<Arc<MountFS>, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// Returns the absolute path of the inode.
    ///
    /// This function only works for `MountFS` and should not be implemented by other file systems.
    /// The performance of this function is O(n) for path queries, and it is extremely
    /// inefficient in file systems that do not implement DName caching.
    ///
    /// **WARNING**
    ///
    /// For special inodes(e.g., sockets,pipes, etc.), this function will
    /// return an special name according to the inode type directly.
    ///
    fn absolute_path(&self) -> Result<String, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// @brief 截断当前inode到指定的长度。如果当前文件长度小于len,则不操作。
    ///
    /// @param len 要被截断到的目标长度
    fn truncate(&self, _len: usize) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// @brief 将当前inode的内容同步到具体设备上
    fn sync(&self) -> Result<(), SystemError> {
        // todo：完善元数据的同步
        self.datasync()
    }

    /// @brief 基于打开文件上下文执行同步（可使用文件句柄等私有信息）
    ///
    /// 默认实现表示该 inode 没有提供 file-level fsync 操作。
    ///
    /// Linux 在 `file_operations.fsync` 缺失时返回 EINVAL。具体文件系统
    /// 需要显式覆盖该方法，避免 pipe/socket/eventfd 等特殊 fd 被错误放行。
    fn sync_file(
        &self,
        datasync: bool,
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        let _ = datasync;
        Err(SystemError::EINVAL)
    }

    /// 基于打开文件上下文同步指定文件字节范围（end 为包含端）。
    ///
    /// 默认回退到 whole-file fsync；支持页缓存范围写回的文件系统应覆盖此方法，
    /// 以匹配 Linux `vfs_fsync_range()` 在 msync/sync_file_range 场景下的范围语义。
    fn sync_file_range(
        &self,
        start: usize,
        end: usize,
        datasync: bool,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        let _ = (start, end);
        self.sync_file(datasync, data)
    }

    /// @brief 仅同步数据到磁盘（不包括元数据）
    ///
    /// O_DSYNC 语义：确保数据写入完成，但不保证元数据（如 mtime）更新
    /// 默认实现调用 sync（向后兼容）
    fn datasync(&self) -> Result<(), SystemError> {
        let page_cache = self.page_cache();
        if let Some(page_cache) = page_cache {
            return page_cache.manager().sync();
        }
        Ok(())
    }

    /// 将 inode 元数据（size/mtime 等）写入磁盘。
    ///
    /// 对齐 Linux `super_operations.write_inode`：在脏页回写完成后，
    /// 如果 inode 有脏元数据（I_DIRTY_SYNC / I_DIRTY_DATASYNC），
    /// VFS 调用此方法将元数据持久化。
    ///
    /// 默认 no-op——procfs/sysfs/pipe/socket 等无磁盘元数据的 inode 不需要覆盖。
    fn write_inode(&self, _wbc: &WritebackControl) -> Result<(), SystemError> {
        Ok(())
    }

    /// ## 创建一个特殊文件节点
    /// - _filename: 文件名
    /// - _mode: 权限信息
    fn mknod(
        &self,
        _filename: &str,
        _mode: InodeMode,
        _dev_t: DeviceNumber,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// # mkdir - 新建名称为`name`的目录项
    ///
    /// 当目录下已有名称为`name`的文件夹时，返回该目录项的引用；否则新建`name`文件夹，并返回该引用。
    ///
    /// 该函数会检查`name`目录是否已存在，如果存在但类型不为文件夹，则会返回`EEXIST`错误。
    ///
    /// # 参数
    ///
    /// - `name`: &str - 要新建的目录项的名称。
    /// - `mode`: InodeMode - 设置目录项的权限模式。
    ///
    /// # 返回值
    ///
    /// - `Ok(Arc<dyn IndexNode>)`: 成功时返回`name`目录项的共享引用。
    /// - `Err(SystemError)`: 出错时返回错误信息。
    fn mkdir(&self, name: &str, mode: InodeMode) -> Result<Arc<dyn IndexNode>, SystemError> {
        match self.find(name) {
            Ok(inode) => {
                if inode.metadata()?.file_type == FileType::Dir {
                    Ok(inode)
                } else {
                    Err(SystemError::EEXIST)
                }
            }
            Err(SystemError::ENOENT) => self.create(name, FileType::Dir, mode),
            Err(err) => Err(err),
        }
    }

    /// ## 返回特殊文件的inode
    fn special_node(&self) -> Option<SpecialNodeData> {
        None
    }

    /// # dname - 返回目录名
    ///
    /// 此函数用于返回一个目录名。
    ///
    /// ## 参数
    ///
    /// 无
    ///
    /// ## 返回值
    /// - Ok(DName): 成功时返回一个目录名。
    /// - Err(SystemError): 如果系统不支持此操作，则返回一个系统错误。
    fn dname(&self) -> Result<DName, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// # parent - 返回父目录的引用
    ///
    /// 当该目录是当前文件系统的根目录时，返回自身的引用。
    ///
    /// ## 参数
    ///
    /// 无
    ///
    /// ## 返回值
    ///
    /// - Ok(Arc<dyn IndexNode>): A reference to the parent directory
    /// - Err(SystemError): If there is an error in finding the parent directory
    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        return self.find("..");
    }

    fn page_cache(&self) -> Option<Arc<PageCache>> {
        // log::warn!(
        //     "function page_cache() has not yet been implemented for inode:{}",
        //     crate::libs::name::get_type_name(&self)
        // );
        None
    }

    /// Transform the inode to a pollable inode
    ///
    /// If the inode is not pollable, return an error
    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// @brief 按文件名获取扩展属性
    ///
    /// @param name 属性名称
    /// @param buf 用于存储扩展属性值的缓冲区
    ///
    /// @return 成功：Ok(属性值的实际长度)
    ///         失败：Err(错误码)
    fn getxattr(&self, _name: &str, _buf: &mut [u8]) -> Result<usize, SystemError> {
        log::warn!(
            "getxattr not implemented for {}",
            crate::libs::name::get_type_name(&self)
        );
        return Err(SystemError::ENOSYS);
    }

    /// @brief 按文件名设置扩展属性
    ///
    /// @param name 属性名称
    /// @param buf 用于存储扩展属性值的缓冲区
    /// @param value 要设置的扩展属性值
    ///
    /// @return 成功：Ok(0)
    ///         失败：Err(错误码)
    fn setxattr(
        &self,
        _name: &str,
        _value: &[u8],
        _flags: XattrFlags,
    ) -> Result<usize, SystemError> {
        log::warn!(
            "setxattr not implemented for {}",
            crate::libs::name::get_type_name(&self)
        );
        return Err(SystemError::ENOSYS);
    }

    /// @brief 列出扩展属性名，返回实际列表长度。
    fn listxattr(&self, _buf: &mut [u8]) -> Result<usize, SystemError> {
        log::warn!(
            "listxattr not implemented for {}",
            crate::libs::name::get_type_name(&self)
        );
        return Err(SystemError::ENOSYS);
    }

    /// @brief 删除指定扩展属性。
    fn removexattr(&self, _name: &str) -> Result<usize, SystemError> {
        log::warn!(
            "removexattr not implemented for {}",
            crate::libs::name::get_type_name(&self)
        );
        return Err(SystemError::ENOSYS);
    }

    /// # 将当前Inode转换为 Socket 引用
    ///
    /// # 返回值
    /// - Some(&dyn Socket): 当前Inode是Socket类型，返回其引用
    /// - None: 当前Inode不是Socket类型
    ///
    /// # 注意
    /// 这个方法已经为dyn Socket实现，
    /// 所以如果可以确定当前`dyn IndexNode`是`dyn Socket`类型，则可以直接调用此方法进行转换
    fn as_socket(&self) -> Option<&dyn Socket> {
        None
    }

    fn fadvise(
        &self,
        _file: &Arc<File>,
        _offset: i64,
        _len: i64,
        _advise: i32,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }
}

impl DowncastArc for dyn IndexNode {
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any> {
        self
    }
}

impl dyn IndexNode {
    /// @brief 将当前Inode转换为一个具体的结构体（类型由T指定）
    /// 如果类型正确，则返回Some,否则返回None
    pub fn downcast_ref<T: IndexNode>(&self) -> Option<&T> {
        return self.as_any_ref().downcast_ref::<T>();
    }

    /// @brief 查找文件（不考虑符号链接）
    ///
    /// @param path 文件路径
    ///
    /// @return Ok(Arc<dyn IndexNode>) 要寻找的目录项的inode
    /// @return Err(SystemError) 错误码
    pub fn lookup(&self, path: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        return self.lookup_follow_symlink(path, 0);
    }

    pub fn lookup_follow_symlink(
        &self,
        path: &str,
        max_follow_times: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        return self.do_lookup_follow_symlink(path, max_follow_times, true);
    }

    pub fn lookup_follow_symlink2(
        &self,
        path: &str,
        max_follow_times: usize,
        follow_final_symlink: bool,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        return self.do_lookup_follow_symlink(path, max_follow_times, follow_final_symlink);
    }

    /// # 查找文件
    /// 查找指定路径的文件，考虑符号链接的存在，并可选择是否返回最终路径的符号链接文件本身。
    ///
    /// ## 参数
    /// - `path`: 文件路径
    /// - `max_follow_times`: 最大经过的符号链接的数量
    /// - `follow_final_symlink`: 是否跟随最后的符号链接
    ///
    /// ## 返回值
    /// - `Ok(Arc<dyn IndexNode>)`: 要寻找的目录项的inode
    /// - `Err(SystemError)`: 错误码，表示查找过程中遇到的错误
    ///
    /// ## Safety
    /// 此函数在处理符号链接时可能会遇到循环引用的情况，`max_follow_times` 参数用于限制符号链接的跟随次数以避免无限循环。
    #[inline(never)]
    pub fn do_lookup_follow_symlink(
        &self,
        path: &str,
        max_follow_times: usize,
        follow_final_symlink: bool,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if self.metadata()?.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // Linux 语义：绝对路径应当以"进程 fs root"（可被 chroot 改变）为起点
        let process_root_inode = ProcessManager::current_pcb().fs_struct().root();
        let trailing_slash = path.ends_with('/');

        // 处理绝对路径
        // result: 上一个被找到的inode
        // rest_path: 还没有查找的路径
        let (mut result, mut rest_path) = if let Some(rest) = path.strip_prefix('/') {
            (process_root_inode.clone(), String::from(rest))
        } else {
            // 是相对路径
            (self.find(".")?, String::from(path))
        };

        let mut symlink_follows_remaining = max_follow_times;

        // 逐级查找文件
        while !rest_path.is_empty() {
            // 当前这一级不是文件夹
            if result.metadata()?.file_type != FileType::Dir {
                return Err(SystemError::ENOTDIR);
            }

            // 检查当前目录的执行权限（搜索权限）
            // 这确保了进程有权限遍历到此目录（对 Remote 权限模型的 FS，该检查会被绕过）
            let metadata = result.metadata()?;
            permission::check_inode_permission(&result, &metadata, PermissionMask::MAY_EXEC)?;

            let name;
            // 寻找"/"
            match rest_path.find('/') {
                Some(pos) => {
                    name = String::from(&rest_path[0..pos]);
                    rest_path = String::from(&rest_path[pos + 1..]);
                }
                None => {
                    name = rest_path;
                    rest_path = String::new();
                }
            }

            // 遇到连续多个"/"的情况
            if name.is_empty() {
                continue;
            }

            // 进程 root 边界：当解析到进程 root 时，".." 不允许逃逸，应当停留在 root。
            // 这对应 Linux 的路径解析语义（参照 namei.c 中对 root 的处理）。
            if name == ".." {
                let cur_md = result.metadata()?;
                let root_md = process_root_inode.metadata()?;
                if cur_md.dev_id == root_md.dev_id && cur_md.inode_id == root_md.inode_id {
                    continue;
                }
            }

            let inode = result.find(&name)?;
            let file_type = inode.metadata()?.file_type;
            // 如果已经是路径的最后一个部分，并且不希望跟随最后的符号链接
            if rest_path.is_empty() && !follow_final_symlink && file_type == FileType::SymLink {
                // Linux 语义：若 pathname 以 '/' 结尾，则必须解析为目录，
                // 此时即使请求"不跟随最终 symlink"，也不能返回 symlink 本身。
                if !trailing_slash {
                    // 返回符号链接本身
                    return Ok(inode);
                }
            }

            // 跟随符号链接跳转
            if file_type == FileType::SymLink {
                // 需要跟随 symlink 的场景：
                // - symlink 位于路径中间（rest_path 非空）
                // - 需要跟随最终 symlink（follow_final_symlink=true）
                // - 或者 pathname 以 '/' 结尾（trailing_slash=true）
                let need_follow = !rest_path.is_empty()
                    || follow_final_symlink
                    || (trailing_slash && rest_path.is_empty());

                // 兼容旧语义：symlink_follows_remaining==0 表示完全不跟随 symlink。
                // 在这种模式下，如果路径解析"需要跟随"（例如 symlink 位于中间，或末尾带 '/'），
                // 我们保持旧行为：把 symlink 当作普通 inode 继续推进，后续通常会因非目录而 ENOTDIR。
                if symlink_follows_remaining == 0 {
                    result = inode;
                    continue;
                }

                // Linux 语义：超过最大符号链接层数应返回 ELOOP。
                // 根据上面的约定：symlink_follows_remaining==1 表示计数已耗尽，不允许再跟随。
                if need_follow && symlink_follows_remaining == 1 {
                    return Err(SystemError::ELOOP);
                }

                // 若不需要跟随（理论上只可能发生在"末尾 symlink + 不跟随 + 无 trailing '/'"），
                // 则 result=inode 由循环末尾处理即可。
                if !need_follow {
                    result = inode;
                    continue;
                }

                symlink_follows_remaining -= 1;

                // 首先检查是否是"魔法链接"（如 /proc/self/fd/N）
                // 这些链接的 readlink 返回的路径可能不可解析（如 pipe:[xxx]），
                // 但它们有一个 special_node 指向真实的 inode
                if let Some(SpecialNodeData::Reference(target_inode)) = inode.special_node() {
                    if rest_path.is_empty() {
                        return Ok(target_inode);
                    } else {
                        // 将 result 设为 magic link 的目标 inode，继续迭代
                        result = target_inode;
                        continue;
                    }
                }

                let mut content = [0u8; 256];
                // 读取符号链接
                // TODO:We need to clarify which interfaces require private data and which do not
                let len = inode.read_at(
                    0,
                    256,
                    &mut content,
                    Mutex::new(FilePrivateData::Unused).lock(),
                )?;

                // 将读到的数据转换为utf8字符串（先转为str，再转为String）
                let link_path = String::from(
                    ::core::str::from_utf8(&content[..len]).map_err(|_| SystemError::EINVAL)?,
                );

                // 拼接路径：将 symlink 目标 + 剩余路径组合
                let new_path = if rest_path.is_empty() {
                    link_path
                } else {
                    link_path + "/" + &rest_path
                };

                // 处理 symlink 目标为绝对路径或相对路径
                // 绝对路径：从进程 root 开始
                // 相对路径：从当前 result（symlink 所在目录）开始
                if let Some(rest) = new_path.strip_prefix('/') {
                    result = process_root_inode.clone();
                    rest_path = String::from(rest);
                } else {
                    rest_path = new_path;
                }

                // 继续迭代（不递归）
                continue;
            }

            result = inode;
        }

        if trailing_slash && result.metadata()?.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        return Ok(result);
    }
}

/// IndexNode的元数据
///
/// 对应Posix2008中的sys/stat.h中的定义 https://pubs.opengroup.org/onlinepubs/9699919799/basedefs/sys_stat.h.html
#[derive(Debug, PartialEq, Eq, Clone, Builder)]
#[builder(no_std, setter(into))]
pub struct Metadata {
    /// 当前inode所在的文件系统的设备号
    /// todo:更改为DeviceNumber结构体
    pub dev_id: usize,

    /// inode号
    pub inode_id: InodeId,

    /// Inode的大小
    /// 文件：文件大小（单位：字节）
    /// 目录：目录项中的文件、文件夹数量
    pub size: i64,

    /// Inode所在的文件系统中，每个块的大小
    pub blk_size: usize,

    /// Inode所占的块的数目
    pub blocks: usize,

    /// inode最后一次被访问的时间
    pub atime: PosixTimeSpec,

    /// inode的文件数据最后一次修改的时间
    pub mtime: PosixTimeSpec,

    /// inode的元数据、权限或文件内容最后一次发生改变的时间
    pub ctime: PosixTimeSpec,

    /// inode的创建时间
    pub btime: PosixTimeSpec,

    /// 文件类型
    pub file_type: FileType,

    /// 权限
    pub mode: InodeMode,

    /// inode运行时状态
    pub flags: InodeFlags,

    /// 硬链接的数量
    pub nlinks: usize,

    /// User ID
    pub uid: usize,

    /// Group ID
    pub gid: usize,

    /// 文件指向的设备的id（对于设备文件系统来说）
    pub raw_dev: DeviceNumber,
}

impl Default for Metadata {
    fn default() -> Self {
        return Self {
            dev_id: 0,
            inode_id: InodeId::new(0),
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: PosixTimeSpec::default(),
            mtime: PosixTimeSpec::default(),
            ctime: PosixTimeSpec::default(),
            btime: PosixTimeSpec::default(),
            file_type: FileType::File,
            mode: InodeMode::empty(),
            flags: InodeFlags::empty(),
            nlinks: 1,
            uid: 0,
            gid: 0,
            raw_dev: DeviceNumber::default(),
        };
    }
}

#[derive(Debug, Clone)]
pub struct SuperBlock {
    // type of filesystem
    pub magic: Magic,
    // optimal transfer block size
    pub bsize: u64,
    // total data blocks in filesystem
    pub blocks: u64,
    // free block in system
    pub bfree: u64,
    // 可供非特权用户使用的空闲块
    pub bavail: u64,
    // total inodes in filesystem
    pub files: u64,
    // free inodes in filesystem
    pub ffree: u64,
    // filesysytem id
    pub fsid: u64,
    // Max length of filename
    pub namelen: u64,
    // fragment size
    pub frsize: u64,
    // mount flags of filesystem
    pub flags: u64,
}

pub struct FsReconfigureRequest<'a> {
    pub sb_flags: MountFlags,
    pub sb_flags_mask: MountFlags,
    pub raw_data: Option<&'a str>,
    pub oldapi: bool,
}

impl SuperBlock {
    pub fn new(magic: Magic, bsize: u64, namelen: u64) -> Self {
        Self {
            magic,
            bsize,
            blocks: 0,
            bfree: 0,
            bavail: 0,
            files: 0,
            ffree: 0,
            fsid: 0,
            namelen,
            frsize: 0,
            flags: 0,
        }
    }
}
bitflags! {
    pub struct Magic: u64 {
        const DEVFS_MAGIC = 0x1373;
        const FAT_MAGIC =  0xf2f52011;
        const EXT4_MAGIC = 0xef53;
        const FUSE_MAGIC = 0x65735546;
        const TMPFS_MAGIC = 0x01021994;
        const KER_MAGIC = 0x3153464b;
        const CGROUP2_SUPER_MAGIC = 0x63677270;
        const PROC_MAGIC = 0x9fa0;
        const RAMFS_MAGIC = 0x858458f6;
        const DEVPTS_MAGIC = 0x1cd1;
        const DEBUGFS_MAGIC = 0x64626720;
        const MQUEUE_MAGIC = 0x19800202;
        const MOUNT_MAGIC = 61267;
        const PIPEFS_MAGIC = 0x50495045;
        const EVENTFD_MAGIC = 0x45564446; // "EVDF" in ASCII
        const OVERLAYFS_MAGIC = 0x794c7630;
    }
}

/// Filesystem-level permission checking policy used by VFS.
///
/// - `Dac`: VFS performs Unix DAC permission checks (mode/uid/gid) locally.
/// - `Remote`: VFS bypasses local DAC checks and lets the filesystem/server decide.
///   For Linux FUSE remote model, execute permission is still checked locally for
///   regular files; see `vfs::permission::check_inode_permission()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsPermissionPolicy {
    Dac,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritebackSyncMode {
    None,
    All,
}

bitflags! {
    /// Flags controlling Linux extended attribute set semantics.
    pub struct XattrFlags: i32 {
        const CREATE = 0x1;
        const REPLACE = 0x2;
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WritebackControl {
    pub sync_mode: WritebackSyncMode,
    pub for_sync: bool,
}

impl WritebackControl {
    pub const fn sync_all_for_sync() -> Self {
        Self {
            sync_mode: WritebackSyncMode::All,
            for_sync: true,
        }
    }

    pub const fn sync_none() -> Self {
        Self {
            sync_mode: WritebackSyncMode::None,
            for_sync: false,
        }
    }
}

/// @brief 所有文件系统都应该实现的trait
pub trait FileSystem: Any + Sync + Send + Debug {
    /// @brief 获取当前文件系统的root inode的指针
    fn root_inode(&self) -> Arc<dyn IndexNode>;

    /// @brief 获取当前文件系统的信息
    fn info(&self) -> FsInfo;

    /// @brief 文件系统是否支持 readahead
    ///
    /// 对于内存文件系统（如 tmpfs），数据已经在 page_cache 中，不需要 readahead
    /// 对于磁盘文件系统（如 ext4、fat），需要从磁盘预读数据，应该支持 readahead
    fn support_readahead(&self) -> bool {
        true // 默认支持 readahead
    }

    /// @brief 本函数用于实现动态转换。
    /// 具体的文件系统在实现本函数时，最简单的方式就是：直接返回self
    fn as_any_ref(&self) -> &dyn Any;

    fn name(&self) -> &str;

    fn super_block(&self) -> SuperBlock;

    /// @brief 获取文件系统统计信息（statfs）
    ///
    /// 默认实现直接返回 super_block。需要自定义 statfs 行为的文件系统可覆写此方法。
    fn statfs(&self, _inode: &Arc<dyn IndexNode>) -> Result<SuperBlock, SystemError> {
        Ok(self.super_block())
    }

    fn reconfigure(&self, request: FsReconfigureRequest<'_>) -> Result<MountFlags, SystemError> {
        if request.raw_data.is_some_and(|raw| !raw.trim().is_empty()) {
            return Err(SystemError::EINVAL);
        }
        Ok(request.sb_flags & request.sb_flags_mask)
    }

    /// VFS permission checking policy for this filesystem instance.
    ///
    /// Default is `Dac` (local Unix DAC checks).
    fn permission_policy(&self) -> FsPermissionPolicy {
        FsPermissionPolicy::Dac
    }

    /// Render the device/source field used by procfs mount exports.
    fn proc_show_devname(&self, mount: &MountFS, out: &mut dyn Write) -> Result<(), SystemError> {
        if let Some(source) = mount.mount_source() {
            out.write_str(&source).map_err(|_| SystemError::EINVAL)?;
        } else {
            out.write_str(self.name())
                .map_err(|_| SystemError::EINVAL)?;
        }
        Ok(())
    }

    /// Render extra mount options for `/proc/*/mounts` and `mountinfo`.
    fn proc_show_mount_options(
        &self,
        _mount: &MountFS,
        _out: &mut dyn Write,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    /// Render the mount root field used by `/proc/*/mountinfo`.
    fn proc_show_mountinfo_root(
        &self,
        mount: &MountFS,
        out: &mut dyn Write,
    ) -> Result<(), SystemError> {
        match mount.root_inner_inode().absolute_path() {
            Ok(root) if !root.is_empty() => out.write_str(&root).map_err(|_| SystemError::EINVAL),
            _ => out.write_char('/').map_err(|_| SystemError::EINVAL),
        }
    }

    /// Render fs-specific stats for `/proc/*/mountstats`.
    ///
    /// Returns `true` if any fs-specific payload was written.
    fn proc_show_mount_stats(
        &self,
        _mount: &MountFS,
        _out: &mut dyn Write,
    ) -> Result<bool, SystemError> {
        Ok(false)
    }

    /// Called after a filesystem is successfully unmounted.
    /// Default is no-op.
    fn on_umount(&self) {}

    /// super_operations.sync_fs 在 sync() 回写脏页后调用，刷新文件系统元数据。
    fn sync_fs(&self, wait: bool) -> Result<(), SystemError> {
        let _ = wait;
        Ok(())
    }

    unsafe fn fault(&self, _pfm: &mut PageFaultMessage) -> VmFaultReason {
        VmFaultReason::VM_FAULT_SIGBUS
    }

    /// Called before a shared writable file mapping is made writable and dirty.
    ///
    /// Filesystems that need writeback handles, size validation, or remote
    /// permission checks should override this hook. Returning an error fault
    /// keeps the PTE read-only and prevents the page from being marked dirty.
    unsafe fn page_mkwrite(&self, _pfm: &mut PageFaultMessage) -> VmFaultReason {
        VmFaultReason::VM_FAULT_SIGBUS
    }

    fn mprotect(&self, _old_vm_flags: VmFlags, _new_vm_flags: VmFlags) -> Result<(), SystemError> {
        Ok(())
    }

    /// Called when a file-backed VMA range is genuinely detached from an address space.
    ///
    /// This is not called for VMA split/reinsert used by mprotect-like metadata
    /// changes. Filesystems may use it to flush dirty shared mappings before
    /// the last mapping reference disappears.
    fn vma_close(&self, _file: &Arc<File>, _region: VirtRegion, _vm_flags: VmFlags) {}

    unsafe fn map_pages(
        &self,
        _pfm: &mut PageFaultMessage,
        _start_pgoff: usize,
        _end_pgoff: usize,
    ) -> VmFaultReason {
        panic!(
            "map_pages() has not yet been implemented for filesystem: {}",
            crate::libs::name::get_type_name(&self)
        )
    }
}

impl DowncastArc for dyn FileSystem {
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any> {
        self
    }
}

/// # 可以被挂载的文件系统应该实现的trait
pub trait MountableFileSystem: FileSystem {
    fn make_mount_data(
        _raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        log::error!("This filesystem does not support make_mount_data");
        Err(SystemError::ENOSYS)
    }

    fn make_fs(
        _data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        log::error!("This filesystem does not support make_fs");
        Err(SystemError::ENOSYS)
    }
}

/// # 注册一个可以被挂载文件系统
/// 此宏用于注册一个可以被挂载的文件系统。
/// 它会将文件系统的创建函数和挂载数据创建函数注册到全局的`FSMAKER`数组中。
///
/// ## 参数
/// - `$fs`: 文件系统对应的结构体
/// - `$maker_name`: 文件系统的注册名
/// - `$fs_name`: 文件系统的名称（字符串字面量）
#[macro_export]
macro_rules! register_mountable_fs {
    ($fs:ident, $maker_name:ident, $fs_name:literal) => {
        impl $fs {
            fn make_fs_bridge(
                data: Option<&dyn FileSystemMakerData>,
            ) -> Result<Arc<dyn FileSystem>, SystemError> {
                <$fs as MountableFileSystem>::make_fs(data)
            }

            fn make_mount_data_bridge(
                raw_data: Option<&str>,
                source: &str,
            ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
                <$fs as MountableFileSystem>::make_mount_data(raw_data, source)
            }
        }

        #[distributed_slice(FSMAKER)]
        static $maker_name: $crate::filesystem::vfs::FileSystemMaker =
            $crate::filesystem::vfs::FileSystemMaker::new(
                $fs_name,
                &($fs::make_fs_bridge
                    as fn(
                        Option<&dyn FileSystemMakerData>,
                    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError>),
                &($fs::make_mount_data_bridge
                    as fn(
                        Option<&str>,
                        &str,
                    )
                        -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError>),
            );
    };
}

#[derive(Debug)]
pub struct FsInfo {
    /// 文件系统所在的块设备的id
    pub blk_dev_id: usize,
    /// 文件名的最大长度
    pub max_name_len: usize,
}

impl Metadata {
    pub fn new(file_type: FileType, mode: InodeMode) -> Self {
        Metadata {
            dev_id: 0,
            inode_id: generate_inode_id(),
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: PosixTimeSpec::default(),
            mtime: PosixTimeSpec::default(),
            ctime: PosixTimeSpec::default(),
            btime: PosixTimeSpec::default(),
            file_type,
            mode,
            flags: InodeFlags::empty(),
            nlinks: 1,
            uid: 0,
            gid: 0,
            raw_dev: DeviceNumber::default(),
        }
    }
}
pub struct FileSystemMaker {
    /// 文件系统的创建函数
    maker: &'static FSMakerFunction,
    /// 文件系统的名称
    name: &'static str,
    /// 用于创建挂载数据的函数
    builder: &'static MountDataBuilder,
}

impl FileSystemMaker {
    pub const fn new(
        name: &'static str,
        maker: &'static FSMakerFunction,
        builder: &'static MountDataBuilder,
    ) -> FileSystemMaker {
        FileSystemMaker {
            maker,
            name,
            builder,
        }
    }

    pub fn build(
        &self,
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem>, SystemError> {
        (self.maker)(data)
    }
}

pub trait FileSystemMakerData: Send + Sync {
    fn as_any(&self) -> &dyn Any;
}

pub type FSMakerFunction =
    fn(data: Option<&dyn FileSystemMakerData>) -> Result<Arc<dyn FileSystem>, SystemError>;
pub type MountDataBuilder =
    fn(
        raw_data: Option<&str>,
        source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError>;

#[macro_export]
macro_rules! define_filesystem_maker_slice {
    ($name:ident) => {
        #[::linkme::distributed_slice]
        pub static $name: [FileSystemMaker] = [..];
    };
    () => {
        compile_error!("define_filesystem_maker_slice! requires at least one argument: slice_name");
    };
}

/// # 通过文件系统的名称和数据创建一个文件系统实例
///
/// ## 参数
/// - `filesystem`: 文件系统的名称
/// - `data`: 可选的挂载数据
/// - `source`: 挂载源
///
/// ## 返回值
/// - `Ok(Arc<dyn FileSystem>)`: 成功时返回文件系统的共享引用
/// - `Err(SystemError)`: 如果找不到对应的文件系统或创建失败，则返回错误
///
/// 这个是之前的`produce_fs!`的函数版本，改成了函数之后ext4的挂载会慢一点，仅作记录
pub fn produce_fs(
    filesystem: &str,
    data: Option<&str>,
    source: &str,
) -> Result<Arc<dyn FileSystem>, SystemError> {
    let canonical_filesystem = if filesystem.starts_with("fuse.") {
        "fuse"
    } else {
        filesystem
    };

    match FSMAKER.iter().find(|&m| m.name == canonical_filesystem) {
        Some(maker) => {
            let mount_data = (maker.builder)(data, source)?;
            let mount_data_ref = mount_data.as_ref().map(|arc| arc.as_ref());
            maker.build(mount_data_ref)
        }
        None => {
            log::error!("mismatch filesystem type : {}", filesystem);
            Err(SystemError::EINVAL)
        }
    }
}

define_filesystem_maker_slice!(FSMAKER);

/// Dirent 格式类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirentFormat {
    /// 旧格式 getdents (linux_dirent)，不包含 d_type 字段
    Getdents,
    /// 新格式 getdents64 (linux_dirent64)，包含 d_type 字段
    Getdents64,
}

/// # 批量填充Dirent时的上下文Add commentMore actions
/// linux语义是通过getdents_callback *类型来实现类似链表的迭代填充，这里考虑通过填充传入的缓冲区来实现
pub struct FilldirContext<'a> {
    user_buf: UserBuffer<'a>,
    current_pos: usize,
    remain_size: usize,
    error: Option<SystemError>,
    format: DirentFormat,
}

impl<'a> FilldirContext<'a> {
    pub fn new(user_buf: UserBuffer<'a>, format: DirentFormat) -> Self {
        let len = user_buf.len();
        Self {
            remain_size: len,
            user_buf,
            current_pos: 0,
            error: None,
            format,
        }
    }

    /// # 填充单个dirent结构体
    ///
    /// ## 参数
    /// - name 目录项名称
    /// - offset 当前目录项偏移量
    /// - ino 目录项的inode的inode_id
    /// - d_type 目录项的inode的file_type_num
    pub(crate) fn fill_dir(
        &mut self,
        name: &str,
        offset: usize,
        ino: u64,
        d_type: u8,
    ) -> Result<(), SystemError> {
        let name_len = name.len();
        let name_bytes = name.as_bytes();

        // 根据格式计算基础结构大小
        // linux_dirent (旧格式): d_ino(8) + d_off(8) + d_reclen(2) = 18 bytes
        // linux_dirent64 (新格式): d_ino(8) + d_off(8) + d_reclen(2) + d_type(1) = 19 bytes
        let base_size = match self.format {
            DirentFormat::Getdents => 18,   // d_ino + d_off + d_reclen
            DirentFormat::Getdents64 => 19, // d_ino + d_off + d_reclen + d_type
        };

        // 计算总长度：基础结构 + 文件名 + null terminator
        let total_size = base_size + name_len + 1;

        // 对齐到 8 字节（Linux 要求 d_reclen 必须是 8 的倍数）
        const ALIGN: usize = 8;
        let align_up = |len: usize| -> usize { (len + ALIGN - 1) & !(ALIGN - 1) };
        let align_up_reclen = align_up(total_size);

        // 检查缓冲区空间是否足够
        if align_up_reclen > self.remain_size {
            self.error = Some(SystemError::EINVAL);
            return Err(SystemError::EINVAL);
        }

        // 获取当前写入位置的偏移量
        let buf_start = self.current_pos;
        // 在内核空间构建完整的 dirent 数据
        let mut dirent_data = vec![0u8; align_up_reclen];

        // 根据格式填充结构
        match self.format {
            DirentFormat::Getdents => {
                // linux_dirent 格式：
                // d_ino: unsigned long (8 bytes)
                // d_off: unsigned long (8 bytes)
                // d_reclen: unsigned short (2 bytes)
                // d_name[0]: char[] (可变长度)

                // 写入 d_ino (offset 0, 8 bytes)
                dirent_data[0..8].copy_from_slice(&ino.to_le_bytes());

                // 写入 d_off (offset 8, 8 bytes) - 注意：旧格式使用 unsigned long
                let d_off = offset as u64;
                dirent_data[8..16].copy_from_slice(&d_off.to_le_bytes());

                // 写入 d_reclen (offset 16, 2 bytes)
                dirent_data[16..18].copy_from_slice(&(align_up_reclen as u16).to_le_bytes());

                // 写入 d_name (offset 18)
                dirent_data[18..18 + name_len].copy_from_slice(name_bytes);
                dirent_data[18 + name_len] = 0; // null terminator
            }
            DirentFormat::Getdents64 => {
                // linux_dirent64 格式：
                // d_ino: uint64_t (8 bytes)
                // d_off: int64_t (8 bytes)
                // d_reclen: unsigned short (2 bytes)
                // d_type: unsigned char (1 byte)
                // d_name[0]: char[] (可变长度)

                // 写入 d_ino (offset 0, 8 bytes)
                dirent_data[0..8].copy_from_slice(&ino.to_le_bytes());

                // 写入 d_off (offset 8, 8 bytes) - 注意：新格式使用 int64_t
                let d_off = offset as i64;
                dirent_data[8..16].copy_from_slice(&d_off.to_le_bytes());

                // 写入 d_reclen (offset 16, 2 bytes)
                dirent_data[16..18].copy_from_slice(&(align_up_reclen as u16).to_le_bytes());

                // 写入 d_type (offset 18, 1 byte)
                dirent_data[18] = d_type;

                // 写入 d_name (offset 19)
                dirent_data[19..19 + name_len].copy_from_slice(name_bytes);
                dirent_data[19 + name_len] = 0; // null terminator
            }
        }
        // 使用受保护的方法写入用户缓冲区
        // 如果写入失败（例如访问不可写页面），应当返回 EFAULT
        if let Err(_e) = self.user_buf.write_to_user(buf_start, &dirent_data) {
            self.error = Some(SystemError::EFAULT);
            return Err(SystemError::EFAULT);
        }
        // 更新位置
        self.current_pos += align_up_reclen;
        self.remain_size -= align_up_reclen;

        Ok(())
    }
}
