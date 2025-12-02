use core::{
    fmt,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use alloc::{string::String, sync::Arc, vec::Vec};
use log::error;
use system_error::SystemError;

use super::{FileType, IndexNode, InodeId, Metadata, SpecialNodeData};
use crate::process::pid::PidPrivateData;
use crate::{
    arch::MMArch,
    driver::{
        base::{block::SeekFrom, device::DevicePrivateData},
        tty::tty_device::TtyFilePrivateData,
    },
    filesystem::{
        epoll::{event_poll::EPollPrivateData, EPollItem},
        procfs::ProcfsFilePrivateData,
        vfs::FilldirContext,
    },
    ipc::{kill::kill_process, pipe::PipeFsPrivateData},
    libs::{rwlock::RwLock, spinlock::SpinLock},
    mm::{
        page::PageFlags,
        readahead::{page_cache_async_readahead, page_cache_sync_readahead, FileReadaheadState},
        MemoryManagementArch,
    },
    process::{
        cred::Cred,
        namespace::{
            ipc_namespace::IpcNamespace, mnt::MntNamespace, net_namespace::NetNamespace,
            pid_namespace::PidNamespace, user_namespace::UserNamespace,
            uts_namespace::UtsNamespace,
        },
        resource::RLimitID,
        ProcessControlBlock, ProcessManager, RawPid,
    },
};

const MAX_LFS_FILESIZE: i64 = i64::MAX;
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
    // Time/cgroup namespaces are not implemented yet.
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
        }
    }
}

/// 文件私有信息的枚举类型
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum FilePrivateData {
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
    /// namespace fd 私有信息（/proc/thread-self/ns/* 打开后得到）
    Namespace(NamespaceFilePrivateData),
    /// 不需要文件私有信息
    Unused,
}

impl Default for FilePrivateData {
    fn default() -> Self {
        return Self::Unused;
    }
}

impl FilePrivateData {
    pub fn update_mode(&mut self, mode: FileMode) {
        if let FilePrivateData::Pipefs(pdata) = self {
            pdata.set_mode(mode);
        }
    }

    pub fn is_pid(&self) -> bool {
        if let FilePrivateData::Pid(_data) = self {
            return true;
        }
        false
    }

    pub fn get_pid(&self) -> i32 {
        if let FilePrivateData::Pid(data) = self {
            return data.pid();
        }
        -1
    }
}

bitflags! {
    /// @brief 文件打开模式
    /// 其中，低2bit组合而成的数字的值，用于表示访问权限。其他的bit，才支持通过按位或的方式来表示参数
    ///
    /// 与Linux 5.19.10的uapi/asm-generic/fcntl.h相同
    /// https://code.dragonos.org.cn/xref/linux-5.19.10/tools/include/uapi/asm-generic/fcntl.h#19
    #[allow(clippy::bad_bit_mask)]
    pub struct FileMode: u32{
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
        const O_SYNC = 0o04000000;

        const O_PATH = 0o10000000;

        const O_PATH_FLAGS = Self::O_DIRECTORY.bits|Self::O_NOFOLLOW.bits|Self::O_CLOEXEC.bits|Self::O_PATH.bits;
    }
}

impl FileMode {
    /// @brief 获取文件的访问模式的值
    #[inline]
    pub fn accmode(&self) -> u32 {
        return self.bits() & FileMode::O_ACCMODE.bits();
    }
}

/// @brief 抽象文件结构体
#[derive(Debug)]
pub struct File {
    inode: Arc<dyn IndexNode>,
    /// 对于文件，表示字节偏移量；对于文件夹，表示当前操作的子目录项偏移量
    offset: AtomicUsize,
    /// 文件的打开模式
    mode: RwLock<FileMode>,
    /// 文件类型
    file_type: FileType,
    /// readdir时候用的，暂存的本次循环中，所有子目录项的名字的数组
    readdir_subdirs_name: SpinLock<Vec<String>>,
    pub private_data: SpinLock<FilePrivateData>,
    /// 文件的凭证
    cred: Arc<Cred>,
    /// 文件描述符标志：是否在execve时关闭
    close_on_exec: AtomicBool,
    /// owner
    pid: SpinLock<Option<Arc<ProcessControlBlock>>>,
    /// 预读状态
    ra_state: SpinLock<FileReadaheadState>,
}

