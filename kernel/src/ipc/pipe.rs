use crate::{
    arch::{ipc::signal::Signal, MMArch},
    filesystem::{
        epoll::{
            event_poll::{EventPoll, LockedEPItemLinkedList},
            EPollEventType, EPollItem,
        },
        vfs::{
            file::FileFlags, vcore::generate_inode_id, FilePrivateData, FileSystem, FileType,
            FsInfo, IndexNode, InodeFlags, InodeMode, Magic, Metadata, PollableInode, SuperBlock,
        },
    },
    ipc::signal_types::SigCode,
    libs::{
        spinlock::{SpinLock, SpinLockGuard},
        wait_queue::WaitQueue,
    },
    mm::MemoryManagementArch,
    process::{ProcessFlags, ProcessManager, ProcessState},
    syscall::user_access::UserBufferWriter,
    time::PosixTimeSpec,
};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::compiler_fence;

use alloc::sync::{Arc, Weak};
use system_error::SystemError;

use super::signal_types::{SigInfo, SigType};

/// 管道缓冲区默认大小（Linux 默认 65536 字节）
pub const PIPE_BUFF_SIZE: usize = 65536;

/// 管道缓冲区最小大小（一页大小，Linux 保证原子写入的最小单位）
pub const PIPE_MIN_SIZE: usize = 4096;

/// 管道缓冲区最大大小（Linux 默认为 1MB）
pub const PIPE_MAX_SIZE: usize = 1024 * 1024;

// FIONREAD: 获取管道中可读的字节数
const FIONREAD: u32 = 0x541B;

// 管道文件系统 - 全局单例
lazy_static! {
    static ref PIPEFS: Arc<PipeFS> = Arc::new(PipeFS);
}

/// 管道文件系统
#[derive(Debug)]
pub struct PipeFS;

impl FileSystem for PipeFS {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        // PipeFS 没有真正的根 inode，但我们需要实现这个方法
        // 返回一个空的 pipe inode 作为占位符
        LockedPipeInode::new()
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: 255,
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "pipefs"
    }

    fn super_block(&self) -> SuperBlock {
        SuperBlock::new(Magic::PIPEFS_MAGIC, MMArch::PAGE_SIZE as u64, 255)
    }
}

impl PipeFS {
    /// 获取全局 PipeFS 实例
    pub fn instance() -> Arc<PipeFS> {
        PIPEFS.clone()
    }
}

#[derive(Debug, Clone)]
pub struct PipeFsPrivateData {
    flags: FileFlags,
}

impl PipeFsPrivateData {
    pub fn new(flags: FileFlags) -> Self {
        return PipeFsPrivateData { flags };
    }

    pub fn set_flags(&mut self, flags: FileFlags) {
        self.flags = flags;
    }
}

/// @brief 管道文件i节点(锁)
#[derive(Debug)]
pub struct LockedPipeInode {
    inner: SpinLock<InnerPipeInode>,
    read_wait_queue: WaitQueue,
    write_wait_queue: WaitQueue,
    /// 用于 FIFO 打开时的阻塞等待（等待另一端打开）
    open_wait_queue: WaitQueue,
    epitems: LockedEPItemLinkedList,
}

/// @brief 管道文件i节点(无锁)
#[derive(Debug)]
pub struct InnerPipeInode {
    self_ref: Weak<LockedPipeInode>,
    /// 管道内可读的数据数
    valid_cnt: i32,
    read_pos: i32,
    write_pos: i32,
    /// 管道缓冲区数据（使用 Vec 支持动态大小）
    data: Vec<u8>,
    /// 当前缓冲区大小
    buf_size: usize,
    /// INode 元数据
    metadata: Metadata,
    reader: u32,
    writer: u32,
    had_reader: bool,
    /// 是否为命名管道（FIFO）
    /// 只有 FIFO 才需要在 open 时阻塞等待另一端
    is_fifo: bool,
    /// 读端打开计数器（只增不减，用于 FIFO 等待逻辑）
    /// 采用 Linux 内核的设计：等待计数器变化而非检查 reader > 0
    r_counter: u32,
    /// 写端打开计数器（只增不减，用于 FIFO 等待逻辑）
    w_counter: u32,
}

