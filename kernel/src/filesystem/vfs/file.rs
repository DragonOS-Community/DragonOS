use core::{
    fmt,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::filesystem::fsnotify::{self, FsEvent};
use alloc::{string::String, sync::Arc};
use log::error;
use system_error::SystemError;

use crate::cgroup::memory::{
    accounting::KernelByteKind,
    object_for_task,
    owned::{AllocatedBox, OwnedBox},
};
use crate::mm::allocation::MemoryAllocationError;

mod allocation;
mod io;
mod position;
pub(crate) mod selftest;
pub(crate) use allocation::PreparedFile;

use super::{
    append_lock::{with_inode_append_lock, AppendLockKey},
    inode_lifecycle::{InodeRetentionGuard, InodeRetentionKind},
    mount::{
        writer::{is_special_file_type, MountWriteGuard},
        MountExternalGuard, MountFSInode, MountFlags,
    },
    utils::should_remove_sgid,
    write_access::InodeWriteGuard,
    DirectoryEntry, FileSystem, FileType, IndexNode, InodeId, Metadata, PrimaryWriteSource,
    SetMetadataMask, SpecialNodeData, WritePrivilegeStage,
};
use crate::{arch::ipc::signal::Signal, filesystem::vfs::InodeFlags, process::pid::PidPrivateData};
use crate::{
    arch::MMArch,
    driver::{
        base::{block::SeekFrom, device::DevicePrivateData},
        block::loop_device::LoopPrivateData,
        tty::tty_device::TtyFilePrivateData,
    },
    filesystem::{
        devfs::{devfs_lookup_device_by_devnum, LockedDevFSInode},
        epoll::{
            event_poll::{EPollItemList, EPollPrivateData, EventPoll},
            EPollItem,
        },
        ext4::inode::LockedExt4Inode,
        fat::fs::LockedFATInode,
        fuse::private_data::FuseFilePrivateData,
        kernfs::callback::KernFilePrivateData,
        overlayfs::OverlayFilePrivateData,
        page_cache::PageCache,
        procfs::ProcfsFilePrivateData,
        ramfs::LockedRamFSInode,
        tmpfs::LockedTmpfsInode,
        vfs::FilldirContext,
    },
    ipc::{kill::send_signal_to_pid, pipe::PipeFsPrivateData},
    libs::{casting::DowncastArc, errseq::ErrSeqValue, mutex::Mutex, rwsem::RwSem},
    mm::{
        page::PageFlags,
        readahead::{page_cache_async_readahead, page_cache_sync_readahead, FileReadaheadState},
        MemoryManagementArch,
    },
    process::{
        cred::{CAPFlags, Cred},
        namespace::{
            cgroup_namespace::CgroupNamespace, ipc_namespace::IpcNamespace, mnt::MntNamespace,
            net_namespace::NetNamespace, pid_namespace::PidNamespace,
            user_namespace::UserNamespace, uts_namespace::UtsNamespace,
        },
        resource::RLimitID,
        ProcessControlBlock, ProcessManager, RawPid,
    },
    syscall::{user_access::UserBufferReader, user_buffer::UserBuffer},
};

use crate::filesystem::vfs::InodeMode;

const MAX_LFS_FILESIZE: i64 = i64::MAX;
static NEXT_OPEN_FILE_ID: AtomicUsize = AtomicUsize::new(1);

#[inline]
fn alloc_open_file_id() -> usize {
    NEXT_OPEN_FILE_ID.fetch_add(1, Ordering::Relaxed)
}

fn canonical_inode_for_file_lock(mut inode: Arc<dyn IndexNode>) -> Arc<dyn IndexNode> {
    loop {
        match inode.clone().downcast_arc::<MountFSInode>() {
            Some(mnt_inode) => inode = mnt_inode.underlying_inode(),
            None => return inode,
        }
    }
}

/// Append-lock domain and the owner that keeps its allocation identity alive.
///
/// The inode part of the key is deliberately derived from current metadata at
/// write time. A stacked filesystem may change backing identity during open
/// (for example, overlayfs copy-up), so caching the complete key here can split
/// concurrent appenders across two locks.
#[derive(Clone, Debug)]
struct AppendLockDomain {
    fs_instance: usize,
    fs_owner: Arc<dyn FileSystem>,
}

impl AppendLockDomain {
    fn new(fs_owner: Arc<dyn FileSystem>) -> Self {
        let fs_instance = Arc::as_ptr(&fs_owner) as *const () as usize;
        Self {
            fs_instance,
            fs_owner,
        }
    }

    fn key(&self, metadata: &Metadata) -> AppendLockKey {
        debug_assert_eq!(
            self.fs_instance,
            Arc::as_ptr(&self.fs_owner) as *const () as usize
        );
        AppendLockKey::new(self.fs_instance, metadata.dev_id, metadata.inode_id)
    }
}

fn is_plain_special_inode(inode: &Arc<dyn IndexNode>) -> bool {
    inode
        .as_any_ref()
        .downcast_ref::<LockedDevFSInode>()
        .is_some()
        || inode
            .as_any_ref()
            .downcast_ref::<LockedTmpfsInode>()
            .is_some()
        || inode
            .as_any_ref()
            .downcast_ref::<LockedRamFSInode>()
            .is_some()
        || inode
            .as_any_ref()
            .downcast_ref::<LockedExt4Inode>()
            .is_some()
        || inode
            .as_any_ref()
            .downcast_ref::<LockedFATInode>()
            .is_some()
}

fn resolve_device_special_inode(
    inode: Arc<dyn IndexNode>,
    file_type: FileType,
    path_metadata: Option<&Metadata>,
) -> Result<Arc<dyn IndexNode>, SystemError> {
    if !matches!(file_type, FileType::CharDevice | FileType::BlockDevice) {
        return Ok(inode);
    }

    let raw_dev = match path_metadata {
        Some(metadata) => metadata.raw_dev,
        None => inode.metadata()?.raw_dev,
    };
    if raw_dev == Default::default() {
        return Ok(inode);
    }

    if is_plain_special_inode(&inode) {
        if let Some(device_inode) = devfs_lookup_device_by_devnum(file_type, raw_dev) {
            return Ok(device_inode);
        }

        return Err(SystemError::ENXIO);
    }

    Ok(inode)
}

#[derive(Clone, Copy, Debug)]
enum OffsetUpdate {
    /// Sequential write path: advance file offset by written length.
    Add,
    /// Append path: set file offset to end-of-write (actual_offset + written_len).
    StoreEnd,
}

#[derive(Clone, Debug)]
pub struct FileOwnerSnapshot {
    pub pcb: Option<Arc<ProcessControlBlock>>,
    pub signum: i32,
}

#[derive(Debug)]
struct FileOwner {
    pcb: Option<Arc<ProcessControlBlock>>,
    signum: i32,
}

impl FileOwner {
    fn new() -> Self {
        Self {
            pcb: None,
            signum: 0,
        }
    }

    fn snapshot(&self) -> FileOwnerSnapshot {
        FileOwnerSnapshot {
            pcb: self.pcb.clone(),
            signum: self.signum,
        }
    }
}

/// 写操作的配置参数
#[derive(Clone, Copy)]
struct WriteConfig {
    /// 是否更新文件偏移量
    update_offset: bool,
    /// 偏移量更新方式
    offset_update: OffsetUpdate,
    /// Only consumed by a delegated write operation.
    sync_intent: WriteSyncIntent,
    /// Whether this operation owns the data-content MODIFY notification.
    emit_data_fsnotify: bool,
}

#[derive(Clone, Copy)]
struct WriteCompletionPolicy {
    sync_intent: WriteSyncIntent,
    emit_data_fsnotify: bool,
}

/// Determines which write operation owns Linux post-write synchronous-I/O
/// semantics for one open file description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostWriteSyncPolicy {
    /// The VFS calls the file-level range fsync operation after a positive write.
    Generic,
    /// The selected write operation consumes [`WriteSyncIntent`] itself.
    Delegated,
    /// The selected Linux write operation does not use `generic_write_sync()`.
    NotApplicable,
}

/// Per-open behavior selected after the inode has initialized private data.
#[derive(Debug, Clone, Copy)]
pub struct OpenFileBehavior {
    pub mode: FileMode,
    pub post_write_sync: PostWriteSyncPolicy,
}

impl OpenFileBehavior {
    fn new(mode: FileMode, file_type: FileType) -> Self {
        let post_write_sync = match file_type {
            FileType::File | FileType::BlockDevice => PostWriteSyncPolicy::Generic,
            _ => PostWriteSyncPolicy::NotApplicable,
        };
        Self {
            mode,
            post_write_sync,
        }
    }
}

/// Synchronization requested as part of a delegated write operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteSyncIntent {
    None,
    Data,
    Full,
}

impl WriteSyncIntent {
    pub fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Full, _) | (_, Self::Full) => Self::Full,
            (Self::Data, _) | (_, Self::Data) => Self::Data,
            _ => Self::None,
        }
    }
}

/// A delegated write reports data progress separately from its sync result.
///
/// This preserves Linux's ordering: offset and write side effects are finalized
/// after a positive write even when the following synchronous flush fails.
#[derive(Debug)]
pub struct DelegatedWriteResult {
    pub written_len: usize,
    pub sync_result: Result<(), MemoryAllocationError>,
    /// The completed data range, filled by File after dropping its append
    /// guard. Recovery retries only synchronization, never the primary write.
    pub(crate) sync_range: Option<(usize, usize, bool)>,
}
/// Namespace fd backing data, typically created from /proc/thread-self/ns/* files.
#[derive(Clone)]
#[allow(dead_code)]
pub enum NamespaceFilePrivateData {
    Ipc(Arc<IpcNamespace>),
    Uts(Arc<UtsNamespace>),
    Mnt(Arc<MntNamespace>),
    Net(Arc<NetNamespace>),
    /// Current thread PID namespace.
    Pid(Arc<PidNamespace>),
    /// PID namespace for children.
    PidForChildren(Arc<PidNamespace>),
    User(Arc<UserNamespace>),
    /// Cgroup namespace.
    Cgroup(Arc<CgroupNamespace>),
    // Time namespace is not implemented yet.
}

impl fmt::Debug for NamespaceFilePrivateData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NamespaceFilePrivateData::Ipc(_) => f.write_str("NamespaceFilePrivateData::Ipc(..)"),
            NamespaceFilePrivateData::Uts(_) => f.write_str("NamespaceFilePrivateData::Uts(..)"),
            NamespaceFilePrivateData::Mnt(_) => f.write_str("NamespaceFilePrivateData::Mnt(..)"),
            NamespaceFilePrivateData::Net(_) => f.write_str("NamespaceFilePrivateData::Net(..)"),
            NamespaceFilePrivateData::Pid(_) => f.write_str("NamespaceFilePrivateData::Pid(..)"),
            NamespaceFilePrivateData::PidForChildren(_) => {
                f.write_str("NamespaceFilePrivateData::PidForChildren(..)")
            }
            NamespaceFilePrivateData::User(_) => f.write_str("NamespaceFilePrivateData::User(..)"),
            NamespaceFilePrivateData::Cgroup(_) => {
                f.write_str("NamespaceFilePrivateData::Cgroup(..)")
            }
        }
    }
}

/// 文件私有信息的枚举类型
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum FilePrivateData {
    /// Cgroup migration uses the opener's credentials and cgroup namespace.
    Cgroup(crate::filesystem::cgroup2::CgroupOpenState),
    /// 管道文件私有信息
    Pipefs(PipeFsPrivateData),
    /// procfs文件私有信息
    Procfs(ProcfsFilePrivateData),
    /// 设备文件的私有信息
    DevFS(DevicePrivateData),
    /// tty设备文件的私有信息
    Tty(TtyFilePrivateData),
    /// epoll私有信息
    EPoll(EPollPrivateData),
    /// pid私有信息
    Pid(PidPrivateData),
    //loop私有信息
    Loop(LoopPrivateData),
    /// namespace fd 私有信息（/proc/thread-self/ns/* 打开后得到）
    Namespace(NamespaceFilePrivateData),
    /// Socket file created by socket syscalls (not by VFS open(2)).
    SocketCreate,
    /// FUSE file private data.
    Fuse(FuseFilePrivateData),
    /// OverlayFS per-open backing file private data.
    Overlayfs(OverlayFilePrivateData),
    /// kernfs/debugfs per-open callback state.
    Kernfs(Option<KernFilePrivateData>),
    /// timerfd open-file-description flags (notably O_NONBLOCK).
    TimerFd(FileFlags),
    /// Configurable filesystem context shared by dup/fork aliases.
    FsContext(Arc<crate::filesystem::vfs::mount_api::context::FsContext>),
    /// Ownership of a detached mount tree until it is attached or finally closed.
    DetachedMount(Arc<crate::filesystem::vfs::mount::DetachedMountTree>),
    /// 不需要文件私有信息
    Unused,
}

impl Default for FilePrivateData {
    fn default() -> Self {
        return Self::Unused;
    }
}

/// Owns filesystem private data for an inode that has already been opened.
///
/// The guard stays armed until a [`File`] takes ownership.  This closes the
/// filesystem handle on every error path between an atomic create/open and
/// construction of the open file description.
pub struct PreopenedFile {
    inode: Arc<dyn IndexNode>,
    private_data: Option<FilePrivateData>,
    writer: Option<MountWriteGuard>,
}

/// Continuous create/open ownership, including backends without atomic open.
/// The preopened handle owns the writer when present; the fallback keeps it
/// here until the File performs the separate backend open.
pub(crate) struct CreatedFile {
    inode: Arc<dyn IndexNode>,
    preopened: Option<PreopenedFile>,
    writer: Option<MountWriteGuard>,
}

impl CreatedFile {
    pub(crate) fn new(
        inode: Arc<dyn IndexNode>,
        mut preopened: Option<PreopenedFile>,
        writer: MountWriteGuard,
    ) -> Self {
        let writer = if let Some(opened) = preopened.as_mut() {
            opened.replace_inode(inode.clone());
            opened.set_writer(writer);
            None
        } else {
            Some(writer)
        };
        Self {
            inode,
            preopened,
            writer,
        }
    }

    pub(crate) fn inode(&self) -> Arc<dyn IndexNode> {
        self.inode.clone()
    }
}

impl PreopenedFile {
    pub fn new(inode: Arc<dyn IndexNode>, private_data: FilePrivateData) -> Self {
        Self {
            inode,
            private_data: Some(private_data),
            writer: None,
        }
    }

    pub fn inode(&self) -> Arc<dyn IndexNode> {
        self.inode.clone()
    }

    pub fn replace_inode(&mut self, inode: Arc<dyn IndexNode>) {
        self.inode = inode;
    }

    pub(crate) fn set_writer(&mut self, writer: MountWriteGuard) {
        self.writer = Some(writer);
    }

    fn take_private_data(&mut self) -> FilePrivateData {
        self.private_data
            .take()
            .expect("preopened file private data already consumed")
    }
}

impl Drop for PreopenedFile {
    fn drop(&mut self) {
        if let Some(data) = self.private_data.take() {
            let _ = self.inode.close(Mutex::new(data).lock());
        }
    }
}