impl File {
    /// @brief 创建一个新的文件对象
    ///
    /// @param inode 文件对象对应的inode
    /// @param mode 文件的打开模式
    pub fn new(inode: Arc<dyn IndexNode>, mut mode: FileMode) -> Result<Self, SystemError> {
        let mut inode = inode;
        let file_type = inode.metadata()?.file_type;
        // 检查是否为命名管道（FIFO）
        let is_named_pipe = if file_type == FileType::Pipe {
            if let Some(SpecialNodeData::Pipe(pipe_inode)) = inode.special_node() {
                inode = pipe_inode;
                true
            } else {
                false
            }
        } else {
            false
        };

        // 对于命名管道，自动添加 O_LARGEFILE 标志（符合 Linux 行为）
        if is_named_pipe {
            mode.insert(FileMode::O_LARGEFILE);
        }

        let close_on_exec = mode.contains(FileMode::O_CLOEXEC);
        mode.remove(FileMode::O_CLOEXEC);

        let private_data = SpinLock::new(FilePrivateData::default());
        inode.open(private_data.lock(), &mode)?;

        let f = File {
            inode,
            offset: AtomicUsize::new(0),
            mode: RwLock::new(mode),
            file_type,
            readdir_subdirs_name: SpinLock::new(Vec::default()),
            private_data,
            cred: ProcessManager::current_pcb().cred(),
            close_on_exec: AtomicBool::new(close_on_exec),
            pid: SpinLock::new(None),
            ra_state: SpinLock::new(FileReadaheadState::new()),
        };

        return Ok(f);
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
        self.do_read(
            self.offset.load(core::sync::atomic::Ordering::SeqCst),
            len,
            buf,
            true,
        )
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
        self.do_write(
            self.offset.load(core::sync::atomic::Ordering::SeqCst),
            len,
            buf,
            true,
        )
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
        self.do_read(offset, len, buf, false)
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
        self.do_write(offset, len, buf, false)
    }