impl InnerPipeInode {
    pub fn poll(&self, private_data: &FilePrivateData) -> Result<usize, SystemError> {
        let mut events = EPollEventType::empty();

        let flags = if let FilePrivateData::Pipefs(PipeFsPrivateData { flags }) = private_data {
            flags
        } else {
            return Err(SystemError::EBADFD);
        };

        if !flags.is_write_only() {
            if self.valid_cnt != 0 {
                // 有数据可读
                events.insert(EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM);
            }

            // 没有写者
            if self.writer == 0 {
                events.insert(EPollEventType::EPOLLHUP)
            }
        }

        if !flags.is_read_only() {
            // 管道内数据未满
            if self.valid_cnt as usize != self.buf_size {
                events.insert(EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM);
            }

            // 没有读者
            if self.reader == 0 {
                events.insert(EPollEventType::EPOLLERR);
            }
        }

        Ok(events.bits() as usize)
    }

    fn buf_full(&self) -> bool {
        return self.valid_cnt as usize == self.buf_size;
    }
}

impl LockedPipeInode {
    pub fn new() -> Arc<Self> {
        let inner = InnerPipeInode {
            self_ref: Weak::default(),
            valid_cnt: 0,
            read_pos: 0,
            write_pos: 0,
            had_reader: false,
            data: Vec::new(), // 延迟分配：初始为空，第一次写入时分配
            buf_size: PIPE_BUFF_SIZE,

            metadata: Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: PIPE_BUFF_SIZE as i64,
                blk_size: 0,
                blocks: 0,
                atime: PosixTimeSpec::default(),
                mtime: PosixTimeSpec::default(),
                ctime: PosixTimeSpec::default(),
                btime: PosixTimeSpec::default(),
                file_type: FileType::Pipe,
                mode: InodeMode::from_bits_truncate(0o666),
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: Default::default(),
                flags: InodeFlags::empty(),
            },
            reader: 0,
            writer: 0,
            is_fifo: false, // 默认为匿名管道
            r_counter: 0,   // 初始化读端计数器
            w_counter: 0,   // 初始化写端计数器
        };
        let result = Arc::new(Self {
            inner: SpinLock::new(inner),
            read_wait_queue: WaitQueue::default(),
            write_wait_queue: WaitQueue::default(),
            open_wait_queue: WaitQueue::default(),
            epitems: LockedEPItemLinkedList::default(),
        });
        let mut guard = result.inner.lock();
        guard.self_ref = Arc::downgrade(&result);
        // 释放锁
        drop(guard); //这一步其实不需要，只要离开作用域，guard生命周期结束，自会解锁
        return result;
    }

    /// 标记此管道为命名管道（FIFO）
    /// 只有 FIFO 才需要在 open 时阻塞等待另一端
    pub fn set_fifo(&self) {
        self.inner.lock().is_fifo = true;
    }

    /// 检查是否为命名管道（FIFO）
    pub fn is_fifo(&self) -> bool {
        self.inner.lock().is_fifo
    }

    pub fn inner(&self) -> &SpinLock<InnerPipeInode> {
        &self.inner
    }

    fn readable(&self) -> bool {
        let inode = self.inner.lock();
        return inode.valid_cnt > 0 || inode.writer == 0;
    }

    fn writeable(&self) -> bool {
        let inode = self.inner.lock();
        return !inode.buf_full() || inode.reader == 0;
    }

    /// 检查写端计数器是否已变化（用于 FIFO O_RDONLY 阻塞等待）
    /// 采用 Linux 内核的设计：等待计数器变化而非检查 writer > 0
    ///
    /// 为了处理计数器溢出回绕的极端情况，采用双重检查：
    /// 1. 计数器是否变化（主要条件）
    /// 2. 当前是否有写端存在（兜底条件，处理回绕）
    fn w_counter_changed(&self, old: u32) -> bool {
        let guard = self.inner.lock();
        // 条件 1：计数器变化（正常情况）
        // 条件 2：当前有写端（处理极端的计数器回绕情况）
        guard.w_counter != old || guard.writer > 0
    }

    /// 检查读端计数器是否已变化（用于 FIFO O_WRONLY 阻塞等待）
    ///
    /// 为了处理计数器溢出回绕的极端情况，采用双重检查：
    /// 1. 计数器是否变化（主要条件）
    /// 2. 当前是否有读端存在（兜底条件，处理回绕）
    fn r_counter_changed(&self, old: u32) -> bool {
        let guard = self.inner.lock();
        // 条件 1：计数器变化（正常情况）
        // 条件 2：当前有读端（处理极端的计数器回绕情况）
        guard.r_counter != old || guard.reader > 0
    }

    /// 设置管道缓冲区大小
    /// 成功返回新的大小，失败返回错误
    pub fn set_pipe_size(&self, size: usize) -> Result<usize, SystemError> {
        // 验证请求的大小
        // Linux 限制：不能超过 /proc/sys/fs/pipe-max-size（默认 1MB）
        // 大于 i32::MAX 的值是无效的（因为在 64 位系统上 long long 可能传入超大值）
        if size > PIPE_MAX_SIZE || size > i32::MAX as usize {
            return Err(SystemError::EINVAL);
        }

        // 将请求的大小向上对齐到页面大小的倍数
        let page_size = MMArch::PAGE_SIZE;
        let new_size = if size == 0 {
            PIPE_MIN_SIZE
        } else {
            // 向上对齐到页面大小
            size.div_ceil(page_size) * page_size
        };

        // 确保不小于最小值
        let new_size = new_size.max(PIPE_MIN_SIZE);
        // 确保不大于最大值
        let new_size = new_size.min(PIPE_MAX_SIZE);

        let mut inner = self.inner.lock();

        // 如果新大小小于当前数据量，返回 EBUSY
        if new_size < inner.valid_cnt as usize {
            return Err(SystemError::EBUSY);
        }

        let old_size = inner.buf_size;
        if new_size == old_size {
            return Ok(new_size);
        }

        // 如果有数据，需要重新分配缓冲区并迁移数据
        if inner.valid_cnt > 0 {
            // 需要重新分配缓冲区
            let mut new_data = vec![0u8; new_size];
            let data_len = inner.valid_cnt as usize;
            let read_pos = inner.read_pos as usize;

            // 从旧缓冲区复制数据到新缓冲区（线性化）
            if read_pos + data_len <= old_size {
                // 数据没有跨越缓冲区边界
                new_data[..data_len].copy_from_slice(&inner.data[read_pos..read_pos + data_len]);
            } else {
                // 数据跨越了缓冲区边界
                let first_part = old_size - read_pos;
                new_data[..first_part].copy_from_slice(&inner.data[read_pos..old_size]);
                let second_part = data_len - first_part;
                new_data[first_part..data_len].copy_from_slice(&inner.data[..second_part]);
            }

            // 重置读写位置
            inner.read_pos = 0;
            inner.write_pos = data_len as i32;
            inner.data = new_data;
        } else {
            // 没有数据，只需更新大小
            // 如果缓冲区已分配，需要重新分配（大小改变）
            if !inner.data.is_empty() {
                inner.data = vec![0u8; new_size];
            }
            // 如果缓冲区未分配，保持 data 为空（延迟分配）
            // 重置读写位置（应该已经为0）
            inner.read_pos = 0;
            inner.write_pos = 0;
        }

        inner.buf_size = new_size;
        inner.metadata.size = new_size as i64;

        Ok(new_size)
    }

    /// 获取管道缓冲区大小
    pub fn get_pipe_size(&self) -> usize {
        self.inner.lock().buf_size
    }
}