impl FilePrivateData {
    pub fn update_flags(&mut self, flags: FileFlags) -> Result<(), SystemError> {
        match self {
            FilePrivateData::Pipefs(pdata) => {
                pdata.set_flags(flags);
            }
            FilePrivateData::Tty(pdata) => {
                pdata.flags = flags;
            }
            FilePrivateData::Fuse(pdata) => {
                pdata.set_flags(flags);
            }
            FilePrivateData::Overlayfs(pdata) => {
                pdata.set_flags(flags)?;
            }
            FilePrivateData::TimerFd(current) => *current = flags,
            _ => {}
        }
        Ok(())
    }
}

impl File {
    /// The File pins a particular first-open runtime. Never follow a FIFO
    /// shell's current active slot after last-close/reopen.
    pub(crate) fn pipe_runtime(&self) -> Result<Arc<crate::ipc::pipe::PipeRuntime>, SystemError> {
        crate::ipc::pipe::LockedPipeInode::runtime(&self.private_data.lock())
    }
}

bitflags! {
    /// @brief 文件打开模式
    /// 其中，低2bit组合而成的数字的值，用于表示访问权限。其他的bit，才支持通过按位或的方式来表示参数
    ///
    /// 与Linux 5.19.10的uapi/asm-generic/fcntl.h相同
    /// https://code.dragonos.org.cn/xref/linux-5.19.10/tools/include/uapi/asm-generic/fcntl.h#19
    #[allow(clippy::bad_bit_mask)]
    pub struct FileFlags: u32{
        /* File access modes for `open' and `fcntl'.  */
        /// Open Read-only
        const O_RDONLY = 0o0;
        /// Open Write-only
        const O_WRONLY = 0o1;
        /// Open read/write
        const O_RDWR = 0o2;
        /// Mask for file access modes
        const O_ACCMODE = 0o00000003;

        /* Bits OR'd into the second argument to open.  */
        /// Create file if it does not exist
        const O_CREAT = 0o00000100;
        /// Fail if file already exists
        const O_EXCL = 0o00000200;
        /// Do not assign controlling terminal
        const O_NOCTTY = 0o00000400;
        /// 文件存在且是普通文件，并以O_RDWR或O_WRONLY打开，则它会被清空
        const O_TRUNC = 0o00001000;
        /// 文件指针会被移动到文件末尾
        const O_APPEND = 0o00002000;
        /// 非阻塞式IO模式
        const O_NONBLOCK = 0o00004000;
        /// 每次write都等待物理I/O完成，但是如果写操作不影响读取刚写入的数据，则不等待文件属性更新
        const O_DSYNC = 0o00010000;
        /// fcntl, for BSD compatibility
        const FASYNC = 0o00020000;
        /* direct disk access hint */
        const O_DIRECT = 0o00040000;
        const O_LARGEFILE = 0o00100000;
        /// 打开的必须是一个目录
        const O_DIRECTORY = 0o00200000;
        /// Do not follow symbolic links
        const O_NOFOLLOW = 0o00400000;
        const O_NOATIME = 0o01000000;
        /// set close_on_exec
        const O_CLOEXEC = 0o02000000;
        /// 每次write都等到物理I/O完成，包括write引起的文件属性的更新
        const __O_SYNC = 0o04000000;
        const O_SYNC = Self::__O_SYNC.bits | Self::O_DSYNC.bits;

        const O_PATH = 0o10000000;

        /// Raw high bit of O_TMPFILE; Linux also requires O_DIRECTORY.
        const __O_TMPFILE = 0o20000000;
        const O_TMPFILE = Self::__O_TMPFILE.bits | Self::O_DIRECTORY.bits;

        const O_PATH_FLAGS = Self::O_DIRECTORY.bits|Self::O_NOFOLLOW.bits|Self::O_CLOEXEC.bits|Self::O_PATH.bits;
    }
}

impl FileFlags {
    /// @brief 获取文件的访问模式 (O_RDONLY/O_WRONLY/O_RDWR)
    ///
    /// 这是正确提取访问模式的方法,因为O_RDONLY=0不能用contains()检查
    #[inline]
    pub fn access_flags(&self) -> FileFlags {
        *self & Self::O_ACCMODE
    }

    /// @brief 检查是否是只读模式
    #[inline]
    pub fn is_read_only(&self) -> bool {
        self.access_flags() == Self::O_RDONLY
    }

    /// @brief 检查是否是只写模式
    #[inline]
    pub fn is_write_only(&self) -> bool {
        self.access_flags() == Self::O_WRONLY
    }

    /// @brief 检查是否是读写模式
    #[inline]
    pub fn is_rdwr(&self) -> bool {
        self.access_flags() == Self::O_RDWR
    }

    /// 检查是否设置了 FASYNC 标志
    #[inline]
    pub fn fasync(&self) -> bool {
        self.contains(FileFlags::FASYNC)
    }
}

bitflags! {
    /// 文件访问模式标志 (对应 Linux 的 file.f_mode)
    ///
    /// 这些标志是内核专用的，用户空间不可见。
    ///
    /// 参考: https://elixir.bootlin.com/linux/v6.12.6/source/include/linux/fs.h#L119
    pub struct FileMode: u32 {
        // ============================================================
        // 基本访问模式 (从 f_flags 转换而来)
        // ============================================================
        /// 文件以读模式打开
        const FMODE_READ = 0x1;
        /// 文件以写模式打开
        const FMODE_WRITE = 0x2;
        /// 文件支持 lseek 操作
        const FMODE_LSEEK = 0x4;
        /// 文件支持 pread 操作
        const FMODE_PREAD = 0x8;
        /// 文件支持 pwrite 操作
        const FMODE_PWRITE = 0x10;
        /// 文件通过 sys_execve / sys_uselib 打开用于执行
        const FMODE_EXEC = 0x20;
        // ============================================================
        // 目录哈希模式 (用于 llseek 偏移)
        // ============================================================
        /// 使用 32 位哈希作为 llseek() 偏移 (用于目录)
        const FMODE_32BITHASH = 0x200;
        /// 使用 64 位哈希作为 llseek() 偏移 (用于目录)
        const FMODE_64BITHASH = 0x400;
        // ============================================================
        // 时间戳和访问模式控制
        // ============================================================
        /// 不更新 ctime 和 mtime
        /// 目前是 XFS open_by_handle ioctl 的特殊 hack，
        /// 未来可能会升级为 open(2) 支持的 O_CMTIME 标志
        const FMODE_NOCMTIME = 0x800;
        /// 期望随机访问模式 (由 fadvise 设置)
        const FMODE_RANDOM = 0x1000;
        /// 文件是巨大的 (如 /dev/mem): 将 loff_t 视为无符号数
        const FMODE_UNSIGNED_OFFSET = 0x2000;
        /// 文件以 O_PATH 方式打开 (几乎无操作权限)
        const FMODE_PATH = 0x4000;
        /// 需要原子性的 f_pos 访问 (常规文件/目录)
        const FMODE_ATOMIC_POS = 0x8000;
        // ============================================================
        // 写权限和能力标志
        // ============================================================
        /// 持有对底层文件系统的写访问权限
        const FMODE_WRITER = 0x10000;
        /// 文件有 read 或 read_iter 方法
        const FMODE_CAN_READ = 0x20000;
        /// 文件有 write 或 write_iter 方法
        const FMODE_CAN_WRITE = 0x40000;
        // ============================================================
        // 文件状态标志
        // ============================================================
        /// 文件已成功打开
        const FMODE_OPENED = 0x80000;
        /// 文件是新创建的 (O_CREAT 成功时设置)
        const FMODE_CREATED = 0x100000;
        /// 文件是流式的 (不可 seek)
        const FMODE_STREAM = 0x200000;
        /// 文件支持 O_DIRECT 直接 I/O
        const FMODE_CAN_ODIRECT = 0x400000;
        /// 文件不应被重用
        const FMODE_NOREUSE = 0x800000;
        // ============================================================
        // 并发和异步 I/O 标志
        // ============================================================
        /// 文件支持来自多线程的非独占 O_DIRECT 写入
        const FMODE_DIO_PARALLEL_WRITE = 0x1000000;
        /// 文件嵌入在 backing_file 对象中
        const FMODE_BACKING = 0x2000000;
        /// 文件由 fanotify 打开，不应生成 fanotify 事件
        const FMODE_NONOTIFY = 0x4000000;
        /// 文件能够在 I/O 将阻塞时返回 -EAGAIN (支持非阻塞 I/O，如 io_uring)
        const FMODE_NOWAIT = 0x8000000;
        // ============================================================
        // 挂载和资源管理标志
        // ============================================================
        /// 文件代表需要卸载的挂载点
        const FMODE_NEED_UNMOUNT = 0x10000000;
        /// 文件不计入 nr_files 计数
        const FMODE_NOACCOUNT = 0x20000000;
        /// 文件支持异步缓冲读取
        const FMODE_BUF_RASYNC = 0x40000000;
        /// 文件支持异步 nowait 缓冲写入
        const FMODE_BUF_WASYNC = 0x80000000;
    }
}

impl FileMode {
    /// @brief 从FileFlags转换为FileMode (实现OPEN_FMODE)
    ///
    /// - O_RDONLY (0) -> FMODE_READ (1)
    /// - O_WRONLY (1) -> FMODE_WRITE (2)
    /// - O_RDWR   (2) -> FMODE_READ | FMODE_WRITE (3)
    /// - 以及对于抑制fsnotify/fanotify机制触发通知的标志FMODE_NONOTIFY
    pub fn open_fmode(flags: FileFlags) -> Self {
        let fmode = flags.bits() & FileMode::FMODE_NONOTIFY.bits()
            | (flags.access_flags().bits + 1) & FileFlags::O_ACCMODE.bits();

        // 初始只设置访问模式,其他能力在后续设置
        FileMode::from_bits_truncate(fmode)
    }

    /// @brief 检查是否可读
    #[inline]
    pub fn is_readable(&self) -> bool {
        self.contains(Self::FMODE_READ)
    }

    /// @brief 检查是否可写
    #[inline]
    pub fn is_writeable(&self) -> bool {
        self.contains(Self::FMODE_WRITE)
    }

    /// @brief 检查是否真的能读 (有读方法)
    #[inline]
    pub fn can_read(&self) -> bool {
        self.is_readable() && self.contains(Self::FMODE_CAN_READ)
    }

    /// @brief 检查是否真的能写 (有写方法)
    #[inline]
    pub fn can_write(&self) -> bool {
        self.is_writeable() && self.contains(Self::FMODE_CAN_WRITE)
    }
}

/// @brief 抽象文件结构体
#[derive(Debug)]
pub struct File {
    data: OwnedBox<FileData>,
}

impl Deref for File {
    type Target = FileData;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl DerefMut for File {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}

/// Uniquely owned open-description storage. Thin Arc identity/weak handles
/// retain no charged body after the last strong File owner closes it.
#[derive(Debug)]
pub struct FileData {
    /// 唯一 open file description id，用于 flock owner 标识。
    open_file_id: usize,
    inode: Arc<dyn IndexNode>,
    /// Original pathname inode when open substitutes a runtime special inode
    /// (for example, a named FIFO's LockedPipeInode).  Linux keeps this
    /// identity in file->f_path for fsnotify even though I/O uses the special
    /// inode.  Anonymous objects and ordinary files need no extra reference.
    fsnotify_path_inode: Option<Arc<dyn IndexNode>>,
    /// Filesystem selected when this open file description was created.
    ///
    /// Mount wrappers retain the mount selected by path lookup. Cache that
    /// filesystem on the open file description so fd-based I/O and mmap fault
    /// paths use the same identity directly without cloning an Arc while MM
    /// locks are held.
    io_fs: Option<Arc<dyn FileSystem>>,
    /// 对于文件，表示字节偏移量；对于文件夹，表示当前操作的子目录项偏移量
    offset: AtomicUsize,
    /// 文件的打开模式
    flags: RwSem<FileFlags>,
    /// Serializes fasync registration with lifecycle-driven flag removal.
    fasync_lock: Mutex<()>,
    /// 文件的访问模式
    mode: RwSem<FileMode>,
    /// 文件类型
    file_type: FileType,
    /// Immutable write-operation policy selected for this open file description.
    post_write_sync: PostWriteSyncPolicy,
    /// Serializes current-position operations; also owns directory iteration state.
    position_state: Mutex<ReaddirState>,
    pub private_data: Mutex<FilePrivateData>,
    /// 文件的凭证
    cred: Arc<Cred>,
    /// Owner state for asynchronous I/O notifications.
    owner: Mutex<FileOwner>,
    /// Stable key for POSIX record locks. Cached at open time to avoid metadata fetch
    /// in close/drop_fd path (which can deadlock with user-space FUSE daemon).
    posix_lock_key: (usize, InodeId),
    /// Internal filesystem domain for serializing atomic append operations.
    /// Only regular, non-O_PATH files retain one; the inode key is resolved
    /// from current metadata when an append starts.
    append_lock_domain: Option<AppendLockDomain>,
    /// 预读状态
    ra_state: Mutex<FileReadaheadState>,
    /// 当前 open file description 已观测到的 page cache 写回错误序列。
    wb_error_seq: Mutex<ErrSeqValue>,
    /// 当前 open file description 已观测到的 superblock 写回错误序列。
    sb_error_seq: Mutex<ErrSeqValue>,
    /// epoll items that reference this open file description.
    ///
    /// Linux removes these from their owning epoll instances during `__fput()`
    /// via `eventpoll_release(file)`.  DragonOS keeps the same lifetime edge
    /// here so fd numbers can be safely reused after close.
    epitems: Arc<EPollItemList>,
    /// Writer admission lasts through close and all shared descriptor/VMA owners.
    /// Field destruction follows declaration order, after the explicit Drop body.
    _write_access: Option<Arc<InodeWriteGuard>>,
    _mount_writer: Option<MountWriteGuard>,
    /// Unpublished pathname open keeps O_TRUNC writer admission until its
    /// actual open hook completes, including lock-free admission recovery.
    pending_truncate_writer: Option<MountWriteGuard>,
    /// One semantic inode pin per open file description. Duplicated file
    /// descriptors and VMAs share this `File`, while `O_PATH` still owns it.
    _inode_retention: InodeRetentionGuard,
    /// FIFO/device I/O uses another inode, but the original pathname must
    /// remain live for file->f_path operations until this description closes.
    _path_inode_retention: Option<InodeRetentionGuard>,
    /// Release every inode owner before the mount pin can start final shutdown
    /// and seal the filesystem's eviction queue.
    _mount_guard: Option<MountExternalGuard>,
}

#[derive(Debug, Clone)]
enum ReaddirSnapshot {
    Typed(Arc<[DirectoryEntry]>),
    Names(Arc<[String]>),
}

impl ReaddirSnapshot {
    fn len(&self) -> usize {
        match self {
            Self::Typed(entries) => entries.len(),
            Self::Names(names) => names.len(),
        }
    }

    fn cookie_after(&self, index: usize) -> Option<u64> {
        match self {
            Self::Typed(entries) => entries.get(index).map(|entry| entry.next_cookie),
            Self::Names(names) => (index < names.len()).then(|| (index + 1) as u64),
        }
    }