    fn file_readahead(&self, offset: usize, len: usize) -> Result<(), SystemError> {
        let page_cache = match self.inode.page_cache() {
            Some(page_cahce) => page_cahce,
            None => return Ok(()),
        };

        let start_page = offset >> MMArch::PAGE_SHIFT;
        let end_page = (offset + len - 1) >> MMArch::PAGE_SHIFT;

        let (async_trigger_page, missing_page) = {
            let page_cache_guard = page_cache.lock_irqsave();
            let mut async_trigger_page = None;
            let mut missing_page = None;

            for index in start_page..=end_page {
                match page_cache_guard.get_page(index) {
                    Some(page)
                        if page
                            .read_irqsave()
                            .flags()
                            .contains(PageFlags::PG_READAHEAD) =>
                    {
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
            page.write_irqsave().remove_flags(PageFlags::PG_READAHEAD);

            page_cache_async_readahead(&page_cache, &self.inode, &mut ra_state, index, req_pages)?;
            *self.ra_state.lock() = ra_state;
        } else if let Some(index) = missing_page {
            let mut ra_state = self.ra_state.lock().clone();
            let req_pages = end_page - index + 1;

            page_cache_sync_readahead(&page_cache, &self.inode, &mut ra_state, index, req_pages)?;
            *self.ra_state.lock() = ra_state;
        }
        Ok(())
    }

    pub fn do_read(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        update_offset: bool,
    ) -> Result<usize, SystemError> {
        // 先检查本文件在权限等规则下，是否可读取。
        self.readable()?;
        if buf.len() < len {
            return Err(SystemError::ENOBUFS);
        }

        if self.file_type == FileType::File && !self.mode().contains(FileMode::O_DIRECT) {
            self.file_readahead(offset, len)?;
        }

        let len = if self.mode().contains(FileMode::O_DIRECT) {
            self.inode
                .read_direct(offset, len, buf, self.private_data.lock())
        } else {
            self.inode
                .read_at(offset, len, buf, self.private_data.lock())
        }?;

        if len > 0 {
            let last_page_readed = (offset + len - 1) >> MMArch::PAGE_SHIFT;
            self.ra_state.lock().prev_index = last_page_readed as i64;
        }

        if update_offset {
            self.offset
                .fetch_add(len, core::sync::atomic::Ordering::SeqCst);
        }
        Ok(len)
    }

    pub fn do_write(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        update_offset: bool,
    ) -> Result<usize, SystemError> {
        // 先检查本文件在权限等规则下，是否可写入。
        self.writeable()?;
        if buf.len() < len {
            return Err(SystemError::ENOBUFS);
        }
        // 获取文件类型
        let md = self.inode.metadata()?;
        let file_type = md.file_type;

        // 检查RLIMIT_FSIZE限制（仅对常规文件生效）
        let actual_len = if matches!(file_type, FileType::File) {
            let current_pcb = ProcessManager::current_pcb();
            let fsize_limit = current_pcb.get_rlimit(RLimitID::Fsize);

            if fsize_limit.rlim_cur != u64::MAX {
                let limit = fsize_limit.rlim_cur as usize;

                // 如果当前文件大小已经达到或超过限制，不允许写入
                if offset >= limit {
                    // 发送SIGXFSZ信号
                    let _ = kill_process(
                        current_pcb.raw_pid(),
                        crate::arch::ipc::signal::Signal::SIGXFSZ,
                    );
                    return Err(SystemError::EFBIG);
                }

                // 计算可写入的最大长度（不超过限制）
                let max_writable = limit.saturating_sub(offset);
                if len > max_writable {
                    max_writable
                } else {
                    len
                }
            } else {
                len
            }
        } else {
            len
        };

        // 仅常规文件考虑“指针超过大小则扩展”语义；管道/字符设备等不应触发 resize
        if matches!(file_type, FileType::File) && offset > md.size as usize {
            self.inode.resize(offset)?;
        }
        let written_len = self
            .inode
            .write_at(offset, actual_len, buf, self.private_data.lock())?;

        if update_offset {
            self.offset
                .fetch_add(written_len, core::sync::atomic::Ordering::SeqCst);
        }

        Ok(written_len)
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
        let file_type = self.inode.metadata()?.file_type;
        match file_type {
            FileType::Pipe | FileType::CharDevice => {
                return Err(SystemError::ESPIPE);
            }
            _ => {}
        }
        // Check for procfs private data. If this is a procfs pseudo-file, disallow SEEK_END
        // and other unsupported seek modes.
        {
            let pdata = self.private_data.lock();
            if let FilePrivateData::Procfs(_) = &*pdata {
                match origin {
                    SeekFrom::SeekEnd(_) | SeekFrom::Invalid => {
                        return Err(SystemError::EINVAL);
                    }
                    _ => {}
                }
            }
        }
        let pos: i64 = match origin {
            SeekFrom::SeekSet(offset) => offset,
            SeekFrom::SeekCurrent(offset) => self.offset.load(Ordering::SeqCst) as i64 + offset,
            SeekFrom::SeekEnd(offset) => {
                if FileType::Dir == file_type {
                    // 对目录，返回 Linux 常见语义：允许 SEEK_END 并返回 MAX_LFS_FILESIZE。
                    // 测试接受 MAX_LFS_FILESIZE 或 EINVAL，但为通过当前测试选择返回 MAX_LFS_FILESIZE。
                    return Ok(MAX_LFS_FILESIZE as usize);
                }
                let metadata = self.metadata()?;
                metadata.size + offset
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
        // 暂时认为只要不是write only, 就可读
        if mode.accmode() == FileMode::O_WRONLY.bits || mode.contains(FileMode::O_PATH) {
            return Err(SystemError::EBADF);
        }

        return Ok(());
    }

    /// @brief 判断当前文件是否可写
    #[inline]
    pub fn writeable(&self) -> Result<(), SystemError> {
        let mode = *self.mode.read();

        // 检查是否是O_PATH文件描述符
        if mode.contains(FileMode::O_PATH) {
            return Err(SystemError::EBADF);
        }

        // 暂时认为只要不是read only, 就可写
        // 根据 POSIX，尝试写入只读文件描述符应返回 EBADF
        if mode.accmode() == FileMode::O_RDONLY.bits() {
            return Err(SystemError::EBADF);
        }

        return Ok(());
    }

    /// # 读取目录项
    ///
    /// ## 参数
    /// - ctx 填充目录项的上下文
    pub fn read_dir(&self, ctx: &mut FilldirContext) -> Result<(), SystemError> {
        // O_PATH 文件描述符只能用于有限的操作，getdents/getdents64
        // 在 Linux 中会返回 EBADF。提前检测并返回相同语义。
        if self.mode().contains(FileMode::O_PATH) {
            return Err(SystemError::EBADF);
        }

        // 仅目录允许读取目录项，其它类型遵循 POSIX 语义返回 ENOTDIR。
        if self.file_type() != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        let inode: &Arc<dyn IndexNode> = &self.inode;
        let mut current_pos = self.offset.load(Ordering::SeqCst);

        // POSIX 标准要求readdir应该返回. 和 ..
        // 但是观察到在现有的子目录中已经包含，不做处理也能正常返回. 和 .. 这里先不做处理

        // 迭代读取目录项
        // 为了保证在目录内容动态变化（例如 /proc/self/fd）时不会因为重新
        // 创建列表而丢失尚未读取的目录项，这里缓存第一次生成的列表，在
        // 文件偏移被 seek 到 0 之前复用该缓存。
        let mut cached_names = self.readdir_subdirs_name.lock();
        if current_pos == 0 || cached_names.is_empty() {
            *cached_names = inode.list()?;
        }
        let readdir_subdirs_name = cached_names.clone();
        drop(cached_names);

        let subdirs_name_len = readdir_subdirs_name.len();
        while current_pos < subdirs_name_len {
            let name = &readdir_subdirs_name[current_pos];
            let sub_inode: Arc<dyn IndexNode> = match inode.find(name) {
                Ok(i) => i,
                Err(e) => {
                    if e == SystemError::ENOENT {
                        // 目录项在本次读取过程中被移除，跳过它，继续读取后续条目
                        self.offset.fetch_add(1, Ordering::SeqCst);
                        current_pos += 1;
                        continue;
                    }
                    error!("Readdir error: Failed to find sub inode");
                    return Err(e);
                }
            };

            let inode_metadata = sub_inode.metadata().unwrap();
            let entry_ino = inode_metadata.inode_id.into() as u64;
            let entry_d_type = inode_metadata.file_type.get_file_type_num() as u8;
            match ctx.fill_dir(name, current_pos, entry_ino, entry_d_type) {
                Ok(_) => {
                    self.offset.fetch_add(1, Ordering::SeqCst);
                    current_pos += 1;
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

    /// @brief 尝试克隆一个文件
    ///
    /// @return Option<File> 克隆后的文件结构体。如果克隆失败，返回None
    pub fn try_clone(&self) -> Option<File> {
        let res = Self {
            inode: self.inode.clone(),
            offset: AtomicUsize::new(self.offset.load(Ordering::SeqCst)),
            mode: RwLock::new(self.mode()),
            file_type: self.file_type,
            readdir_subdirs_name: SpinLock::new(self.readdir_subdirs_name.lock().clone()),
            private_data: SpinLock::new(self.private_data.lock().clone()),
            cred: self.cred.clone(),
            close_on_exec: AtomicBool::new(self.close_on_exec.load(Ordering::SeqCst)),
            pid: SpinLock::new(None),
            ra_state: SpinLock::new(self.ra_state.lock().clone()),
        };
        // 调用inode的open方法，让inode知道有新的文件打开了这个inode
        // TODO: reopen is not a good idea for some inodes, need a better design
        if self
            .inode
            .open(res.private_data.lock(), &res.mode())
            .is_err()
        {
            return None;
        }

        return Some(res);
    }

    /// @brief 获取文件的类型
    #[inline]
    pub fn file_type(&self) -> FileType {
        return self.file_type;
    }

    /// @brief 获取文件的打开模式
    #[inline]
    pub fn mode(&self) -> FileMode {
        return *self.mode.read();
    }

    /// 获取文件是否在execve时关闭
    #[inline]
    pub fn close_on_exec(&self) -> bool {
        return self.close_on_exec.load(Ordering::SeqCst);
    }

    /// 设置文件是否在execve时关闭
    #[inline]
    pub fn set_close_on_exec(&self, close_on_exec: bool) {
        self.close_on_exec.store(close_on_exec, Ordering::SeqCst);
    }

    pub fn set_mode(&self, mut mode: FileMode) -> Result<(), SystemError> {
        // todo: 是否需要调用inode的open方法，以更新private data（假如它与mode有关的话）?
        // 也许需要加个更好的设计，让inode知晓文件的打开模式发生了变化，让它自己决定是否需要更新private data

        // 提取 O_CLOEXEC 状态并更新 close_on_exec 字段
        let close_on_exec = mode.contains(FileMode::O_CLOEXEC);
        self.close_on_exec.store(close_on_exec, Ordering::SeqCst);

        // 从 mode 中移除 O_CLOEXEC 标志，保持与构造函数一致的行为
        mode.remove(FileMode::O_CLOEXEC);

        // 更新文件的打开模式
        *self.mode.write() = mode;
        self.private_data.lock().update_mode(mode);
        return Ok(());
    }

    /// @brief 重新设置文件的大小
    ///
    /// 如果文件大小增加，则文件内容不变，但是文件的空洞部分会被填充为0
    /// 如果文件大小减小，则文件内容会被截断
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    pub fn ftruncate(&self, len: usize) -> Result<(), SystemError> {
        // 如果文件不可写，返回错误
        self.writeable()?;

        // 统一通过 VFS 封装，复用类型/只读检查
        crate::filesystem::vfs::vcore::vfs_truncate(self.inode(), len)?;
        return Ok(());
    }

    /// Add an EPollItem to the file
    pub fn add_epitem(&self, epitem: Arc<EPollItem>) -> Result<(), SystemError> {
        let private_data = self.private_data.lock();
        self.inode
            .as_pollable_inode()?
            .add_epitem(epitem, &private_data)
    }

    /// Remove epitems associated with the epoll
    pub fn remove_epitem(&self, epitem: &Arc<EPollItem>) -> Result<(), SystemError> {
        let private_data = self.private_data.lock();
        self.inode
            .as_pollable_inode()?
            .remove_epitem(epitem, &private_data)
    }

    /// Poll the file for events
    pub fn poll(&self) -> Result<usize, SystemError> {
        let private_data = self.private_data.lock();
        self.inode.as_pollable_inode()?.poll(&private_data)
    }

    pub fn owner(&self) -> Option<RawPid> {
        self.pid.lock().as_ref().map(|pcb| pcb.raw_pid())
    }

    /// Set a process (group) as owner of the file descriptor.
    ///
    /// Such that this process (group) will receive `SIGIO` and `SIGURG` signals
    /// for I/O events on the file descriptor, if `O_ASYNC` status flag is set
    /// on this file.
    pub fn set_owner(&self, pid: Option<Arc<ProcessControlBlock>>) -> Result<(), SystemError> {
        let Some(pcb) = pid else {
            *self.pid.lock() = None;
            return Ok(());
        };

        self.pid.lock().replace(pcb);
        // todo: update inode owner
        log::error!("set_owner has not been implemented yet");
        Ok(())
    }
}

impl Drop for File {
    fn drop(&mut self) {
        let r: Result<(), SystemError> = self.inode.close(self.private_data.lock());
        // 打印错误信息
        if r.is_err() {
            error!(
                "pid: {:?} failed to close file: {:?}, errno={:?}",
                ProcessManager::current_pcb().raw_pid(),
                self,
                r.as_ref().unwrap_err()
            );
        }
    }
}

/// @brief pcb里面的文件描述符数组
#[derive(Debug)]
pub struct FileDescriptorVec {
    /// 当前进程打开的文件描述符
    fds: Vec<Option<Arc<File>>>,
}
impl Default for FileDescriptorVec {
    fn default() -> Self {
        Self::new()
    }
}
impl FileDescriptorVec {
    /// 文件描述符表的初始容量
    pub const INITIAL_CAPACITY: usize = 1024;
    /// 文件描述符表的最大容量限制（防止无限扩容）
    pub const MAX_CAPACITY: usize = 65536;

    #[inline(never)]
    pub fn new() -> FileDescriptorVec {
        let mut data = Vec::with_capacity(FileDescriptorVec::INITIAL_CAPACITY);
        data.resize(FileDescriptorVec::INITIAL_CAPACITY, None);

        // 初始化文件描述符数组结构体
        return FileDescriptorVec { fds: data };
    }

    /// @brief 克隆一个文件描述符数组
    ///
    /// @return FileDescriptorVec 克隆后的文件描述符数组
    pub fn clone(&self) -> FileDescriptorVec {
        let mut res = FileDescriptorVec::new();
        // 调整容量以匹配源文件描述符表
        let _ = res.resize_to_capacity(self.fds.len());

        for i in 0..self.fds.len() {
            if let Some(file) = &self.fds[i] {
                res.fds[i] = Some(file.clone());
            }
        }
        return res;
    }

    /// 返回当前已占用的最高文件描述符索引（若无则为None）
    #[inline]
    fn highest_open_index(&self) -> Option<usize> {
        // 从高到低查找第一个占用的槽位
        (0..self.fds.len()).rev().find(|&i| self.fds[i].is_some())
    }

    /// 扩容文件描述符表到指定容量
    ///
    /// ## 参数
    /// - `new_capacity`: 新的容量大小
    ///
    /// ## 返回值
    /// - `Ok(())`: 扩容成功
    /// - `Err(SystemError)`: 扩容失败
    fn resize_to_capacity(&mut self, new_capacity: usize) -> Result<(), SystemError> {
        if new_capacity > FileDescriptorVec::MAX_CAPACITY {
            return Err(SystemError::EMFILE);
        }

        let current_len = self.fds.len();
        if new_capacity > current_len {
            // 扩容：扩展向量并填充None
            // 使用 try_reserve 先检查内存分配是否可能成功
            if self.fds.try_reserve(new_capacity - current_len).is_err() {
                return Err(SystemError::ENOMEM);
            }
            self.fds.resize(new_capacity, None);
        } else if new_capacity < current_len {
            // 缩容：允许，但不能丢弃仍在使用的高位fd。
            // 若高位fd仍在使用，将缩容目标提升到 (最高已用fd + 1)。
            let floor = self.highest_open_index().map(|idx| idx + 1).unwrap_or(0);
            let target = core::cmp::max(new_capacity, floor);
            if target < current_len {
                self.fds.truncate(target);
            }
        }
        Ok(())
    }

    /// 返回 `已经打开的` 文件描述符的数量
    pub fn fd_open_count(&self) -> usize {
        let mut size = 0;
        for fd in &self.fds {
            if fd.is_some() {
                size += 1;
            }
        }
        return size;
    }

    /// @brief 判断文件描述符序号是否合法
    ///
    /// @return true 合法
    ///
    /// @return false 不合法
    #[inline]
    pub fn validate_fd(&self, fd: i32) -> bool {
        return !(fd < 0 || fd as usize >= self.fds.len());
    }

    /// 申请文件描述符，并把文件对象存入其中。
    ///
    /// ## 参数
    ///
    /// - `file` 要存放的文件对象
    /// - `fd` 如果为Some(i32)，表示指定要申请这个文件描述符，如果这个文件描述符已经被使用，那么返回EBADF
    ///
    /// ## 返回值
    ///
    /// - `Ok(i32)` 申请成功，返回申请到的文件描述符
    /// - `Err(SystemError)` 申请失败，返回错误码，并且，file对象将被drop掉
    pub fn alloc_fd(&mut self, file: File, fd: Option<i32>) -> Result<i32, SystemError> {
        // 获取RLIMIT_NOFILE限制
        let nofile_limit = crate::process::ProcessManager::current_pcb()
            .get_rlimit(crate::process::resource::RLimitID::Nofile)
            .rlim_cur as usize;

        if let Some(new_fd) = fd {
            // 检查指定的文件描述符是否在有效范围内
            if new_fd < 0 || new_fd as usize >= nofile_limit {
                return Err(SystemError::EMFILE);
            }

            // 如果指定的fd超出当前容量，需要扩容
            if new_fd as usize >= self.fds.len() {
                self.resize_to_capacity(new_fd as usize + 1)?;
            }

            let x = &mut self.fds[new_fd as usize];
            if x.is_none() {
                *x = Some(Arc::new(file));
                return Ok(new_fd);
            } else {
                return Err(SystemError::EBADF);
            }
        } else {
            // 没有指定要申请的文件描述符编号，在有效范围内查找空位
            let max_search = core::cmp::min(self.fds.len(), nofile_limit);
            for i in 0..max_search {
                if self.fds[i].is_none() {
                    self.fds[i] = Some(Arc::new(file));
                    return Ok(i as i32);
                }
            }
            return Err(SystemError::EMFILE);
        }
    }

    /// 根据文件描述符序号，获取文件结构体的Arc指针
    ///
    /// ## 参数
    ///
    /// - `fd` 文件描述符序号
    pub fn get_file_by_fd(&self, fd: i32) -> Option<Arc<File>> {
        if !self.validate_fd(fd) {
            return None;
        }
        self.fds[fd as usize].clone()
    }

    /// 当RLIMIT_NOFILE变化时调整文件描述符表容量
    ///
    /// ## 参数
    /// - `new_rlimit_nofile`: 新的RLIMIT_NOFILE值
    ///
    /// ## 返回值
    /// - `Ok(())`: 调整成功
    /// - `Err(SystemError)`: 调整失败
    pub fn adjust_for_rlimit_change(
        &mut self,
        new_rlimit_nofile: usize,
    ) -> Result<(), SystemError> {
        // 目标容量不超过实现上限
        let desired = core::cmp::min(new_rlimit_nofile, FileDescriptorVec::MAX_CAPACITY);
        if desired >= self.fds.len() {
            // rlimit 变大：扩容到 desired
            self.resize_to_capacity(desired)
        } else {
            // rlimit 变小：按用户建议，缩容到 max(desired, 最高已用fd+1)
            let floor = self.highest_open_index().map(|idx| idx + 1).unwrap_or(0);
            let target = core::cmp::max(desired, floor);
            self.resize_to_capacity(target)
        }
    }

    /// 释放文件描述符，同时关闭文件。
    ///
    /// ## 参数
    ///
    /// - `fd` 文件描述符序号
    pub fn drop_fd(&mut self, fd: i32) -> Result<Arc<File>, SystemError> {
        self.get_file_by_fd(fd).ok_or(SystemError::EBADF)?;

        // 把文件描述符数组对应位置设置为空
        let file = self.fds[fd as usize].take().unwrap();
        return Ok(file);
    }

    #[allow(dead_code)]
    pub fn iter(&self) -> FileDescriptorIterator<'_> {
        return FileDescriptorIterator::new(self);
    }

    pub fn close_on_exec(&mut self) {
        for i in 0..self.fds.len() {
            if let Some(file) = &self.fds[i] {
                let to_drop = file.close_on_exec();
                if to_drop {
                    if let Err(r) = self.drop_fd(i as i32) {
                        error!(
                            "Failed to close file: pid = {:?}, fd = {}, error = {:?}",
                            ProcessManager::current_pcb().raw_pid(),
                            i,
                            r
                        );
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct FileDescriptorIterator<'a> {
    fds: &'a FileDescriptorVec,
    index: usize,
}

impl<'a> FileDescriptorIterator<'a> {
    pub fn new(fds: &'a FileDescriptorVec) -> Self {
        return Self { fds, index: 0 };
    }
}

impl Iterator for FileDescriptorIterator<'_> {
    type Item = (i32, Arc<File>);

    fn next(&mut self) -> Option<Self::Item> {
        while self.index < self.fds.fds.len() {
            let fd = self.index as i32;
            self.index += 1;
            if let Some(file) = self.fds.get_file_by_fd(fd) {
                return Some((fd, file));
            }
        }
        return None;
    }
}