impl PollableInode for LockedPipeInode {
    fn poll(&self, private_data: &FilePrivateData) -> Result<usize, SystemError> {
        self.inner.lock().poll(private_data)
    }

    fn add_epitem(
        &self,
        epitem: Arc<EPollItem>,
        _private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        self.epitems.lock().push_back(epitem);
        Ok(())
    }

    fn remove_epitem(
        &self,
        epitem: &Arc<EPollItem>,
        _private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        let mut guard = self.epitems.lock();
        let len = guard.len();
        guard.retain(|x| !Arc::ptr_eq(x, epitem));
        if len != guard.len() {
            return Ok(());
        }
        Err(SystemError::ENOENT)
    }
}

impl IndexNode for LockedPipeInode {
    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        data_guard: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let data = data_guard.clone();
        drop(data_guard);
        // 获取flags
        let flags: FileFlags;
        if let FilePrivateData::Pipefs(pdata) = &data {
            flags = pdata.flags;
        } else {
            return Err(SystemError::EBADF);
        }

        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }
        // log::debug!("pipe flags: {:?}", flags);
        // 加锁
        let mut inner_guard = self.inner.lock();

        // 如果管道里面没有数据，则唤醒写端，
        while inner_guard.valid_cnt == 0 {
            // 如果当前管道写者数为0，则返回EOF
            if inner_guard.writer == 0 {
                return Ok(0);
            }

            self.write_wait_queue
                .wakeup(Some(ProcessState::Blocked(true)));

            // 如果为非阻塞管道，直接返回错误
            if flags.contains(FileFlags::O_NONBLOCK) {
                drop(inner_guard);
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }

            // 否则在读等待队列中睡眠，并释放锁
            drop(inner_guard);
            let r = wq_wait_event_interruptible!(self.read_wait_queue, self.readable(), {});
            if r.is_err() {
                ProcessManager::current_pcb()
                    .flags()
                    .insert(ProcessFlags::HAS_PENDING_SIGNAL);
                return Err(SystemError::ERESTARTSYS);
            }

            inner_guard = self.inner.lock();
        }

        let mut num = inner_guard.valid_cnt as usize;
        // 决定要输出的字节数与起始位置
        let start = inner_guard.read_pos as usize;
        if len < inner_guard.valid_cnt as usize {
            num = len;
        }

        let buf_size = inner_guard.buf_size;
        // 采用两段复制，统一处理不跨尾、跨尾、以及 end==start 的写满/读空边界
        let first = core::cmp::min(num, buf_size - start);
        let second = num as isize - first as isize;
        // 第1段：从 start 开始直到缓冲尾部或读取完
        buf[0..first].copy_from_slice(&inner_guard.data[start..start + first]);
        // 第2段：如需要，从缓冲头部继续
        if second > 0 {
            buf[first..num].copy_from_slice(&inner_guard.data[0..second as usize]);
        }

        //更新读位置以及valid_cnt
        inner_guard.read_pos = (inner_guard.read_pos + num as i32) % buf_size as i32;
        inner_guard.valid_cnt -= num as i32;

        // 读完以后如果未读完，则唤醒下一个读者
        if inner_guard.valid_cnt > 0 {
            self.read_wait_queue
                .wakeup(Some(ProcessState::Blocked(true)));
        }

        //读完后解锁并唤醒等待在写等待队列中的进程
        self.write_wait_queue
            .wakeup(Some(ProcessState::Blocked(true)));
        let pollflag = EPollEventType::from_bits_truncate(inner_guard.poll(&data)? as u32);
        drop(inner_guard);
        // 唤醒epoll中等待的进程（忽略错误，因为状态已更新，这是尽力而为的通知）
        let _ = EventPoll::wakeup_epoll(&self.epitems, pollflag);

        //返回读取的字节数
        return Ok(num);
    }

    fn open(
        &self,
        mut data: SpinLockGuard<FilePrivateData>,
        flags: &crate::filesystem::vfs::file::FileFlags,
    ) -> Result<(), SystemError> {
        let accflags = flags.access_flags();
        let is_nonblock = flags.contains(FileFlags::O_NONBLOCK);
        let flags_copy = *flags;

        // 先设置 private_data（在可能的睡眠之前）
        // 这样即使睡眠，数据也已经设置好了
        *data = FilePrivateData::Pipefs(PipeFsPrivateData { flags: flags_copy });

        // 检查是否为命名管道（FIFO）
        // 只有 FIFO 才需要阻塞等待另一端打开
        let is_fifo = self.inner.lock().is_fifo;

        if accflags == FileFlags::O_RDONLY {
            // 读端打开
            let mut guard = self.inner.lock();
            guard.r_counter += 1; // 增加读端计数器（永不减少）
            guard.reader += 1;
            guard.had_reader = true;
            let writers = guard.writer;
            let cur_w_counter = guard.w_counter; // 记录当前写端计数器
            drop(guard);

            // 只有 FIFO 才需要处理阻塞等待
            if is_fifo {
                // 唤醒可能在等待读端的写者
                self.open_wait_queue.wakeup_all(None);

                // 如果是非阻塞模式，立即返回
                if is_nonblock {
                    return Ok(());
                }

                // 阻塞模式：等待写端计数器变化（采用 Linux 内核的设计）
                if writers == 0 {
                    // 在睡眠前必须释放 data 锁
                    drop(data);
                    let r = wq_wait_event_interruptible!(
                        self.open_wait_queue,
                        self.w_counter_changed(cur_w_counter),
                        {}
                    );
                    if r.is_err() {
                        // 被信号中断，需要回滚 reader 计数
                        // 注意：不要回滚 r_counter，它只增不减
                        // 注意：不要重置 had_reader，即使 reader 变为 0
                        let mut guard = self.inner.lock();
                        guard.reader -= 1;
                        drop(guard);
                        return Err(SystemError::EINTR);
                    }
                }
            }
        } else if accflags == FileFlags::O_WRONLY {
            // 写端打开
            if is_fifo {
                // FIFO 语义
                if is_nonblock {
                    // 非阻塞模式：如果没有读端，返回 ENXIO
                    let mut guard = self.inner.lock();
                    if guard.reader == 0 {
                        return Err(SystemError::ENXIO);
                    }
                    guard.w_counter += 1; // 增加写端计数器（永不减少）
                    guard.writer += 1;
                    drop(guard);
                } else {
                    // 阻塞模式：先增加 writer 计数，再等待读端
                    // 采用 Linux 内核的设计：等待计数器变化
                    let mut guard = self.inner.lock();
                    guard.w_counter += 1; // 增加写端计数器（永不减少）
                    guard.writer += 1;
                    let readers = guard.reader;
                    let cur_r_counter = guard.r_counter; // 记录当前读端计数器
                    drop(guard);

                    // 唤醒可能在等待写端的读者（在增加 w_counter 之后立即唤醒）
                    self.open_wait_queue.wakeup_all(None);

                    if readers == 0 {
                        // 在睡眠前必须释放 data 锁
                        drop(data);
                        // 等待读端计数器变化
                        let r = wq_wait_event_interruptible!(
                            self.open_wait_queue,
                            self.r_counter_changed(cur_r_counter),
                            {}
                        );
                        if r.is_err() {
                            // 被信号中断，需要回滚 writer 计数
                            // 注意：不要回滚 w_counter，它只增不减
                            let mut guard = self.inner.lock();
                            guard.writer -= 1;
                            drop(guard);
                            return Err(SystemError::EINTR);
                        }
                    }
                }

                // 非阻塞模式下也需要唤醒可能在等待写端的读者
                if is_nonblock {
                    self.open_wait_queue.wakeup_all(None);
                }
            } else {
                // 匿名管道：直接增加写端计数
                let mut guard = self.inner.lock();
                guard.w_counter += 1;
                guard.writer += 1;
                drop(guard);
            }
        } else if accflags == FileFlags::O_RDWR {
            // O_RDWR 模式：同时作为读端和写端，不阻塞
            let mut guard = self.inner.lock();
            guard.r_counter += 1; // 增加读端计数器
            guard.w_counter += 1; // 增加写端计数器
            guard.reader += 1;
            guard.writer += 1;
            guard.had_reader = true;
            drop(guard);

            // 只有 FIFO 才需要唤醒等待的进程
            if is_fifo {
                self.open_wait_queue.wakeup_all(None);
            }
        }

        return Ok(());
    }

    fn metadata(&self) -> Result<crate::filesystem::vfs::Metadata, SystemError> {
        let inode = self.inner.lock();
        return Ok(inode.metadata.clone());
    }

    fn close(&self, data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        let flags: FileFlags;
        if let FilePrivateData::Pipefs(pipe_data) = &*data {
            flags = pipe_data.flags;
        } else {
            return Err(SystemError::EBADF);
        }
        let accflags = flags.access_flags();
        let mut guard = self.inner.lock();

        // 写端关闭
        if accflags == FileFlags::O_WRONLY {
            assert!(guard.writer > 0);
            guard.writer -= 1;
            // 如果已经没有写端了，则唤醒读端
            if guard.writer == 0 {
                // 写端耗尽意味着读端应收到 POLLHUP，唤醒等待者与 epoll
                // 注意：这里需要使用读端的flags来获取POLLHUP事件
                // 因为poll()中只在!flags.is_write_only()时才设置EPOLLHUP
                let poll_flags = FileFlags::O_RDONLY;
                let poll_data = FilePrivateData::Pipefs(PipeFsPrivateData { flags: poll_flags });
                // 忽略 poll 错误：状态已更新（writer已减为0），poll失败不应导致close失败
                // 这与下面对 wakeup_epoll 错误的处理方式一致
                let pollflag = guard
                    .poll(&poll_data)
                    .map(|v| EPollEventType::from_bits_truncate(v as u32))
                    .unwrap_or(EPollEventType::EPOLLHUP);
                drop(guard); // 先释放 inner 锁，避免潜在的死锁
                self.read_wait_queue
                    .wakeup_all(Some(ProcessState::Blocked(true)));
                // 唤醒所有依赖 epoll 的等待者，确保 HUP 事件可见
                // 忽略错误：状态已更新（writer已减为0），wakeup_epoll失败不影响close操作的语义
                let _ = EventPoll::wakeup_epoll(&self.epitems, pollflag);
                return Ok(());
            }
        }

        // 读端关闭
        if accflags == FileFlags::O_RDONLY {
            assert!(guard.reader > 0);
            guard.reader -= 1;
            // 如果已经没有读端了，则唤醒写端
            if guard.reader == 0 {
                drop(guard); // 先释放 inner 锁，避免死锁
                             // 唤醒所有等待的写端（不进行状态过滤，因为进程可能已经被其他操作唤醒但还未从队列中移除）
                self.write_wait_queue.wakeup_all(None);
                return Ok(());
            }
        }

        // O_RDWR 模式关闭：同时减少读写计数
        if accflags == FileFlags::O_RDWR {
            assert!(guard.reader > 0);
            assert!(guard.writer > 0);
            guard.reader -= 1;
            guard.writer -= 1;
            let wake_reader = guard.writer == 0;
            let wake_writer = guard.reader == 0;
            drop(guard); // 先释放 inner 锁

            // 如果已经没有写端了，则唤醒读端
            if wake_reader {
                self.read_wait_queue.wakeup_all(None);
            }
            // 如果已经没有读端了，则唤醒写端
            if wake_writer {
                self.write_wait_queue.wakeup_all(None);
            }
        }

        return Ok(());
    }

    fn write_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &[u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // 获取flags
        let flags: FileFlags;
        if let FilePrivateData::Pipefs(pdata) = &*data {
            flags = pdata.flags;
        } else {
            return Err(SystemError::EBADF);
        }

        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }

        // 提前释放 data 锁，因为后续可能需要睡眠
        // 我们已经提取了需要的 mode 信息
        drop(data);

        // 加锁
        let mut inner_guard = self.inner.lock();

        if inner_guard.reader == 0 {
            if !inner_guard.had_reader {
                // 如果从未有读端，直接返回 ENXIO，无论是否阻塞模式
                return Err(SystemError::ENXIO);
            } else {
                // 如果曾经有读端，现在已关闭
                match flags.contains(FileFlags::O_NONBLOCK) {
                    true => {
                        // 非阻塞模式，直接返回 EPIPE
                        return Err(SystemError::EPIPE);
                    }
                    false => {
                        let sig = Signal::SIGPIPE;
                        let mut info = SigInfo::new(
                            sig,
                            0,
                            SigCode::Kernel,
                            SigType::Kill(ProcessManager::current_pcb().task_pid_vnr()),
                        );
                        compiler_fence(core::sync::atomic::Ordering::SeqCst);

                        let _retval = sig
                            .send_signal_info(
                                Some(&mut info),
                                ProcessManager::current_pcb().task_pid_vnr(),
                            )
                            .map(|x| x as usize);

                        compiler_fence(core::sync::atomic::Ordering::SeqCst);
                        return Err(SystemError::EPIPE);
                    }
                }
            }
        }

        // 延迟分配：如果缓冲区未分配，在第一次写入时分配
        if inner_guard.data.is_empty() {
            // 分配缓冲区大小为 buf_size
            let buf_size = inner_guard.buf_size;
            inner_guard.data = vec![0u8; buf_size];
        }

        let mut total_written: usize = 0;

        // 循环写入，直到写完所有数据
        while total_written < len {
            // 计算本次要写入的字节数
            let remaining = len - total_written;
            let buf_size = inner_guard.buf_size;
            let available_space = buf_size - inner_guard.valid_cnt as usize;

            // 如果没有可用空间，需要等待
            if available_space == 0 {
                // 唤醒读端
                self.read_wait_queue
                    .wakeup(Some(ProcessState::Blocked(true)));

                // 如果为非阻塞管道，返回已写入的字节数或 EAGAIN
                if flags.contains(FileFlags::O_NONBLOCK) {
                    drop(inner_guard);
                    if total_written > 0 {
                        return Ok(total_written);
                    }
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }

                // 解锁并睡眠
                drop(inner_guard);
                let r = wq_wait_event_interruptible!(self.write_wait_queue, self.writeable(), {});

                if r.is_err() {
                    if total_written > 0 {
                        return Ok(total_written);
                    }
                    return Err(SystemError::ERESTARTSYS);
                }
                inner_guard = self.inner.lock();

                // 检查读端是否已关闭
                if inner_guard.reader == 0 && inner_guard.had_reader {
                    drop(inner_guard);

                    // 发送 SIGPIPE 信号（阻塞模式下）
                    if !flags.contains(FileFlags::O_NONBLOCK) {
                        let sig = Signal::SIGPIPE;
                        let mut info = SigInfo::new(
                            sig,
                            0,
                            SigCode::Kernel,
                            SigType::Kill(ProcessManager::current_pcb().task_pid_vnr()),
                        );
                        compiler_fence(core::sync::atomic::Ordering::SeqCst);

                        let _ = sig.send_signal_info(
                            Some(&mut info),
                            ProcessManager::current_pcb().task_pid_vnr(),
                        );

                        compiler_fence(core::sync::atomic::Ordering::SeqCst);
                    }

                    if total_written > 0 {
                        return Ok(total_written);
                    }
                    return Err(SystemError::EPIPE);
                }

                continue;
            }

            // 计算本次写入的字节数
            let to_write = core::cmp::min(remaining, available_space);

            // 决定要输入的字节（两段复制处理 wrap 与 end==start 情况）
            let start = inner_guard.write_pos as usize;
            let first = core::cmp::min(to_write, buf_size - start);
            let second = to_write as isize - first as isize;
            // 第1段：写到缓冲尾部或写完
            inner_guard.data[start..start + first]
                .copy_from_slice(&buf[total_written..total_written + first]);
            // 第2段：如需要，从缓冲头部继续
            if second > 0 {
                inner_guard.data[0..second as usize]
                    .copy_from_slice(&buf[total_written + first..total_written + to_write]);
            }
            // 更新写位置以及valid_cnt
            inner_guard.write_pos = (inner_guard.write_pos + to_write as i32) % buf_size as i32;
            inner_guard.valid_cnt += to_write as i32;
            total_written += to_write;
        }

        // 写完后还有位置，则唤醒下一个写者
        if (inner_guard.valid_cnt as usize) < inner_guard.buf_size {
            self.write_wait_queue
                .wakeup(Some(ProcessState::Blocked(true)));
        }

        // 读完后解锁并唤醒等待在读等待队列中的进程
        self.read_wait_queue
            .wakeup(Some(ProcessState::Blocked(true)));

        // 构造用于 poll 的 FilePrivateData
        let poll_data = FilePrivateData::Pipefs(PipeFsPrivateData::new(flags));
        let pollflag = EPollEventType::from_bits_truncate(inner_guard.poll(&poll_data)? as u32);

        drop(inner_guard);
        // 唤醒epoll中等待的进程（忽略错误，因为数据已写入，这是尽力而为的通知）
        let _ = EventPoll::wakeup_epoll(&self.epitems, pollflag);

        // 返回写入的字节数
        return Ok(total_written);
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn get_entry_name_and_metadata(
        &self,
        ino: crate::filesystem::vfs::InodeId,
    ) -> Result<(alloc::string::String, crate::filesystem::vfs::Metadata), SystemError> {
        // 如果有条件，请在文件系统中使用高效的方式实现本接口，而不是依赖这个低效率的默认实现。
        let name = self.get_entry_name(ino)?;
        let entry = self.find(&name)?;
        return Ok((name, entry.metadata()?));
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        PipeFS::instance()
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        Ok(self)
    }

    fn absolute_path(&self) -> Result<String, SystemError> {
        Ok(String::from("pipe"))
    }

    fn ioctl(
        &self,
        cmd: u32,
        data: usize,
        _private_data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        match cmd {
            FIONREAD => {
                let inner = self.inner.lock();
                let available = inner.valid_cnt;
                drop(inner);

                let mut writer =
                    UserBufferWriter::new(data as *mut u8, core::mem::size_of::<i32>(), true)?;
                writer
                    .buffer_protected(0)?
                    .write_one::<i32>(0, &available)?;
                Ok(0)
            }
            _ => Err(SystemError::ENOSYS),
        }
    }
}