    fn index_after_cookie(&self, cookie: u64) -> usize {
        match self {
            Self::Typed(entries) => entries
                .iter()
                .position(|entry| entry.next_cookie == cookie)
                .map_or(entries.len(), |index| index + 1),
            Self::Names(names) => usize::try_from(cookie)
                .ok()
                .filter(|index| *index <= names.len())
                .unwrap_or(names.len()),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ReaddirState {
    snapshot: Option<ReaddirSnapshot>,
    next_index: usize,
}

impl ReaddirState {
    fn resume_after_cookie(&mut self, cookie: u64) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            self.next_index = 0;
            return;
        };
        self.next_index = snapshot.index_after_cookie(cookie);
    }
}

fn checked_dirent_cookie(cookie: u64) -> Result<usize, SystemError> {
    let cookie = i64::try_from(cookie).map_err(|_| SystemError::EOVERFLOW)?;
    usize::try_from(cookie).map_err(|_| SystemError::EOVERFLOW)
}

#[cfg(test)]
mod readdir_tests {
    use super::checked_dirent_cookie;
    use system_error::SystemError;

    #[test]
    fn dirent_cookie_must_fit_signed_offset() {
        assert_eq!(checked_dirent_cookie(1), Ok(1));
        assert_eq!(
            checked_dirent_cookie(i64::MAX as u64 + 1),
            Err(SystemError::EOVERFLOW.into())
        );
    }
}

/// Only kernel-created pseudo-file descriptions bypass pathname writer
/// accounting. A fresh VFS open of the same inode is always regular.
enum FileOpenOrigin {
    Regular(Option<Metadata>, Option<MountWriteGuard>),
    InternalPseudo,
}

impl File {
    fn configure_base_open_mode(
        inode: &Arc<dyn IndexNode>,
        file_type: FileType,
        mode: &mut FileMode,
    ) {
        mode.remove(
            FileMode::FMODE_STREAM
                | FileMode::FMODE_LSEEK
                | FileMode::FMODE_PREAD
                | FileMode::FMODE_PWRITE
                | FileMode::FMODE_ATOMIC_POS
                | FileMode::FMODE_CAN_READ
                | FileMode::FMODE_CAN_WRITE,
        );
        if inode.is_stream() {
            mode.insert(FileMode::FMODE_STREAM);
        }
        if inode.supports_seek() {
            mode.insert(FileMode::FMODE_LSEEK);
        }
        // Anonymous stream inodes may also report FileType::File.
        if !mode.contains(FileMode::FMODE_STREAM)
            && matches!(file_type, FileType::File | FileType::Dir)
        {
            mode.insert(FileMode::FMODE_ATOMIC_POS);
        }
        if inode.supports_pread() {
            mode.insert(FileMode::FMODE_PREAD);
        }
        if inode.supports_pwrite() {
            mode.insert(FileMode::FMODE_PWRITE);
        }
        // Establish default I/O capabilities before the provider configures
        // its operation table. Owners without a read/write method can clear
        // these bits, including when an existing description is reopened.
        if mode.contains(FileMode::FMODE_READ) {
            mode.insert(FileMode::FMODE_CAN_READ);
        }
        if mode.contains(FileMode::FMODE_WRITE) {
            mode.insert(FileMode::FMODE_CAN_WRITE);
        }
    }

    pub fn resolved_path(&self) -> Result<super::utils::ResolvedPath, SystemError> {
        let mount_guard = self
            ._mount_guard
            .as_ref()
            .map(MountExternalGuard::derive)
            .transpose()?;
        // `self.inode` can be the runtime I/O inode of a FIFO or device.
        // Path-based operations must use the inode paired with this mount pin.
        super::utils::ResolvedPath::from_existing_mount(self.path_inode(), mount_guard)
    }

    #[inline]
    pub fn cred(&self) -> Arc<Cred> {
        self.cred.clone()
    }

    pub fn check_and_advance_wb_error(
        &self,
        page_cache: &Arc<PageCache>,
    ) -> Result<(), SystemError> {
        let since = *self.wb_error_seq.lock();
        if page_cache.check_writeback_error_since(since).is_none() {
            return Ok(());
        }

        let mut since = self.wb_error_seq.lock();
        match page_cache.check_and_advance_writeback_error(&mut since) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub fn check_and_advance_sb_wb_error(
        &self,
        mount_fs: &Arc<super::mount::MountFS>,
    ) -> Result<(), SystemError> {
        let mut since = self.sb_error_seq.lock();
        match mount_fs.check_and_advance_wb_error(&mut since) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub fn sync_range_and_check_wb_error(
        &self,
        start: usize,
        end: usize,
        datasync: bool,
    ) -> Result<(), SystemError> {
        Self::recover_io(|| self.try_sync_range_and_check_wb_error(start, end, datasync))
    }

    pub(crate) fn try_sync_range_and_check_wb_error(
        &self,
        start: usize,
        end: usize,
        datasync: bool,
    ) -> Result<(), MemoryAllocationError> {
        let inode = self.inode();
        let sync_result = inode.sync_file_range(start, end, datasync, self.private_data.lock());
        let wb_result = match inode.page_cache() {
            Some(page_cache) => self.check_and_advance_wb_error(&page_cache),
            None => Ok(()),
        };

        match sync_result {
            Err(e) => Err(e),
            Ok(()) => wb_result.map_err(Into::into),
        }
    }

    pub fn flush_for_close(&self, lock_owner: u64) -> Result<(), SystemError> {
        Self::recover_io(|| self.try_flush_for_close(lock_owner))
    }

    pub(crate) fn try_flush_for_close(&self, lock_owner: u64) -> Result<(), MemoryAllocationError> {
        if self.mode().contains(FileMode::FMODE_PATH) {
            return Ok(());
        }
        let inode = self.inode();
        inode.flush_file(self.private_data.lock(), lock_owner)
    }

    fn maybe_kill_suid_sgid_after_write(
        &self,
    ) -> Result<bool, crate::mm::allocation::MemoryAllocationError> {
        // 仅对普通文件生效。
        if self.file_type != FileType::File {
            return Ok(false);
        }

        // Linux 语义：若调用者具备 CAP_FSETID，则写/截断不会清除 suid/sgid。
        let cred = ProcessManager::current_pcb().cred();
        if cred.has_capability(CAPFlags::CAP_FSETID) {
            return Ok(false);
        }

        let mut md = self.inode.metadata()?;
        if !md.mode.intersects(InodeMode::S_ISUID | InodeMode::S_ISGID) {
            return Ok(false);
        }

        let original_mode = md.mode;
        // suid always must be killed on write/truncate when no CAP_FSETID.
        md.mode.remove(InodeMode::S_ISUID);

        // setgid 清理规则与 Linux fs/attr.c:setattr_should_drop_sgid 对齐：
        // - 若 S_IXGRP 置位：无条件清除（这是"真正的"SGID 可执行文件）
        // - 否则（mandatory locking 语义）：仅当调用者不在文件所属组时清除
        if should_remove_sgid(md.mode, md.gid, &cred) {
            md.mode.remove(InodeMode::S_ISGID);
        }
        if md.mode == original_mode {
            return Ok(false);
        }

        self.inode.set_metadata_masked(
            &md,
            SetMetadataMask::MODE | SetMetadataMask::WRITE_SIDE_EFFECT,
        )?;
        Ok(true)
    }

    #[inline(never)]
    fn limit_write_len_by_fsize(
        &self,
        file_type: FileType,
        offset: usize,
        len: usize,
    ) -> Result<usize, SystemError> {
        if !matches!(file_type, FileType::File) {
            return Ok(len);
        }
        let current_pcb = ProcessManager::current_pcb();
        let fsize_limit = current_pcb.get_rlimit(RLimitID::Fsize);

        if fsize_limit.rlim_cur == u64::MAX {
            return Ok(len);
        }

        let limit = fsize_limit.rlim_cur as usize;
        if offset >= limit {
            let _ = send_signal_to_pid(
                current_pcb.raw_pid(),
                crate::arch::ipc::signal::Signal::SIGXFSZ,
            );
            return Err(SystemError::EFBIG);
        }

        Ok(len.min(limit.saturating_sub(offset)))
    }

    fn mount_is_synchronous(&self) -> bool {
        self._mount_guard.as_ref().map_or_else(
            || self.inode.mount_flags().contains(MountFlags::SYNCHRONOUS),
            MountExternalGuard::is_synchronous,
        )
    }

    fn generic_sync_after_write(
        &self,
        start: usize,
        written_len: usize,
        sync_intent: WriteSyncIntent,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        if written_len == 0
            || sync_intent == WriteSyncIntent::None
            || self.post_write_sync != PostWriteSyncPolicy::Generic
        {
            return Ok(());
        }

        let end = start
            .checked_add(written_len - 1)
            .ok_or(SystemError::EOVERFLOW)?;
        // Linux generic_write_sync() 只有 IOCB_SYNC 才请求完整 metadata sync；
        // O_DSYNC、S_SYNC 与 SB_SYNCHRONOUS 都按 datasync 处理。
        self.try_sync_range_and_check_wb_error(start, end, sync_intent != WriteSyncIntent::Full)
    }

    fn post_write_sync_intent(&self, flags: FileFlags, inode_flags: InodeFlags) -> WriteSyncIntent {
        if self.post_write_sync == PostWriteSyncPolicy::NotApplicable {
            return WriteSyncIntent::None;
        }
        if flags.contains(FileFlags::__O_SYNC) {
            return WriteSyncIntent::Full;
        }
        if flags.contains(FileFlags::O_DSYNC)
            || inode_flags.contains(InodeFlags::S_SYNC)
            || self.mount_is_synchronous()
        {
            return WriteSyncIntent::Data;
        }
        WriteSyncIntent::None
    }

    #[inline(never)]
    fn write_at_and_finalize_split(
        &self,
        actual_offset: usize,
        actual_len: usize,
        buf: &[u8],
        config: WriteConfig,
    ) -> Result<DelegatedWriteResult, crate::mm::allocation::MemoryAllocationError> {
        let mut publish = || {
            if !self.mode.read().contains(FileMode::FMODE_NONOTIFY) {
                self.notify_dentry_event(FsEvent::ATTRIB);
            }
        };
        let mut stage = WritePrivilegeStage::new(&mut publish);
        let (written_len, sync_result, sync_range) = match self.post_write_sync {
            PostWriteSyncPolicy::Delegated => {
                let result = self.inode.write_at_with_sync_with_privilege_stage(
                    actual_offset,
                    actual_len,
                    buf,
                    config.sync_intent,
                    self.private_data.lock(),
                    &mut stage,
                )?;
                (
                    result.written_len,
                    Some(result.sync_result),
                    result.sync_range,
                )
            }
            PostWriteSyncPolicy::Generic | PostWriteSyncPolicy::NotApplicable => (
                self.inode.write_primary_with_privilege_stage(
                    actual_offset,
                    actual_len,
                    PrimaryWriteSource::Kernel(buf),
                    self.private_data.lock(),
                    &mut stage,
                )?,
                None,
                None,
            ),
        };

        let written_len =
            self.finalize_write(actual_offset, written_len, config, stage.handled())?;
        Ok(DelegatedWriteResult {
            written_len,
            sync_result: sync_result.unwrap_or(Ok(())),
            sync_range,
        })
    }

    fn write_user_at_and_finalize(
        &self,
        actual_offset: usize,
        actual_len: usize,
        input: &UserBuffer<'_>,
        config: WriteConfig,
        creator: &Arc<crate::cgroup::memory::object::ObjectAccount>,
    ) -> Result<DelegatedWriteResult, MemoryAllocationError> {
        if let Some(result) =
            self.write_user_buffer_at_and_finalize(actual_offset, actual_len, input, config)?
        {
            return Ok(result);
        }
        // Only the scalar entry reaches this fallback. Keep the old reader
        // interface for sockets and native privilege-aware primary writes.
        let reader =
            UserBufferReader::new(input.user_addr().data() as *const u8, input.len(), true)?;
        let mut publish = || {
            if !self.mode.read().contains(FileMode::FMODE_NONOTIFY) {
                self.notify_dentry_event(FsEvent::ATTRIB);
            }
        };
        let mut stage = WritePrivilegeStage::new(&mut publish);
        let (written_len, sync_result, sync_range) =
            if self.post_write_sync == PostWriteSyncPolicy::Delegated {
                let result =
                    PrimaryWriteSource::User(&reader, creator).with_buffer(actual_len, |buf| {
                        self.inode.write_at_with_sync_with_privilege_stage(
                            actual_offset,
                            actual_len.min(buf.len()),
                            buf,
                            config.sync_intent,
                            self.private_data.lock(),
                            &mut stage,
                        )
                    })?;
                (
                    result.written_len,
                    Some(result.sync_result),
                    result.sync_range,
                )
            } else {
                let written_len = match self.inode.write_user_at(
                    actual_offset,
                    actual_len,
                    &reader,
                    self.private_data.lock(),
                )? {
                    Some(written_len) => written_len,
                    None => self.inode.write_primary_with_privilege_stage(
                        actual_offset,
                        actual_len,
                        PrimaryWriteSource::User(&reader, creator),
                        self.private_data.lock(),
                        &mut stage,
                    )?,
                };
                (written_len, None, None)
            };

        let written_len =
            self.finalize_write(actual_offset, written_len, config, stage.handled())?;
        Ok(DelegatedWriteResult {
            written_len,
            sync_result: sync_result.unwrap_or(Ok(())),
            sync_range,
        })
    }

    fn write_user_buffer_at_and_finalize(
        &self,
        offset: usize,
        len: usize,
        input: &UserBuffer<'_>,
        config: WriteConfig,
    ) -> Result<Option<DelegatedWriteResult>, MemoryAllocationError> {
        // Delegated native providers own their privilege and sync transaction.
        if self.post_write_sync == PostWriteSyncPolicy::Delegated {
            return Ok(None);
        }
        let Some(written) =
            self.inode
                .write_user_buffer_at(offset, len, input, self.private_data.lock())?
        else {
            return Ok(None);
        };
        if written > len {
            return Err(SystemError::EIO.into());
        }
        let written_len = self.finalize_write(offset, written, config, false)?;
        Ok(Some(DelegatedWriteResult {
            written_len,
            sync_result: Ok(()),
            sync_range: None,
        }))
    }

    fn finalize_write(
        &self,
        actual_offset: usize,
        written_len: usize,
        config: WriteConfig,
        privileges_handled: bool,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        let mode_changed =
            !privileges_handled && written_len > 0 && self.maybe_kill_suid_sgid_after_write()?;

        if config.update_offset {
            match config.offset_update {
                OffsetUpdate::StoreEnd => {
                    self.offset.store(
                        actual_offset + written_len,
                        core::sync::atomic::Ordering::SeqCst,
                    );
                }
                OffsetUpdate::Add => {
                    self.offset
                        .fetch_add(written_len, core::sync::atomic::Ordering::SeqCst);
                }
            }
        }

        // fsnotify: deliver IN_MODIFY after a successful write (FMODE_NONOTIFY short-circuits, preventing inotify fd recursion).
        if written_len > 0
            && fsnotify::has_any_watch()
            && !self.mode.read().contains(FileMode::FMODE_NONOTIFY)
        {
            // Linux publishes the ATTR_MODE change from file_remove_privs
            // before the successful write's MODIFY event.
            if mode_changed {
                self.notify_fs_event(FsEvent::ATTRIB);
            }
            if config.emit_data_fsnotify {
                self.notify_fs_event(FsEvent::MODIFY);
            }
        }
        Ok(written_len)
    }
    /// @brief 创建一个新的文件对象
    ///
    /// @param inode 文件对象对应的inode
    /// @param flags 文件的打开模式
    pub fn new(
        inode: Arc<dyn IndexNode>,
        flags: FileFlags,
    ) -> Result<Self, crate::mm::allocation::MemoryAllocationError> {
        Self::new_with_private_data(inode, flags, FilePrivateData::default())
    }

    pub(crate) fn new_prepared(
        inode: Arc<dyn IndexNode>,
        flags: FileFlags,
        storage: PreparedFile,
    ) -> Result<Self, MemoryAllocationError> {
        Self::new_with_private_data_prepared(inode, flags, FilePrivateData::default(), storage)
    }

    /// Only for guardless producers: storage was admitted before instance
    /// side effects; resume an unfinished open on the same retained candidate.
    pub(crate) fn new_prepared_recovering(
        inode: Arc<dyn IndexNode>,
        flags: FileFlags,
        storage: PreparedFile,
    ) -> Result<Self, SystemError> {
        Self::new_with_private_data_prepared_recovering(
            inode,
            flags,
            FilePrivateData::default(),
            storage,
        )
    }

    pub(crate) fn new_with_private_data_prepared_recovering(
        inode: Arc<dyn IndexNode>,
        flags: FileFlags,
        private_data: FilePrivateData,
        storage: PreparedFile,
    ) -> Result<Self, SystemError> {
        let mut candidate = Self::prepare_regular(inode, flags, private_data, storage)?;
        Self::recover_io(|| candidate.complete_open())?;
        Ok(candidate)
    }

    /// Construct an internal pseudo-file description without a pathname open.
    ///
    /// Like Linux `alloc_file_pseudo()` and `alloc_file_clone()`, this preserves
    /// FMODE_WRITE but does not acquire the inode's pathname write-access count.
    /// Reopening the inode through a path (including /proc/self/fd) must use
    /// `File::new()` so executable write exclusion still applies.
    pub(crate) fn new_pseudo(
        inode: Arc<dyn IndexNode>,
        flags: FileFlags,
    ) -> Result<Self, crate::mm::allocation::MemoryAllocationError> {
        Self::new_pseudo_prepared(inode, flags, PreparedFile::try_new()?)
    }

    pub(crate) fn new_pseudo_prepared(
        inode: Arc<dyn IndexNode>,
        flags: FileFlags,
        storage: PreparedFile,
    ) -> Result<Self, MemoryAllocationError> {
        Self::prepare_with_private_data_and_mount_guard(
            inode,
            flags,
            FilePrivateData::default(),
            None,
            None,
            None,
            FileOpenOrigin::InternalPseudo,
            storage,
        )
        .map_err(MemoryAllocationError::System)
        .and_then(Self::finish_open)
    }

    /// Create a new file object with an explicit initial FilePrivateData.
    ///
    /// This is primarily used for objects that are not created via VFS open(2)
    /// semantics (e.g. sockets created by socket syscalls).
    pub fn new_with_private_data(
        inode: Arc<dyn IndexNode>,
        flags: FileFlags,
        private_data_init: FilePrivateData,
    ) -> Result<Self, crate::mm::allocation::MemoryAllocationError> {
        let storage = PreparedFile::try_new()?;
        Self::new_with_private_data_prepared(inode, flags, private_data_init, storage)
    }

    pub(crate) fn new_with_private_data_prepared(
        inode: Arc<dyn IndexNode>,
        flags: FileFlags,
        private_data_init: FilePrivateData,
        storage: PreparedFile,
    ) -> Result<Self, MemoryAllocationError> {
        Self::prepare_regular(inode, flags, private_data_init, storage)
            .map_err(MemoryAllocationError::System)
            .and_then(Self::finish_open)
    }

    fn prepare_regular(
        inode: Arc<dyn IndexNode>,
        flags: FileFlags,
        private_data_init: FilePrivateData,
        storage: PreparedFile,
    ) -> Result<Self, SystemError> {
        let mount_guard = inode
            .clone()
            .downcast_arc::<MountFSInode>()
            .map(|inode| inode.mount_fs().pin_operation_owner())
            .transpose()?;
        Self::prepare_with_private_data_and_mount_guard(
            inode,
            flags,
            private_data_init,
            mount_guard,
            None,
            None,
            FileOpenOrigin::Regular(None, None),
            storage,
        )
    }

    /// Construct a pathname-backed file by consuming the mount pin acquired
    /// during path resolution. This closes the lookup-to-open umount window.
    pub fn new_with_mount_guard(
        storage: PreparedFile,
        inode: Arc<dyn IndexNode>,
        flags: FileFlags,
        mount_guard: Option<MountExternalGuard>,
        operation_guard: InodeRetentionGuard,
        cached_path_metadata: Option<Metadata>,
    ) -> Result<Self, SystemError> {
        Self::prepare_with_private_data_and_mount_guard(
            inode,
            flags,
            FilePrivateData::default(),
            mount_guard,
            Some(operation_guard),
            None,
            FileOpenOrigin::Regular(cached_path_metadata, None),
            storage,
        )
    }

    /// Construct a pathname-backed file from an already-opened inode.
    pub fn new_preopened_with_mount_guard(
        storage: PreparedFile,
        preopened: PreopenedFile,
        flags: FileFlags,
        mount_guard: Option<MountExternalGuard>,
        operation_guard: InodeRetentionGuard,
    ) -> Result<Self, SystemError> {
        let inode = preopened.inode();
        Self::prepare_with_private_data_and_mount_guard(
            inode,
            flags,
            FilePrivateData::default(),
            mount_guard,
            Some(operation_guard),
            Some(preopened),
            FileOpenOrigin::Regular(None, None),
            storage,
        )
    }

    pub(crate) fn new_unlinked_with_mount_guard(
        storage: PreparedFile,
        mut unlinked: super::UnlinkedFile,
        flags: FileFlags,
        mount_guard: Option<MountExternalGuard>,
        operation_guard: InodeRetentionGuard,
    ) -> Result<Self, SystemError> {
        let writer = unlinked.take_mount_writer();
        Self::prepare_with_private_data_and_mount_guard(
            unlinked.inode(),
            flags,
            FilePrivateData::default(),
            mount_guard,
            Some(operation_guard),
            None,
            FileOpenOrigin::Regular(None, writer),
            storage,
        )
    }

    pub(crate) fn new_created_with_mount_guard(
        storage: PreparedFile,
        created: CreatedFile,
        flags: FileFlags,
        mount_guard: Option<MountExternalGuard>,
        operation_guard: InodeRetentionGuard,
        cached_path_metadata: Option<Metadata>,
    ) -> Result<Self, SystemError> {
        Self::prepare_with_private_data_and_mount_guard(
            created.inode,
            flags,
            FilePrivateData::default(),
            mount_guard,
            Some(operation_guard),
            created.preopened,
            FileOpenOrigin::Regular(cached_path_metadata, created.writer),
            storage,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Prepared resources have independent ownership and cancellation order"
    )]
    fn prepare_with_private_data_and_mount_guard(
        inode: Arc<dyn IndexNode>,
        mut flags: FileFlags,
        private_data_init: FilePrivateData,
        mount_guard: Option<MountExternalGuard>,
        // Parameters drop in reverse order on failure: release the operation
        // pin before the mount pin, just as the completed File does.
        _operation_guard: Option<InodeRetentionGuard>,
        mut preopened: Option<PreopenedFile>,
        origin: FileOpenOrigin,
        storage: PreparedFile,
    ) -> Result<Self, SystemError> {
        let (cached_path_metadata, account_write_access, inherited_mount_writer) = match origin {
            FileOpenOrigin::Regular(metadata, writer) => (metadata, true, writer),
            FileOpenOrigin::InternalPseudo => (None, false, None),
        };
        let mut inode = inode;
        let path_inode = inode.clone();
        let mut file_type = match cached_path_metadata.as_ref() {
            Some(metadata) => metadata.file_type,
            None => inode.metadata()?.file_type,
        };
        let is_path = flags.contains(FileFlags::O_PATH);

        if !is_path {
            // 检查是否为命名管道（FIFO）
            let is_named_pipe = if file_type == FileType::Pipe {
                if let Some(SpecialNodeData::Pipe(pipe_inode)) = inode.special_node() {
                    inode = pipe_inode;
                    file_type = if cached_path_metadata.is_some() {
                        inode.cached_metadata()?.file_type
                    } else {
                        inode.metadata()?.file_type
                    };
                    true
                } else {
                    false
                }
            } else {
                false
            };

            // 对于命名管道，自动添加 O_LARGEFILE 标志（符合 Linux 行为）
            if is_named_pipe {
                flags.insert(FileFlags::O_LARGEFILE);
            }

            inode = resolve_device_special_inode(inode, file_type, cached_path_metadata.as_ref())?;
        }

        let metadata = match cached_path_metadata.as_ref() {
            Some(metadata) if Arc::ptr_eq(&inode, &path_inode) => metadata.clone(),
            _ if cached_path_metadata.is_some() => inode.cached_metadata()?,
            _ => inode.metadata()?,
        };
        if !is_path && metadata.flags.contains(InodeFlags::S_APPEND) {
            flags.insert(FileFlags::O_APPEND);
        }

        let canonical_inode = canonical_inode_for_file_lock(inode.clone());
        let canonical_metadata = if Arc::ptr_eq(&canonical_inode, &inode) {
            metadata.clone()
        } else {
            if cached_path_metadata.is_some() {
                canonical_inode.cached_metadata()?
            } else {
                canonical_inode.metadata()?
            }
        };
        let posix_lock_key = (canonical_metadata.dev_id, canonical_metadata.inode_id);
        let append_lock_domain = if file_type == FileType::File && !is_path {
            canonical_inode.append_lock_fs().map(AppendLockDomain::new)
        } else {
            None
        };

        // O_CLOEXEC 是 per-fd 属性，由 alloc_fd/alloc_fd_arc 的 cloexec 参数控制，
        // 不存储在 File.flags 中。
        flags.remove(FileFlags::O_CLOEXEC);

        let mut mode = FileMode::open_fmode(flags);
        // Pin the final operation inode before invoking filesystem open. If
        // open fails, the local guard releases exactly once on this error path.
        let inode_retention =
            InodeRetentionGuard::new(inode.clone(), InodeRetentionKind::OpenFileDescription)?;
        let path_inode_retention = if Arc::ptr_eq(&inode, &path_inode) {
            None
        } else {
            Some(InodeRetentionGuard::new(
                path_inode.clone(),
                InodeRetentionKind::OpenFileDescription,
            )?)
        };

        if is_path && preopened.is_some() {
            return Err(SystemError::EINVAL);
        }
        let already_open = preopened.is_some();
        // Preserve a preopen's token until a successful File takes ownership.
        // It also covers read-only creates; only writable regular OFDs retain it.
        let needs_mount_writer = account_write_access
            && !is_special_file_type(file_type)
            && !is_path
            && mode.contains(FileMode::FMODE_WRITE);
        let mut mount_writer = if needs_mount_writer
            && inherited_mount_writer.is_none()
            && preopened
                .as_ref()
                .is_none_or(|opened| opened.writer.is_none())
        {
            path_inode
                .clone()
                .downcast_arc::<MountFSInode>()
                .map(|mounted| match mount_guard.as_ref() {
                    Some(pin) => mounted.mount_fs().want_write_from(pin),
                    None => mounted.mount_fs().want_write(),
                })
                .transpose()?
        } else {
            inherited_mount_writer
        };
        let pending_truncate_writer = if !needs_mount_writer
            && file_type == FileType::File
            && !is_path
            && flags.contains(FileFlags::O_TRUNC)
            && mount_writer.is_none()
        {
            path_inode
                .clone()
                .downcast_arc::<MountFSInode>()
                .map(|mounted| match mount_guard.as_ref() {
                    Some(pin) => mounted.mount_fs().want_write_from(pin),
                    None => mounted.mount_fs().want_write(),
                })
                .transpose()?
        } else {
            None
        };
        let write_access = if account_write_access
            && file_type == FileType::File
            && !is_path
            && mode.contains(FileMode::FMODE_WRITE)
        {
            Some(InodeWriteGuard::writer(inode.clone())?)
        } else {
            None
        };
        let private_data = Mutex::new(match preopened.as_mut() {
            Some(preopened) => preopened.take_private_data(),
            None => private_data_init,
        });
        if is_path {
            mode = FileMode::FMODE_PATH | FileMode::FMODE_OPENED;
        } else if already_open {
            // The preopen's private token has transferred once. Any later
            // cancellation must close it, never invoke open again.
            mode.insert(FileMode::FMODE_OPENED);
        }

        if needs_mount_writer {
            if let Some(opened) = preopened.as_mut() {
                if opened.writer.is_some() {
                    mount_writer = opened.writer.take();
                }
            }
            if mount_writer.is_some() {
                mode.insert(FileMode::FMODE_WRITER);
            }
        }

        let wb_error_seq = inode
            .page_cache()
            .map(|page_cache| page_cache.sample_writeback_error())
            .unwrap_or(0);
        let sb_error_seq = inode
            .clone()
            .downcast_arc::<MountFSInode>()
            .map(|mnt_inode| mnt_inode.mount_fs().sample_wb_error())
            .unwrap_or(0);
        let io_fs = inode
            .clone()
            .downcast_arc::<MountFSInode>()
            .map(|mnt_inode| mnt_inode.mount_fs() as Arc<dyn FileSystem>);

        let f = storage.init(|epitems| FileData {
            open_file_id: alloc_open_file_id(),
            fsnotify_path_inode: (!Arc::ptr_eq(&inode, &path_inode)).then_some(path_inode),
            inode,
            io_fs,
            offset: AtomicUsize::new(0),
            flags: RwSem::new(flags),
            fasync_lock: Mutex::new(()),
            mode: RwSem::new(mode),
            file_type,
            post_write_sync: PostWriteSyncPolicy::NotApplicable,
            position_state: Mutex::new(ReaddirState::default()),
            private_data,
            cred: ProcessManager::current_pcb().cred(),
            owner: Mutex::new(FileOwner::new()),
            posix_lock_key,
            append_lock_domain,
            ra_state: Mutex::new(FileReadaheadState::new()),
            wb_error_seq: Mutex::new(wb_error_seq),
            sb_error_seq: Mutex::new(sb_error_seq),
            epitems,
            _write_access: write_access,
            _mount_writer: mount_writer.filter(|_| needs_mount_writer),
            pending_truncate_writer,
            _inode_retention: inode_retention,
            _path_inode_retention: path_inode_retention,
            _mount_guard: mount_guard,
        });

        return Ok(f);
    }

    fn finish_open(mut file: Self) -> Result<Self, MemoryAllocationError> {
        file.complete_open()?;
        Ok(file)
    }

    /// Resume only the unfinished open hook on this exact resolved inode.
    /// The caller may recover a typed failure after the hook's guards unwind,
    /// without replaying pathname resolution, create or a preopened handle.
    pub(crate) fn complete_open(&mut self) -> Result<(), MemoryAllocationError> {
        let mut mode = self.mode();
        if mode.contains(FileMode::FMODE_PATH) {
            return Ok(());
        }
        if !mode.contains(FileMode::FMODE_OPENED) {
            self.inode.open(self.private_data.lock(), &self.flags())?;
        }
        // Open-time truncation/revalidation may advance writeback errors.
        // Preserve the original constructor's post-open sampling boundary.
        *self.wb_error_seq.lock() = self
            .inode
            .page_cache()
            .map(|cache| cache.sample_writeback_error())
            .unwrap_or(0);
        *self.sb_error_seq.lock() = self
            .inode
            .clone()
            .downcast_arc::<MountFSInode>()
            .map(|mounted| mounted.mount_fs().sample_wb_error())
            .unwrap_or(0);
        mode.remove(FileMode::FMODE_OPENED);
        Self::configure_base_open_mode(&self.inode, self.file_type, &mut mode);
        let mut behavior = OpenFileBehavior::new(mode, self.file_type);
        self.inode
            .configure_open_file(&self.private_data.lock(), &mut behavior);
        mode = behavior.mode;
        if self._mount_writer.is_some() {
            mode.insert(FileMode::FMODE_WRITER);
        }
        mode.insert(FileMode::FMODE_OPENED);
        self.post_write_sync = behavior.post_write_sync;
        *self.mode.write() = mode;
        drop(self.pending_truncate_writer.take());
        Ok(())
    }

    /// Dispatch using one coherent dentry snapshot. This keeps the read/write
    /// hot path free of namespace walks and temporary String allocations.
    pub(crate) fn notify_fs_event(&self, mask: FsEvent) {
        let event_inode = self.fsnotify_path_inode.as_ref().unwrap_or(&self.inode);
        if let Some(mounted) = event_inode.clone().downcast_arc::<MountFSInode>() {
            let Some((child, parent)) = mounted.fsnotify_snapshot() else {
                return;
            };
            // Linux deliberately withholds ACCESS/MODIFY child events from a
            // special file's parent directory to avoid a side channel.  The
            // special inode itself still receives the event through f_path.
            let special_content_event = mask.intersects(FsEvent::ACCESS | FsEvent::MODIFY)
                && matches!(
                    self.file_type,
                    FileType::BlockDevice
                        | FileType::CharDevice
                        | FileType::FramebufferDevice
                        | FileType::KvmDevice
                        | FileType::Pipe
                        | FileType::Socket
                );
            if let Some((parent, name)) = parent.as_ref().filter(|_| !special_content_event) {
                fsnotify::fsnotify_targets(
                    mask,
                    Some((parent, name.0.as_str())),
                    Some(&child),
                    0,
                    true,
                );
            } else {
                fsnotify::fsnotify_targets(mask, None, Some(&child), 0, true);
            }
        } else {
            fsnotify::fsnotify(mask, None, Some(event_inode), 0);
        }
    }

    /// Publish a dentry-data event such as ATTRIB/truncate. Unlike PATH I/O
    /// events, this is not suppressed by IN_EXCL_UNLINK.
    pub(crate) fn notify_dentry_event(&self, mask: FsEvent) {
        let event_inode = self.fsnotify_path_inode.as_ref().unwrap_or(&self.inode);
        fsnotify::fsnotify_inode(mask, event_inode);
    }

    /// Publish an I/O event when this open file description participates in
    /// userspace-visible I/O.  Internal FMODE_NONOTIFY users must not recurse
    /// into fsnotify, and O_PATH descriptors never perform such I/O.
    pub(crate) fn notify_io_event(&self, mask: FsEvent) {
        if fsnotify::has_any_watch()
            && !self
                .mode
                .read()
                .intersects(FileMode::FMODE_NONOTIFY | FileMode::FMODE_PATH)
        {
            self.notify_fs_event(mask);
        }
    }

    /// Notify a successful userspace-visible open. Callers decide which File
    /// constructions represent VFS open/exec rather than internal kernel I/O.
    pub(crate) fn notify_open_event(&self) {
        if !fsnotify::has_any_watch() {
            return;
        }
        let mode = *self.mode.read();
        if !mode.intersects(FileMode::FMODE_NONOTIFY | FileMode::FMODE_PATH) {
            self.notify_fs_event(FsEvent::OPEN);
        }
    }

    /// Create a file object for sockets created by socket syscalls.
    ///
    /// These should not be subject to open(2) pathname semantics.
    pub fn new_socket(
        inode: Arc<dyn IndexNode>,
        flags: FileFlags,
    ) -> Result<Self, crate::mm::allocation::MemoryAllocationError> {
        Self::new_with_private_data(inode, flags, FilePrivateData::SocketCreate)
    }

    pub(crate) fn new_socket_prepared(
        inode: Arc<dyn IndexNode>,
        flags: FileFlags,
        storage: PreparedFile,
    ) -> Result<Self, MemoryAllocationError> {
        Self::new_with_private_data_prepared(inode, flags, FilePrivateData::SocketCreate, storage)
    }

    /// ## 从文件中读取指定的字节数到buffer中
    ///
    /// ### 参数
    /// - `len`: 要读取的字节数
    /// - `buf`: 缓冲区
    /// - `read_direct`: 忽略缓存，直接读取磁盘
    ///
    /// ### 返回值
    /// - `Ok(usize)`: 成功读取的字节数
    /// - `Err(SystemError)`: 错误码
    pub fn read(&self, len: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        let _position = self.lock_position();
        let stream = self.mode.read().contains(FileMode::FMODE_STREAM);
        let offset = if stream {
            0
        } else {
            self.offset.load(Ordering::SeqCst)
        };

        self.do_read(offset, len, buf, !stream)
    }

    /// Read a bounded chunk without committing the position before user copy.
    /// The syscall holds the position transaction across every chunk.
    /// Only the backing read is recoverable: copying and consumption must never
    /// be replayed by charge recovery.
    pub(crate) fn read_syscall_chunk(
        &self,
        buf: &mut [u8],
        output: &mut UserBuffer<'_>,
        output_offset: usize,
    ) -> Result<usize, SystemError> {
        let stream = self.mode.read().contains(FileMode::FMODE_STREAM);
        let offset = if stream {
            0
        } else {
            self.offset.load(Ordering::SeqCst)
        };
        let read = Self::recover_io(|| self.read_backing(offset, buf.len(), buf))?;
        let result = output.write_prefix_to_user(output_offset, &buf[..read]);
        // file_accessed() also runs after a nonzero regular request which
        // reaches EOF or faults on its first output byte. No ACCESS is emitted
        // here: the syscall owns that notification boundary.
        self.finalize_read(
            offset,
            result.as_ref().copied().unwrap_or(0),
            !stream,
            false,
        );
        result
    }

    /// Linux rw_verify_area's signed count/position checks. Stream descriptions
    /// have no position; address-range admission belongs to the syscall.
    /// The caller holds the position transaction through verification and I/O.
    pub(crate) fn verify_read_range(&self, count: usize) -> Result<(), SystemError> {
        if count > isize::MAX as usize {
            return Err(SystemError::EINVAL);
        }
        if !self.mode.read().contains(FileMode::FMODE_STREAM) {
            let offset = self.offset.load(Ordering::SeqCst);
            if offset > isize::MAX as usize || count > isize::MAX as usize - offset {
                return Err(SystemError::EINVAL);
            }
        }
        Ok(())
    }

    /// Let an inode consume a protected userspace buffer as one read syscall.
    ///
    /// `None` means the inode does not need this capability and the syscall
    /// layer should use the normal bounded kernel-buffer fallback.
    pub fn read_user(
        &self,
        len: usize,
        writer: &mut UserBuffer<'_>,
    ) -> Result<Option<usize>, SystemError> {
        let _position = self.lock_position();
        self.read_user_at_current_position(len, writer, true)
    }

    /// Read one userspace-buffer chunk for a vector syscall.
    ///
    /// The caller holds the whole syscall position transaction.
    /// The vector syscall publishes ACCESS once at its completion boundary,
    /// rather than once per iovec processed by the fallback `.read` loop.
    pub(crate) fn read_user_syscall_chunk(
        &self,
        len: usize,
        writer: &mut UserBuffer<'_>,
    ) -> Result<Option<usize>, SystemError> {
        self.read_user_at_current_position(len, writer, false)
    }

    /// Caller owns the whole current-position transaction.
    pub(crate) fn read_user_at_current_position(
        &self,
        len: usize,
        writer: &mut UserBuffer<'_>,
        emit_fsnotify: bool,
    ) -> Result<Option<usize>, SystemError> {
        let mode = *self.mode.read();
        let stream = mode.contains(FileMode::FMODE_STREAM);
        let offset = if stream {
            0
        } else {
            self.offset.load(Ordering::SeqCst)
        };

        self.readable()?;
        if writer.len() < len {
            return Err(SystemError::ENOBUFS);
        }
        let Some(read_len) =
            self.inode
                .read_user_at(offset, len, writer, self.private_data.lock())?
        else {
            return Ok(None);
        };

        self.finalize_read(offset, read_len, !stream, emit_fsnotify);
        Ok(Some(read_len))
    }

    #[inline]
    pub fn user_read_mode(&self) -> super::UserReadMode {
        self.inode.user_read_mode()
    }

    pub(crate) fn supports_read_iter_flags(&self) -> bool {
        // Linux directory operations expose legacy generic_read_dir, even
        // when the filesystem's ordinary files implement read_iter.
        self.file_type != FileType::Dir && self.inode.supports_read_iter_flags()
    }

    /// Read from the current file position without advancing it.
    pub fn read_noadv(&self, len: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        let _position = self.lock_position();
        let offset = if self.mode.read().contains(FileMode::FMODE_STREAM) {
            0
        } else {
            self.offset.load(Ordering::SeqCst)
        };

        self.do_read(offset, len, buf, false)
    }

    /// ## 从buffer向文件写入指定的字节数的数据
    ///
    /// ### 参数
    /// - `offset`: 文件偏移量
    /// - `len`: 要写入的字节数
    /// - `buf`: 写入缓冲区
    ///
    /// ### 返回值
    /// - `Ok(usize)`: 成功写入的字节数
    /// - `Err(SystemError)`: 错误码
    pub fn write(&self, len: usize, buf: &[u8]) -> Result<usize, SystemError> {
        let _position = self.lock_position();
        let stream = self.mode.read().contains(FileMode::FMODE_STREAM);
        let offset = if stream {
            0
        } else {
            self.offset.load(Ordering::SeqCst)
        };

        self.do_write(offset, len, buf, !stream, false)
    }

    /// ## 从文件中指定的偏移处读取指定的字节数到buf中
    ///
    /// ### 参数
    /// - `offset`: 文件偏移量
    /// - `len`: 要读取的字节数
    /// - `buf`: 读出缓冲区
    ///
    /// ### 返回值
    /// - `Ok(usize)`: 成功读取的字节数
    pub fn pread(&self, offset: usize, len: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        Self::recover_io(|| self.pread_with_fsnotify(offset, len, buf, true))
    }

    pub(crate) fn verify_pread_access(&self) -> Result<(), SystemError> {
        // Linux 语义：O_PATH fd 任何 I/O 都应返回 EBADF（优先于 ESPIPE）。
        let mode = *self.mode.read();
        if mode.contains(FileMode::FMODE_PATH) {
            return Err(SystemError::EBADF);
        }

        // Linux 语义：对不可 seek 的“流式对象”（Pipe/Socket/部分伪文件等），
        // pread/pwrite/preadv/pwritev 应返回 ESPIPE（优先于读写权限导致的 EBADF）。
        // 这里用 FMODE_STREAM 作为 VFS 层收敛点，避免因 FMODE_PREAD/权限位组合导致误报 EBADF。
        if mode.contains(FileMode::FMODE_STREAM) {
            return Err(SystemError::ESPIPE);
        }

        // 对于不支持随机访问的文件（如管道），应该返回 ESPIPE，而不是因为权限问题返回 EBADF
        if !mode.contains(FileMode::FMODE_PREAD) {
            return Err(SystemError::ESPIPE);
        }

        Ok(())
    }

    pub(crate) fn verify_pread_range(
        &self,
        offset: usize,
        count: usize,
    ) -> Result<(), SystemError> {
        if offset > isize::MAX as usize || count > isize::MAX as usize - offset {
            return Err(SystemError::EINVAL);
        }
        Ok(())
    }

    fn pread_with_fsnotify(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        emit_fsnotify: bool,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        self.verify_pread_access()?;
        let mode = *self.mode.read();

        // 检查读权限
        if !mode.contains(FileMode::FMODE_READ) {
            return Err(SystemError::EBADF.into());
        }

        // 检查读能力
        if !mode.can_read() {
            return Err(SystemError::EINVAL.into());
        }

        self.do_read_with_fsnotify(offset, len, buf, false, emit_fsnotify)
    }

    /// Read one internal `pread(2)` chunk while deferring ACCESS until the
    /// syscall has determined its final, user-visible result.
    pub(crate) fn pread_syscall_chunk(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        Self::recover_io(|| self.pread_with_fsnotify(offset, len, buf, false))
    }

    /// Read for splice while deferring ACCESS until the transfer commits.
    /// This preserves the normal permission, readahead and atime behavior but
    /// lets do_splice publish MODIFY(out) before ACCESS(in), as Linux does.
    pub(crate) fn try_read_for_transfer(
        &self,
        offset: Option<usize>,
        len: usize,
        buf: &mut [u8],
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        match offset {
            Some(offset) => self.pread_with_fsnotify(offset, len, buf, false),
            None => {
                let offset = if self.mode.read().contains(FileMode::FMODE_STREAM) {
                    0
                } else {
                    self.offset.load(Ordering::SeqCst)
                };
                self.do_read_with_fsnotify(offset, len, buf, false, false)
            }
        }
    }

    /// Read a transfer's already validated local position without committing
    /// f_pos or emitting a per-chunk notification. The syscall owns capability
    /// checks and commits only the bytes accepted by its output.
    pub(crate) fn try_read_at_for_transfer(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        self.do_read_with_fsnotify(offset, len, buf, false, false)
    }

    /// ## 从buf向文件中指定的偏移处写入指定的字节数的数据
    ///
    /// ### 参数
    /// - `offset`: 文件偏移量
    /// - `len`: 要写入的字节数
    /// - `buf`: 写入缓冲区
    ///
    /// ### 返回值
    /// - `Ok(usize)`: 成功写入的字节数
    pub fn pwrite(&self, offset: usize, len: usize, buf: &[u8]) -> Result<usize, SystemError> {
        let result = Self::recover_io(|| {
            self.pwrite_with_sync_intent(offset, len, buf, WriteSyncIntent::None)
        })?;
        self.finish_write(result)
    }

    /// Perform a positional write while merging an operation-level sync
    /// request with this file's own O_SYNC/S_SYNC/SB_SYNCHRONOUS semantics.
    ///
    /// Data progress is returned separately from the synchronization result so
    /// stacking filesystems can preserve write side effects when fsync fails.
    pub fn pwrite_with_sync_intent(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        requested_sync: WriteSyncIntent,
    ) -> Result<DelegatedWriteResult, crate::mm::allocation::MemoryAllocationError> {
        self.pwrite_with_sync_intent_and_fsnotify(offset, len, buf, requested_sync, true)
    }

    fn pwrite_with_sync_intent_and_fsnotify(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        requested_sync: WriteSyncIntent,
        emit_data_fsnotify: bool,
    ) -> Result<DelegatedWriteResult, crate::mm::allocation::MemoryAllocationError> {
        // Linux 语义：O_PATH fd 任何 I/O 都应返回 EBADF（优先于 ESPIPE）。
        let mode = *self.mode.read();
        if mode.contains(FileMode::FMODE_PATH) {
            return Err(SystemError::EBADF.into());
        }

        // 同 pread：流式对象不支持随机偏移写入，返回 ESPIPE（优先于权限 EBADF）。
        if mode.contains(FileMode::FMODE_STREAM) {
            return Err(SystemError::ESPIPE.into());
        }

        // 对于不支持随机访问的文件（如管道），应该返回 ESPIPE，而不是因为权限问题返回 EBADF
        if !mode.contains(FileMode::FMODE_PWRITE) {
            return Err(SystemError::ESPIPE.into());
        }

        // 检查写权限
        if !mode.contains(FileMode::FMODE_WRITE) {
            return Err(SystemError::EBADF.into());
        }

        self.do_write_split(
            offset,
            len,
            buf,
            false,
            false,
            WriteCompletionPolicy {
                sync_intent: requested_sync,
                emit_data_fsnotify,
            },
        )
    }

    /// Write one internal `pwrite(2)` chunk while deferring its data-content
    /// MODIFY notification until the syscall completion point. Metadata
    /// changes such as setid removal remain visible at their commit point.
    pub(crate) fn pwrite_syscall_chunk(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        let result = Self::recover_io(|| {
            self.pwrite_with_sync_intent_and_fsnotify(
                offset,
                len,
                buf,
                WriteSyncIntent::None,
                false,
            )
        })?;
        self.finish_write(result)
    }

    /// 强制追加写（Linux `RWF_APPEND`/`IOCB_APPEND` 语义）：
    /// - 对常规文件：忽略当前偏移/给定偏移，追加到 EOF
    /// - 会推进本 fd 的文件偏移（类似 write）
    pub fn write_append(&self, len: usize, buf: &[u8]) -> Result<usize, SystemError> {
        let _position = self.lock_position();
        self.do_write(
            self.offset.load(core::sync::atomic::Ordering::SeqCst),
            len,
            buf,
            true,
            true,
        )
    }

    /// 强制追加 pwrite（Linux `RWF_APPEND` 语义）：
    /// - 对常规文件：忽略给定 offset，追加到 EOF
    /// - 不推进本 fd 的文件偏移（类似 pwrite）
    pub fn pwrite_append(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        self.writeable()?;
        if !self.mode().contains(FileMode::FMODE_PWRITE) {
            return Err(SystemError::ESPIPE);
        }
        self.do_write(offset, len, buf, false, true)
    }

    fn file_readahead(&self, offset: usize, len: usize) -> Result<(), SystemError> {
        if self.mode().contains(FileMode::FMODE_RANDOM) {
            return Ok(());
        }

        // 检查文件系统是否支持 readahead
        if !self.with_io_fs(|fs| fs.support_readahead()) {
            return Ok(());
        }

        let page_cache = match self.inode.page_cache() {
            Some(page_cahce) => page_cahce,
            None => return Ok(()),
        };
        let task = ProcessManager::current_pcb();
        let owner = crate::cgroup::memory::account_for_task(&task);
        let creator = crate::cgroup::memory::object_for_task(&task);
        drop(task);

        let start_page = offset >> MMArch::PAGE_SHIFT;
        let end_page = (offset + len - 1) >> MMArch::PAGE_SHIFT;

        let (async_trigger_page, missing_page) = {
            let mut async_trigger_page = None;
            let mut missing_page = None;

            for index in start_page..=end_page {
                match page_cache.manager().peek_page(index) {
                    Some(page) if page.read().flags().contains(PageFlags::PG_READAHEAD) => {
                        async_trigger_page = Some((index, page.clone()));
                        break;
                    }
                    None => {
                        missing_page = Some(index);
                        break;
                    }
                    _ => {}
                }
            }
            (async_trigger_page, missing_page)
        };

        if let Some((index, page)) = async_trigger_page {
            let mut ra_state = self.ra_state.lock().clone();
            let req_pages = end_page - index + 1;
            page.write().remove_flags(PageFlags::PG_READAHEAD);

            match page_cache_async_readahead(
                &page_cache,
                &self.inode,
                &mut ra_state,
                index,
                req_pages,
                &owner,
                &creator,
            ) {
                Ok(_) => {}
                // Speculative prefetch never kills a task for an optional page.
                // Required read_at admission is handled by the inode outer.
                Err(crate::mm::allocation::MemoryAllocationError::Charge(_)) => return Ok(()),
                Err(crate::mm::allocation::MemoryAllocationError::System(error)) => {
                    return Err(error)
                }
            }
            *self.ra_state.lock() = ra_state;
        } else if let Some(index) = missing_page {
            let mut ra_state = self.ra_state.lock().clone();
            let req_pages = end_page - index + 1;

            match page_cache_sync_readahead(
                &page_cache,
                &self.inode,
                &mut ra_state,
                index,
                req_pages,
                &owner,
                &creator,
            ) {
                Ok(_) => {}
                Err(crate::mm::allocation::MemoryAllocationError::Charge(_)) => return Ok(()),
                Err(crate::mm::allocation::MemoryAllocationError::System(error)) => {
                    return Err(error)
                }
            }
            *self.ra_state.lock() = ra_state;
        }
        Ok(())
    }

    fn do_read(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        update_offset: bool,
    ) -> Result<usize, SystemError> {
        Self::recover_io(|| self.do_read_with_fsnotify(offset, len, buf, update_offset, true))
    }

    fn do_read_with_fsnotify(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        update_offset: bool,
        emit_fsnotify: bool,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        let read = self.read_backing(offset, len, buf)?;
        if len != 0 {
            self.finalize_read(offset, read, update_offset, emit_fsnotify);
        }
        Ok(read)
    }

    fn read_backing(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
    ) -> Result<usize, MemoryAllocationError> {
        self.readable()?;
        // Linux/POSIX: count==0 must not touch the buffer and must not block.
        if len == 0 {
            return Ok(0);
        }
        if buf.len() < len {
            return Err(SystemError::ENOBUFS.into());
        }

        if self.file_type == FileType::File && !self.flags().contains(FileFlags::O_DIRECT) {
            self.file_readahead(offset, len)?;
        }

        let len = if self.flags().contains(FileFlags::O_DIRECT) {
            self.inode
                .read_direct(offset, len, buf, self.private_data.lock())
                .map_err(MemoryAllocationError::System)
        } else {
            self.inode
                .read_at(offset, len, buf, self.private_data.lock())
        }?;

        Ok(len)
    }

    fn finalize_read(&self, offset: usize, len: usize, update_offset: bool, emit_fsnotify: bool) {
        if len > 0 {
            let last_page_readed = (offset + len - 1) >> MMArch::PAGE_SHIFT;
            self.ra_state.lock().prev_index = last_page_readed as i64;
        }

        if update_offset {
            self.offset
                .fetch_add(len, core::sync::atomic::Ordering::SeqCst);
        }
        // Linux file_accessed() runs for every non-zero regular-file read
        // request, including EOF. Zero-count reads returned above are exempt.
        if len > 0 || self.file_type == FileType::File {
            self.touch_atime_after_access();
        }
        // fsnotify: deliver IN_ACCESS only when data was actually read (len > 0) (FMODE_NONOTIFY short-circuits).
        // An EOF read (len==0) does not deliver—independent of atime semantics.
        if emit_fsnotify
            && len > 0
            && fsnotify::has_any_watch()
            && !self.mode.read().contains(FileMode::FMODE_NONOTIFY)
        {
            self.notify_fs_event(FsEvent::ACCESS);
        }
    }

    /// Best-effort equivalent of Linux file_accessed()/touch_atime().
    fn touch_atime_after_access(&self) {
        use super::mount::{MountFS, MountFlags};

        if self.flags().contains(FileFlags::O_NOATIME) {
            return;
        }
        let mount_flags = self.inode.mount_flags();
        if mount_flags.contains(MountFlags::NOATIME)
            || (self.file_type == FileType::Dir && mount_flags.contains(MountFlags::NODIRATIME))
        {
            return;
        }
        let Some(fs) = self.inode.try_fs() else {
            // Anonymous kernel objects such as sockets are represented by an
            // IndexNode but are not backed by a filesystem or mount. Linux
            // does not apply file_accessed() timestamp updates to their I/O.
            return;
        };
        if fs
            .downcast_arc::<MountFS>()
            .is_some_and(|mount| mount.is_readonly())
        {
            return;
        }

        let now = crate::time::PosixTimeSpec::now();
        let _ = self
            .inode
            .update_atime(now, mount_flags.contains(MountFlags::RELATIME));
    }

    fn do_write(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        update_offset: bool,
        force_append: bool,
    ) -> Result<usize, SystemError> {
        let result = Self::recover_io(|| {
            self.do_write_split(
                offset,
                len,
                buf,
                update_offset,
                force_append,
                WriteCompletionPolicy {
                    sync_intent: WriteSyncIntent::None,
                    emit_data_fsnotify: true,
                },
            )
        })?;
        self.finish_write(result)
    }

    /// Write one internal transfer chunk while deferring its data-content
    /// notification to the syscall that owns the complete transfer. Metadata
    /// side effects such as setid removal are still reported immediately.
    pub(crate) fn try_write_for_transfer(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        update_offset: bool,
    ) -> Result<DelegatedWriteResult, MemoryAllocationError> {
        self.do_write_split(
            offset,
            len,
            buf,
            update_offset,
            false,
            WriteCompletionPolicy {
                sync_intent: WriteSyncIntent::None,
                emit_data_fsnotify: false,
            },
        )
    }

    fn do_write_split(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        update_offset: bool,
        force_append: bool,
        completion: WriteCompletionPolicy,
    ) -> Result<DelegatedWriteResult, crate::mm::allocation::MemoryAllocationError> {
        self.writeable()?;

        let inode_flags = self.get_inode_flags()?;
        let flags = self.flags();
        // 检查 S_IMMUTABLE
        if inode_flags.contains(InodeFlags::S_IMMUTABLE) {
            return Err(SystemError::EPERM.into());
        }
        // S_APPEND 的 inode 只能追加写入
        if inode_flags.contains(InodeFlags::S_APPEND) && !flags.contains(FileFlags::O_APPEND) {
            return Err(SystemError::EPERM.into());
        }

        if buf.len() < len {
            return Err(SystemError::ENOBUFS.into());
        }
        let md = self.inode.metadata()?;
        let file_type = md.file_type;
        let sync_intent = match self.post_write_sync {
            PostWriteSyncPolicy::Generic | PostWriteSyncPolicy::Delegated => self
                .post_write_sync_intent(flags, inode_flags)
                .combine(completion.sync_intent),
            PostWriteSyncPolicy::NotApplicable => WriteSyncIntent::None,
        };

        let is_append = matches!(file_type, FileType::File)
            && (flags.contains(FileFlags::O_APPEND) || force_append);

        // 普通文件始终按 EOF 执行 O_APPEND/RWF_APPEND；提供稳定锁域的文件系统
        // 还会在 VFS 层串行化“取 EOF + 写入”，避免并发追加互相覆盖。
        if is_append {
            let append_write = || {
                // 在锁内刷新元数据，确保 EOF 是最新的。
                let md = self.inode.metadata()?;
                let actual_offset = md.size.max(0) as usize;
                let actual_len = self.limit_write_len_by_fsize(file_type, actual_offset, len)?;

                self.write_at_and_finalize_split(
                    actual_offset,
                    actual_len,
                    buf,
                    WriteConfig {
                        update_offset,
                        offset_update: OffsetUpdate::StoreEnd,
                        sync_intent,
                        emit_data_fsnotify: completion.emit_data_fsnotify,
                    },
                )
                .map(|result| (actual_offset, result))
            };
            let (actual_offset, mut result) = match self.append_lock_domain.as_ref() {
                Some(domain) => with_inode_append_lock(domain.key(&md), append_write),
                None => append_write(),
            }?;
            if result.sync_result.is_ok() {
                // generic_write_sync() observes IS_SYNC after the write.
                // Re-sample the cheap mount atomic here while reusing the
                // inode flags already fetched before I/O.
                let current_sync_intent = self
                    .post_write_sync_intent(flags, inode_flags)
                    .combine(completion.sync_intent);
                result.sync_result = self.generic_sync_after_write(
                    actual_offset,
                    result.written_len,
                    current_sync_intent,
                );
            }
            Self::set_completed_sync_range(&mut result, actual_offset, sync_intent);
            return Ok(result);
        }

        let actual_offset = offset;
        let actual_len = self.limit_write_len_by_fsize(file_type, actual_offset, len)?;

        let mut result = self.write_at_and_finalize_split(
            actual_offset,
            actual_len,
            buf,
            WriteConfig {
                update_offset,
                offset_update: OffsetUpdate::Add,
                sync_intent,
                emit_data_fsnotify: completion.emit_data_fsnotify,
            },
        )?;
        if result.sync_result.is_ok() {
            let current_sync_intent = self
                .post_write_sync_intent(flags, inode_flags)
                .combine(completion.sync_intent);
            result.sync_result = self.generic_sync_after_write(
                actual_offset,
                result.written_len,
                current_sync_intent,
            );
        }
        Self::set_completed_sync_range(&mut result, actual_offset, sync_intent);
        Ok(result)
    }

    fn do_write_user(
        &self,
        offset: usize,
        len: usize,
        input: &UserBuffer<'_>,
        update_offset: bool,
        force_append: bool,
    ) -> Result<usize, SystemError> {
        let result = Self::recover_io(|| {
            self.try_do_write_user(offset, len, input, update_offset, force_append)
        })?;
        self.finish_write(result)
    }

    fn try_do_write_user(
        &self,
        offset: usize,
        len: usize,
        input: &UserBuffer<'_>,
        update_offset: bool,
        force_append: bool,
    ) -> Result<DelegatedWriteResult, MemoryAllocationError> {
        self.try_do_write_user_with(
            offset,
            len,
            update_offset,
            force_append,
            |actual_offset, actual_len, config, creator| {
                self.write_user_at_and_finalize(actual_offset, actual_len, input, config, creator)
            },
        )
    }

    /// Keep write policy independent of the input's scalar/vector shape.
    fn try_do_write_user_with(
        &self,
        offset: usize,
        len: usize,
        update_offset: bool,
        force_append: bool,
        commit: impl Fn(
            usize,
            usize,
            WriteConfig,
            &Arc<crate::cgroup::memory::object::ObjectAccount>,
        ) -> Result<DelegatedWriteResult, MemoryAllocationError>,
    ) -> Result<DelegatedWriteResult, MemoryAllocationError> {
        let creator = object_for_task(&ProcessManager::current_pcb());
        self.writeable()?;

        let inode_flags = self.get_inode_flags()?;
        let flags = self.flags();
        // Check S_IMMUTABLE
        if inode_flags.contains(InodeFlags::S_IMMUTABLE) {
            return Err(SystemError::EPERM.into());
        }
        // Inodes with S_APPEND may only be appended to
        if inode_flags.contains(InodeFlags::S_APPEND) && !flags.contains(FileFlags::O_APPEND) {
            return Err(SystemError::EPERM.into());
        }

        let md = self.inode.metadata()?;
        let file_type = md.file_type;
        let sync_intent = self.post_write_sync_intent(flags, inode_flags);

        let is_append = matches!(file_type, FileType::File)
            && (flags.contains(FileFlags::O_APPEND) || force_append);

        // Always select EOF for regular-file append semantics. Filesystems
        // with a stable lock domain additionally serialize EOF lookup + write.
        if is_append {
            let append_write = || {
                let md = self.inode.metadata()?;
                let actual_offset = md.size.max(0) as usize;
                let actual_len = self.limit_write_len_by_fsize(file_type, actual_offset, len)?;

                commit(
                    actual_offset,
                    actual_len,
                    WriteConfig {
                        update_offset,
                        offset_update: OffsetUpdate::StoreEnd,
                        sync_intent,
                        emit_data_fsnotify: true,
                    },
                    &creator,
                )
                .map(|result| (actual_offset, result))
            };
            let (actual_offset, mut result) = match self.append_lock_domain.as_ref() {
                Some(domain) => with_inode_append_lock(domain.key(&md), append_write),
                None => append_write(),
            }?;
            let current_sync_intent = self.post_write_sync_intent(flags, inode_flags);
            if result.sync_result.is_ok() {
                result.sync_result = self.generic_sync_after_write(
                    actual_offset,
                    result.written_len,
                    current_sync_intent,
                );
            }
            Self::set_completed_sync_range(&mut result, actual_offset, current_sync_intent);
            return Ok(result);
        }

        let actual_offset = offset;
        let actual_len = self.limit_write_len_by_fsize(file_type, actual_offset, len)?;

        let mut result = commit(
            actual_offset,
            actual_len,
            WriteConfig {
                update_offset,
                offset_update: OffsetUpdate::Add,
                sync_intent,
                emit_data_fsnotify: true,
            },
            &creator,
        )?;
        let current_sync_intent = self.post_write_sync_intent(flags, inode_flags);
        if result.sync_result.is_ok() {
            result.sync_result = self.generic_sync_after_write(
                actual_offset,
                result.written_len,
                current_sync_intent,
            );
        }
        Self::set_completed_sync_range(&mut result, actual_offset, current_sync_intent);
        Ok(result)
    }

    /// Write one scalar, contiguous userspace input. Vectors use the separate entry.
    pub fn write_user(&self, len: usize, input: &UserBuffer<'_>) -> Result<usize, SystemError> {
        let _position = self.lock_position();
        let stream = self.mode.read().contains(FileMode::FMODE_STREAM);
        let offset = if stream {
            0
        } else {
            self.offset.load(Ordering::SeqCst)
        };

        self.do_write_user(offset, len, input, !stream, false)
    }

    /// Preserve vector provenance until the owner chooses its copy units.
    pub(crate) fn write_user_vectored(
        &self,
        vectors: &super::iov::IoVecs,
    ) -> Result<usize, SystemError> {
        let _position = self.lock_position();
        self.writeable()?;
        let len = vectors.total_len();
        let segments = vectors.user_buffer_segments(len)?;
        let input = unsafe { UserBuffer::new_vectored(&segments, len) };
        let stream = self.mode.read().contains(FileMode::FMODE_STREAM);
        let offset = if stream {
            0
        } else {
            self.offset.load(Ordering::SeqCst)
        };
        let result = Self::recover_io(|| {
            self.try_do_write_user_with(
                offset,
                len,
                !stream,
                false,
                |offset, len, config, _creator| {
                    if let Some(result) =
                        self.write_user_buffer_at_and_finalize(offset, len, &input, config)?
                    {
                        return Ok(result);
                    }
                    let bytes = vectors.gather_prefix(len)?;
                    self.write_at_and_finalize_split(offset, bytes.len(), &bytes, config)
                },
            )
        })?;
        self.finish_write(result)
    }

    /// @brief 获取文件的元数据
    pub fn metadata(&self) -> Result<Metadata, SystemError> {
        return self.inode.metadata();
    }

    /// @brief 根据inode号获取子目录项的名字
    #[allow(dead_code)]
    pub fn get_entry_name(&self, ino: InodeId) -> Result<String, SystemError> {
        return self.inode.get_entry_name(ino);
    }

    /// @brief 调整文件操作指针的位置
    ///
    /// @param origin 调整的起始位置
    pub fn lseek(&self, origin: SeekFrom) -> Result<usize, SystemError> {
        let position = self.lock_position();
        let mode = self.mode();
        // Linux 语义：O_PATH fd 的 lseek 返回 EBADF（优先于 ESPIPE）。
        if mode.contains(FileMode::FMODE_PATH) {
            return Err(SystemError::EBADF);
        }
        if self.inode.has_noop_llseek() {
            return Ok(self.offset.load(Ordering::SeqCst));
        }
        if mode.contains(FileMode::FMODE_STREAM) || !mode.contains(FileMode::FMODE_LSEEK) {
            return Err(SystemError::ESPIPE);
        }

        // Position-only seeks use the immutable open-file type. In particular,
        // sysctl llseek remains usable after its table has been unregistered;
        // attributes are needed only for size-dependent regular-file seeks.
        let file_type = self.file_type();
        // Check for procfs private data. If this is a procfs pseudo-file, disallow SEEK_END
        // and other unsupported seek modes.
        {
            let pdata = self.private_data.lock();
            if let FilePrivateData::Procfs(_) = &*pdata {
                match origin {
                    SeekFrom::SeekEnd(_)
                    | SeekFrom::SeekData(_)
                    | SeekFrom::SeekHole(_)
                    | SeekFrom::Invalid => {
                        return Err(SystemError::EINVAL);
                    }
                    _ => {}
                }
            }
        }
        if file_type == FileType::Dir {
            let mut state = position.unwrap_or_else(|| self.position_state.lock());
            let pos = match origin {
                SeekFrom::SeekSet(offset) => offset,
                SeekFrom::SeekCurrent(0) => return Ok(self.offset.load(Ordering::SeqCst)),
                SeekFrom::SeekCurrent(offset) => self.offset.load(Ordering::SeqCst) as i64 + offset,
                SeekFrom::SeekEnd(_) => {
                    // Preserve the existing directory SEEK_END behavior.
                    return Ok(MAX_LFS_FILESIZE as usize);
                }
                SeekFrom::SeekData(_) | SeekFrom::SeekHole(_) => {
                    return Err(SystemError::EINVAL);
                }
                SeekFrom::Invalid => return Err(SystemError::EINVAL),
            };
            if pos < 0 {
                return Err(SystemError::EINVAL);
            }
            if pos == 0 {
                state.snapshot = None;
                state.next_index = 0;
            } else {
                state.resume_after_cookie(pos as u64);
            }
            self.offset.store(pos as usize, Ordering::SeqCst);
            return Ok(pos as usize);
        }

        let pos: i64 = match origin {
            SeekFrom::SeekSet(offset) => offset,
            SeekFrom::SeekCurrent(offset) => self.offset.load(Ordering::SeqCst) as i64 + offset,
            SeekFrom::SeekEnd(offset) => self.inode.metadata()?.size + offset,
            SeekFrom::SeekData(offset) => {
                let metadata = self.inode.metadata()?;
                if file_type != FileType::File {
                    return Err(SystemError::EINVAL);
                }
                if offset < 0 || offset >= metadata.size {
                    return Err(SystemError::ENXIO);
                }
                offset
            }
            SeekFrom::SeekHole(offset) => {
                let metadata = self.inode.metadata()?;
                if file_type != FileType::File {
                    return Err(SystemError::EINVAL);
                }
                if offset < 0 || offset >= metadata.size {
                    return Err(SystemError::ENXIO);
                }
                metadata.size
            }
            SeekFrom::Invalid => {
                return Err(SystemError::EINVAL);
            }
        };
        // 根据linux man page, lseek允许超出文件末尾，并且不改变文件大小
        // 当pos超出文件末尾时，read返回0。直到开始写入数据时，才会改变文件大小
        if pos < 0 {
            return Err(SystemError::EINVAL);
        }
        self.offset.store(pos as usize, Ordering::SeqCst);
        return Ok(pos as usize);
    }

    /// @brief 判断当前文件是否可读
    #[inline]
    pub fn readable(&self) -> Result<(), SystemError> {
        let mode = *self.mode.read();
        // O_PATH文件几乎不能做任何I/O操作
        if mode.contains(FileMode::FMODE_PATH) {
            return Err(SystemError::EBADF);
        }

        // 检查读权限
        if !mode.contains(FileMode::FMODE_READ) {
            return Err(SystemError::EBADF);
        }

        // 检查读能力
        if !mode.can_read() {
            return Err(SystemError::EINVAL);
        }

        Ok(())
    }

    /// @brief 判断当前文件是否可写
    #[inline]
    pub fn writeable(&self) -> Result<(), SystemError> {
        let mode = *self.mode.read();

        // O_PATH文件不能写
        if mode.contains(FileMode::FMODE_PATH) {
            return Err(SystemError::EBADF);
        }

        // 检查写权限
        if !mode.contains(FileMode::FMODE_WRITE) {
            return Err(SystemError::EBADF);
        }

        Ok(())
    }

    /// # 读取目录项
    ///
    /// ## 参数
    /// - ctx 填充目录项的上下文
    pub fn read_dir(&self, ctx: &mut FilldirContext) -> Result<(), SystemError> {
        // O_PATH file descriptors cannot be used by getdents/getdents64.
        if self.flags().contains(FileFlags::O_PATH) {
            return Err(SystemError::EBADF);
        }

        if self.file_type() != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // Directory iteration state also needs serialization for providers
        // which opt out of FMODE_ATOMIC_POS (for example a FUSE stream).
        let mut position = self.position_state.lock();
        let result = self.read_dir_impl(ctx, &mut position);
        // Match Linux iterate_dir(): once filesystem iteration has started,
        // reaching EOF or returning an error both count as directory access.
        // Keep the position transaction through metadata/notification completion;
        // the lock order remains position state before inode operations.
        self.touch_atime_after_access();
        if fsnotify::has_any_watch()
            && !self
                .mode
                .read()
                .intersects(FileMode::FMODE_PATH | FileMode::FMODE_NONOTIFY)
        {
            self.notify_fs_event(FsEvent::ACCESS);
        }
        result
    }

    fn read_dir_impl(
        &self,
        ctx: &mut FilldirContext,
        state: &mut ReaddirState,
    ) -> Result<(), SystemError> {
        let inode: &Arc<dyn IndexNode> = &self.inode;
        inode.before_read_dir()?;
        // POSIX 标准要求readdir应该返回. 和 ..
        // 但是观察到在现有的子目录中已经包含，不做处理也能正常返回. 和 .. 这里先不做处理

        // 迭代读取目录项
        // 为了保证在目录内容动态变化（例如 /proc/self/fd）时不会因为重新
        // 创建列表而丢失尚未读取的目录项，这里缓存第一次生成的列表，在
        // 文件偏移被 seek 到 0 之前复用该缓存。
        let current_cookie = self.offset.load(Ordering::SeqCst) as u64;
        if state.snapshot.is_none() {
            state.snapshot = Some(match inode.list_entries()? {
                Some(entries) => ReaddirSnapshot::Typed(Arc::from(entries)),
                None => ReaddirSnapshot::Names(Arc::from(inode.list()?)),
            });
            if current_cookie == 0 {
                state.next_index = 0;
            } else {
                state.resume_after_cookie(current_cookie);
            }
        } else {
            let expected_cookie = state.snapshot.as_ref().and_then(|snapshot| {
                state
                    .next_index
                    .checked_sub(1)
                    .and_then(|index| snapshot.cookie_after(index))
            });
            if state.next_index > 0 && expected_cookie != Some(current_cookie) {
                state.resume_after_cookie(current_cookie);
            }
        }

        let snapshot = state
            .snapshot
            .as_ref()
            .expect("readdir snapshot initialized")
            .clone();
        if state.next_index > snapshot.len() {
            return Err(SystemError::EINVAL);
        }
        while state.next_index < snapshot.len() {
            let index = state.next_index;
            let (name, cookie, metadata) = match &snapshot {
                ReaddirSnapshot::Typed(entries) => {
                    let entry = &entries[index];
                    (
                        entry.name.as_slice(),
                        entry.next_cookie,
                        Some((entry.ino, entry.d_type)),
                    )
                }
                ReaddirSnapshot::Names(names) => {
                    let name = &names[index];
                    (name.as_bytes(), (index + 1) as u64, None)
                }
            };
            let next_cookie = checked_dirent_cookie(cookie)?;
            let (ino, d_type) = if let Some(metadata) = metadata {
                metadata
            } else {
                let entry = match name {
                    b"." => inode.metadata().map(|md| (inode.reported_ino(&md), md)),
                    b".." => inode.parent().and_then(|parent| {
                        parent.metadata().map(|md| (parent.reported_ino(&md), md))
                    }),
                    _ => inode
                        .find_bytes(name)
                        .and_then(|child| child.metadata().map(|md| (child.reported_ino(&md), md))),
                };
                let (ino, metadata) = match entry {
                    Ok(entry) => entry,
                    Err(SystemError::ENOENT) => {
                        state.next_index += 1;
                        self.offset.store(next_cookie, Ordering::SeqCst);
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                (
                    ino.into() as u64,
                    metadata.file_type.get_file_type_num() as u8,
                )
            };
            match ctx.fill_dir(name, next_cookie, ino, d_type) {
                Ok(_) => {
                    state.next_index += 1;
                    self.offset.store(next_cookie, Ordering::SeqCst);
                }
                Err(SystemError::EINVAL) => {
                    return Ok(());
                }
                Err(e) => {
                    ctx.error = Some(e.clone());
                    return Err(e);
                }
            }
        }
        return Ok(());
    }

    pub fn inode(&self) -> Arc<dyn IndexNode> {
        return self.inode.clone();
    }

    /// Inode selected by the pathname that opened this file. Special-file I/O
    /// may use a substituted runtime inode, but procfs fd links and fsnotify
    /// must retain Linux file->f_path identity.
    #[inline]
    pub(crate) fn path_inode(&self) -> Arc<dyn IndexNode> {
        self.fsnotify_path_inode
            .clone()
            .unwrap_or_else(|| self.inode.clone())
    }

    /// Invoke an operation on the filesystem selected for this open file.
    ///
    /// Pathname-backed files retain their selected mount and therefore avoid
    /// both a topology lookup and an Arc refcount operation in fault hot paths.
    /// Anonymous/pseudo files fall back lazily because several such inodes do
    /// not implement `fs()` and must remain constructible (for example epoll).
    #[inline]
    pub fn with_io_fs<T>(&self, op: impl FnOnce(&dyn FileSystem) -> T) -> T {
        if let Some(fs) = self.io_fs.as_ref() {
            op(fs.as_ref())
        } else {
            let fs = self.inode.fs();
            op(fs.as_ref())
        }
    }

    /// @brief 尝试克隆一个文件
    ///
    /// Allocation failures retain their admission scope until lock-free recovery.
    pub fn try_clone(&self) -> Result<File, MemoryAllocationError> {
        let position = self.position_state.lock();
        let storage = PreparedFile::try_new()?;
        let inode_retention =
            InodeRetentionGuard::new(self.inode.clone(), InodeRetentionKind::OpenFileDescription)?;
        let path_inode_retention = self
            ._path_inode_retention
            .as_ref()
            .map(|retention| retention.derive_existing(InodeRetentionKind::OpenFileDescription));
        let flags = self.flags();
        let mut mode = self.mode();
        let mount_writer = self
            ._mount_writer
            .as_ref()
            .map(MountWriteGuard::derive)
            .transpose()?;
        let private_data = Mutex::new(self.private_data.lock().clone());
        let mount_guard = self
            ._mount_guard
            .as_ref()
            .map(MountExternalGuard::derive)
            .transpose()?;

        if !mode.contains(FileMode::FMODE_PATH) {
            self.inode.open(private_data.lock(), &flags)?;
        }

        let post_write_sync = if mode.contains(FileMode::FMODE_PATH) {
            PostWriteSyncPolicy::NotApplicable
        } else {
            Self::configure_base_open_mode(&self.inode, self.file_type, &mut mode);
            let mut behavior = OpenFileBehavior::new(mode, self.file_type);
            self.inode
                .configure_open_file(&private_data.lock(), &mut behavior);
            mode = behavior.mode;
            behavior.post_write_sync
        };

        let res = storage.init(|epitems| FileData {
            open_file_id: alloc_open_file_id(),
            inode: self.inode.clone(),
            fsnotify_path_inode: self.fsnotify_path_inode.clone(),
            io_fs: self.io_fs.clone(),
            offset: AtomicUsize::new(self.offset.load(Ordering::SeqCst)),
            flags: RwSem::new(flags),
            fasync_lock: Mutex::new(()),
            mode: RwSem::new(mode),
            file_type: self.file_type,
            post_write_sync,
            position_state: Mutex::new(position.clone()),
            private_data,
            cred: self.cred.clone(),
            owner: Mutex::new(FileOwner::new()),
            posix_lock_key: self.posix_lock_key,
            append_lock_domain: self.append_lock_domain.clone(),
            ra_state: Mutex::new(self.ra_state.lock().clone()),
            wb_error_seq: Mutex::new(*self.wb_error_seq.lock()),
            sb_error_seq: Mutex::new(*self.sb_error_seq.lock()),
            epitems,
            _write_access: self._write_access.clone(),
            _mount_writer: mount_writer,
            pending_truncate_writer: None,
            _inode_retention: inode_retention,
            _path_inode_retention: path_inode_retention,
            _mount_guard: mount_guard,
        });
        return Ok(res);
    }

    /// @brief 获取文件的类型
    #[inline]
    pub fn file_type(&self) -> FileType {
        return self.file_type;
    }

    #[inline]
    pub fn open_file_id(&self) -> usize {
        self.open_file_id
    }

    #[inline]
    pub fn posix_lock_key(&self) -> (usize, InodeId) {
        self.posix_lock_key
    }

    /// 获取当前文件偏移（等价于用户态的 file position）。
    #[inline]
    pub fn pos(&self) -> usize {
        self.offset.load(Ordering::SeqCst)
    }

    /// Commit a syscall-owned transfer cursor. Stream objects have no file
    /// position. The caller owns the implicit-position transaction.
    pub(crate) fn commit_transfer_position(&self, position: usize) {
        if !self.mode().contains(FileMode::FMODE_STREAM) {
            self.offset.store(position, Ordering::SeqCst);
        }
    }

    /// 检查文件是否为普通文件或目录
    ///
    /// 用于 poll(2)/epoll 的"总是就绪"判断。
    /// Linux 语义：普通文件和目录的 I/O 操作不会阻塞，因此它们在 poll 中总是可读/可写的。
    #[inline]
    pub fn is_regular_file_or_dir(&self) -> bool {
        matches!(self.file_type, FileType::File | FileType::Dir)
    }

    /// 检查文件是否为"总是就绪"类型（用于 epoll）
    ///
    /// 满足以下条件的文件总是就绪：
    /// 1. 文件类型为普通文件或目录
    /// 2. 文件不支持 poll 操作
    ///
    /// Linux 语义：普通文件和目录的 I/O 操作不会阻塞，因此它们总是可读/可写的。
    /// 参考 Linux 内核的 DEFAULT_POLLMASK
    pub fn is_always_ready(&self) -> bool {
        !self.supports_poll() && self.is_regular_file_or_dir()
    }

    /// @brief 获取文件的打开模式
    #[inline]
    pub fn flags(&self) -> FileFlags {
        return *self.flags.read();
    }

    /// @brief 获取文件的访问模式
    #[inline]
    pub fn mode(&self) -> FileMode {
        return *self.mode.read();
    }

    pub fn set_flags(&self, mut new_flags: FileFlags) -> Result<(), SystemError> {
        // todo: 是否需要调用inode的open方法，以更新private data（假如它与flags有关的话）?
        // 也许需要加个更好的设计，让inode知晓文件的打开模式发生了变化，让它自己决定是否需要更新private data

        // Preserve lifecycle-driven FASYNC changes while merging the other
        // status flags. This lock is also held by fasync registration and TTY
        // hangup cleanup.
        let _fasync_guard = self.fasync_lock.lock();

        // 访问模式不可修改
        let old_flags = self.flags();
        let old_accflags = old_flags.access_flags();
        let new_accflags = new_flags.access_flags();

        if old_accflags != new_accflags {
            return Err(SystemError::EINVAL);
        }

        // 只有部分标志可修改
        const SETFL_MASK: u32 = FileFlags::O_APPEND.bits()
            | FileFlags::O_NONBLOCK.bits()
            | FileFlags::O_DIRECT.bits()
            | FileFlags::O_NOATIME.bits();

        let new_bits = (new_flags.bits() & SETFL_MASK) | (old_flags.bits() & !SETFL_MASK);
        new_flags = FileFlags::from_bits_truncate(new_bits);

        self.private_data.lock().update_flags(new_flags)?;
        // Socket operations such as accept consult their cached mode. Keep it
        // coherent for both F_SETFL and FIONBIO, under the same update lock.
        if let Some(socket) = self.inode.as_socket() {
            socket.set_nonblocking(new_flags.contains(FileFlags::O_NONBLOCK));
        }
        // 更新文件的打开模式
        *self.flags.write() = new_flags;

        // Sync the O_NONBLOCK change to the inotify fd (its read_at checks an internal AtomicBool rather than FileFlags,
        // same as socket's set_nonblocking).
        if new_flags.contains(FileFlags::O_NONBLOCK) != old_flags.contains(FileFlags::O_NONBLOCK) {
            if let Some(ino) = self
                .inode
                .as_any_ref()
                .downcast_ref::<crate::filesystem::inotify::InotifyInode>()
            {
                ino.set_nonblocking(new_flags.contains(FileFlags::O_NONBLOCK));
            }
        }
        return Ok(());
    }

    /// Serialize a fasync registration update with the corresponding status
    /// bit commit.
    ///
    /// Keeping the dedicated fasync lock across the inode callback lets inode
    /// lifecycle paths (notably TTY hangup) clear a registered item and its
    /// FASYNC bit without racing a delayed post-callback commit.
    pub(crate) fn update_fasync_flag<F>(&self, enabled: bool, update: F) -> Result<(), SystemError>
    where
        F: FnOnce() -> Result<bool, SystemError>,
    {
        let _guard = self.fasync_lock.lock();
        if self.flags().contains(FileFlags::FASYNC) == enabled {
            return Ok(());
        }

        if update()? {
            let mut flags = self.flags.write();
            if enabled {
                flags.insert(FileFlags::FASYNC);
            } else {
                flags.remove(FileFlags::FASYNC);
            }
        }
        Ok(())
    }

    /// Clear the handler-owned FASYNC status bit during inode teardown.
    pub(crate) fn clear_fasync_flag(&self) {
        let _guard = self.fasync_lock.lock();
        let mut flags = self.flags.write();
        flags.remove(FileFlags::FASYNC);
    }

    /// @brief 重新设置文件的大小
    ///
    /// 如果文件大小增加，则文件内容不变，但是文件的空洞部分会被填充为0
    /// 如果文件大小减小，则文件内容会被截断
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    pub fn ftruncate(&self, len: usize) -> Result<(), SystemError> {
        // 类型必须是普通文件，否则 EINVAL
        let md = self.inode.metadata()?;
        if md.file_type != FileType::File {
            return Err(SystemError::EINVAL);
        }

        // O_PATH 直接返回 EBADF，保持与 open 时的行为一致
        let mode = *self.mode.read();
        if mode.contains(FileMode::FMODE_PATH) {
            return Err(SystemError::EBADF);
        }

        // 非可写打开返回 EINVAL（对齐 gVisor 预期）
        if !mode.contains(FileMode::FMODE_WRITE) || !mode.can_write() {
            return Err(SystemError::EINVAL);
        }

        // RLIMIT_FSIZE 检查
        let current_pcb = ProcessManager::current_pcb();
        let fsize_limit = current_pcb.get_rlimit(RLimitID::Fsize);
        if fsize_limit.rlim_cur != u64::MAX && len as u64 > fsize_limit.rlim_cur {
            let _ = send_signal_to_pid(current_pcb.raw_pid(), Signal::SIGXFSZ);
            return Err(SystemError::EFBIG);
        }

        // 统一通过 VFS 封装，复用类型/只读检查，同时保留 fd 上下文。
        Self::recover_io(|| {
            crate::filesystem::vfs::vcore::vfs_truncate_file(
                self.inode(),
                len,
                crate::filesystem::vfs::vcore::current_file_lock_owner_id(),
                || self.private_data.lock(),
            )
        })?;
        return Ok(());
    }

    /// Add an EPollItem to the file
    pub fn add_epitem(&self, epitem: Arc<EPollItem>) -> Result<(), SystemError> {
        let private_data = self.private_data.lock();
        self.inode
            .as_pollable_inode()?
            .add_epitem(epitem.clone(), &private_data)?;
        self.epitems.add(epitem);
        Ok(())
    }

    /// Remove epitems associated with the epoll
    pub fn remove_epitem(&self, epitem: &Arc<EPollItem>) -> Result<(), SystemError> {
        let private_data = self.private_data.lock();
        let result = self
            .inode
            .as_pollable_inode()?
            .remove_epitem(epitem, &private_data);
        let _ = self.epitems.remove(epitem);
        result
    }

    /// Poll the file for events
    pub fn poll(&self) -> Result<usize, SystemError> {
        let private_data = self.private_data.lock();
        self.inode.as_pollable_inode()?.poll(&private_data)
    }

    /// 检查文件是否支持 poll 操作
    #[inline]
    pub fn supports_poll(&self) -> bool {
        self.inode.as_pollable_inode().is_ok()
    }

    pub fn owner(&self) -> Option<RawPid> {
        self.owner
            .lock()
            .pcb
            .as_ref()
            .map(|pcb| pcb.pid().pid_vnr())
    }

    /// Get the owner process control block
    pub fn get_owner(&self) -> Option<Arc<ProcessControlBlock>> {
        self.owner.lock().pcb.clone()
    }

    pub fn owner_snapshot(&self) -> FileOwnerSnapshot {
        self.owner.lock().snapshot()
    }

    /// Set a process (group) as owner of the file descriptor.
    ///
    /// Such that this process (group) will receive `SIGIO` and `SIGURG` signals
    /// for I/O events on the file descriptor, if `O_ASYNC` status flag is set
    /// on this file.
    pub fn set_owner(&self, pid: Option<Arc<ProcessControlBlock>>) -> Result<(), SystemError> {
        self.owner.lock().pcb = pid;
        Ok(())
    }

    pub fn owner_signum(&self) -> i32 {
        self.owner.lock().signum
    }

    pub fn set_owner_signum(&self, signum: i32) {
        self.owner.lock().signum = signum;
    }

    pub fn get_inode_flags(&self) -> Result<InodeFlags, SystemError> {
        let metadata = self.inode.metadata()?;
        Ok(metadata.flags)
    }

    /// 修改文件访问模式标志
    pub fn set_mode_flags(&self, flags: FileMode) {
        self.mode.write().insert(flags);
    }

    /// 清除文件访问模式标志
    pub fn remove_mode_flags(&self, flags: FileMode) {
        self.mode.write().remove(flags);
    }

    /// 设置预读窗口大小
    pub fn set_ra_pages(&self, pages: usize) {
        self.ra_state.lock().ra_pages = pages;
    }

    pub fn get_ra_state(&self) -> FileReadaheadState {
        self.ra_state.lock().clone()
    }

    pub fn set_ra_state(&self, ra_state: FileReadaheadState) -> Result<(), SystemError> {
        *self.ra_state.lock() = ra_state;
        Ok(())
    }
}

impl Drop for File {
    fn drop(&mut self) {
        // A pending open that failed never acquired an open handle, fasync
        // registration or close event. Its owned resources still retire below.
        if !self.mode().contains(FileMode::FMODE_OPENED) {
            return;
        }
        let epitems = self.epitems.take_all();
        for epitem in epitems {
            EventPoll::release_file_epitem(&epitem);
            let _ = self.remove_epitem(&epitem);
        }
        // fsnotify: the final close → IN_CLOSE_WRITE / IN_CLOSE_NOWRITE (FMODE_NONOTIFY short-circuits).
        // At this point self.inode is still alive (Drop runs before inode.close()), so it is safe to take the inode_id.
        if fsnotify::has_any_watch() {
            let mode = *self.mode.read();
            if mode.contains(FileMode::FMODE_OPENED)
                && !mode.intersects(FileMode::FMODE_NONOTIFY | FileMode::FMODE_PATH)
            {
                let m = if mode.contains(FileMode::FMODE_WRITE) {
                    FsEvent::CLOSE_WRITE
                } else {
                    FsEvent::CLOSE_NOWRITE
                };
                self.notify_fs_event(m);
            }
        }

        if self.flags().contains(FileFlags::FASYNC) {
            if let Ok(pollable) = self.inode.as_pollable_inode() {
                let private_data = self.private_data.lock();
                let _ = pollable.release_fasync(self, &private_data);
            }
        }

        super::flock::release_all_for_file(self);
        if self.mode().contains(FileMode::FMODE_OPENED)
            && !self.mode().contains(FileMode::FMODE_PATH)
        {
            let r: Result<(), SystemError> = self.inode.close(self.private_data.lock());
            // 打印错误信息
            if let Err(e) = r {
                error!(
                    "pid: {:?} failed to close file: {:?}, errno={:?}",
                    ProcessManager::current_pcb().raw_pid(),
                    self,
                    e
                );
            }
        }
    }
}
