use crate::filesystem::vfs::syscall::SpliceFlags;
use crate::libs::mutex::MutexGuard;
use crate::{
    arch::{ipc::signal::Signal, MMArch},
    filesystem::{
        epoll::{
            event_poll::{EventPoll, LockedEPItemLinkedList},
            EPollEventType, EPollItem,
        },
        vfs::{
            fasync::{FAsyncItem, FAsyncItems, FASYNC_POLL_IN, FASYNC_POLL_OUT},
            file::FileFlags,
            vcore::generate_inode_id,
            FilePrivateData, FileSystem, FileType, FsInfo, IndexNode, InodeFlags, InodeMode, Magic,
            Metadata, PollableInode, SuperBlock,
        },
    },
    ipc::signal::send_kernel_signal_to_current,
    libs::{spinlock::SpinLock, wait_queue::WaitQueue},
    mm::MemoryManagementArch,
    process::ProcessState,
    syscall::user_access::UserBufferWriter,
    time::PosixTimeSpec,
};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;

use alloc::sync::{Arc, Weak};
use system_error::SystemError;

/// 管道缓冲区默认大小（Linux 默认 65536 字节）
pub const PIPE_BUFF_SIZE: usize = 65536;

/// 管道缓冲区最小大小（一页大小，Linux 保证原子写入的最小单位）
pub const PIPE_MIN_SIZE: usize = 4096;

/// PIPE_BUF: writes of <= PIPE_BUF must be atomic.
/// Linux guarantees PIPE_BUF is at least a page (4096).
pub const PIPE_BUF: usize = PIPE_MIN_SIZE;

/// 管道缓冲区最大大小（Linux 默认为 1MB）
pub const PIPE_MAX_SIZE: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum WakeMode {
    #[default]
    None,
    One,
    All,
}

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
    read_fasync_items: FAsyncItems,
    write_fasync_items: FAsyncItems,
}

/// @brief 管道文件i节点(无锁)
#[derive(Debug)]
pub struct InnerPipeInode {
    self_ref: Weak<LockedPipeInode>,
    /// 管道内可读的数据数
    valid_cnt: i32,
    read_pos: i32,
    write_pos: i32,
    splice_hold: usize,
    /// 管道缓冲区数据（使用 Vec 支持动态大小）
    data: Vec<u8>,
    /// 当前缓冲区大小
    buf_size: usize,
    /// INode 元数据
    metadata: Metadata,
    reader: u32,
    writer: u32,
    /// Writers that have committed to sleeping for pipe space.
    ///
    /// This is protected by `LockedPipeInode::inner` and is used only to
    /// choose between an exclusive wakeup and a broadcast after space grows.
    write_wait_intents: usize,
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
            if self.valid_cnt != 0 && self.splice_hold == 0 {
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

    #[inline]
    fn poll_both_ends(&self) -> EPollEventType {
        // poll() 的返回与 FileFlags 的读/写端有关。
        // 为了正确唤醒同时监听读端/写端的 epoll，需要合并两侧视角的事件。
        let read_data = FilePrivateData::Pipefs(PipeFsPrivateData {
            flags: FileFlags::O_RDONLY,
        });
        let write_data = FilePrivateData::Pipefs(PipeFsPrivateData {
            flags: FileFlags::O_WRONLY,
        });

        let read_mask = self.poll(&read_data).unwrap_or(0);
        let write_mask = self.poll(&write_data).unwrap_or(0);
        EPollEventType::from_bits_truncate((read_mask | write_mask) as u32)
    }

    fn buf_full(&self) -> bool {
        return self.valid_cnt as usize == self.buf_size;
    }

    /// 从另一个管道复制数据到本管道（不消耗源数据）
    /// src: 源管道
    /// count: 要复制的字节数
    /// skip: 源管道读取偏移量
    pub fn copy_from_other(&mut self, src: &InnerPipeInode, count: usize, skip: usize) -> usize {
        if count == 0 {
            return 0;
        }

        // 计算实际可读/可写量
        let src_avail = src.valid_cnt.max(0) as usize;
        if skip >= src_avail {
            return 0;
        }
        let src_real_avail = src_avail - skip;

        let dst_space = self.buf_size.saturating_sub(self.valid_cnt.max(0) as usize);
        let actual_copy = count.min(src_real_avail).min(dst_space);

        if actual_copy == 0 {
            return 0;
        }

        // 确保目标缓冲区已分配
        if self.data.is_empty() {
            self.data = vec![0u8; self.buf_size];
        }

        let src_buf_size = src.buf_size;
        let dst_buf_size = self.buf_size;

        let src_start = (src.read_pos as usize + skip) % src_buf_size;
        let dst_start = self.write_pos as usize;

        // 由于是环形缓冲区，源和目标都可能分为两段
        // 最坏情况是 2段 -> 2段，共4次拷贝。
        // 为简化逻辑，我们可以先获取源数据的切片（1或2个），然后写入目标。

        let mut src_slices = [
            &src.data[..0], // placeholder
            &src.data[..0],
        ];
        let mut slice_count = 0;

        let src_first_len = actual_copy.min(src_buf_size - src_start);
        src_slices[0] = &src.data[src_start..src_start + src_first_len];
        slice_count += 1;

        let src_second_len = actual_copy - src_first_len;
        if src_second_len > 0 {
            src_slices[1] = &src.data[0..src_second_len];
            slice_count += 1;
        }

        // 现在将这些切片写入目标
        let mut current_dst_pos = dst_start;
        for slice in src_slices.iter().take(slice_count) {
            let slice = *slice;
            let slice_len = slice.len();
            if slice_len == 0 {
                continue;
            }

            let dst_first_len = slice_len.min(dst_buf_size - current_dst_pos);
            self.data[current_dst_pos..current_dst_pos + dst_first_len]
                .copy_from_slice(&slice[0..dst_first_len]);

            let dst_second_len = slice_len - dst_first_len;
            if dst_second_len > 0 {
                self.data[0..dst_second_len].copy_from_slice(&slice[dst_first_len..]);
            }

            current_dst_pos = (current_dst_pos + slice_len) % dst_buf_size;
        }

        // 更新目标写指针
        self.write_pos = (self.write_pos + actual_copy as i32) % dst_buf_size as i32;
        self.valid_cnt += actual_copy as i32;

        actual_copy
    }
}

impl LockedPipeInode {
    #[inline]
    fn writer_wake_mode(inner: &InnerPipeInode) -> WakeMode {
        match inner.write_wait_intents {
            0 => WakeMode::None,
            1 => WakeMode::One,
            _ => WakeMode::All,
        }
    }

    #[inline]
    fn wake_waiters(queue: &WaitQueue, mode: WakeMode) {
        match mode {
            WakeMode::None => {}
            WakeMode::One => {
                queue.wakeup(Some(ProcessState::Blocked(true)));
            }
            WakeMode::All => {
                queue.wakeup_all(Some(ProcessState::Blocked(true)));
            }
        }
    }

    #[inline]
    fn wake_pipe_waiters(&self, readers: WakeMode, writers: WakeMode) {
        Self::wake_waiters(&self.read_wait_queue, readers);
        Self::wake_waiters(&self.write_wait_queue, writers);
    }

    fn send_sigpipe() {
        if let Err(err) = send_kernel_signal_to_current(Signal::SIGPIPE) {
            log::error!("Failed to send SIGPIPE for pipe write: {:?}", err);
        }
    }

    fn publish_write_progress(&self, reader_wake: WakeMode, pollflag: EPollEventType) {
        Self::wake_waiters(&self.read_wait_queue, reader_wake);
        let _ = EventPoll::wakeup_epoll(&self.epitems, pollflag);
        self.read_fasync_items.send_sigio(FASYNC_POLL_IN);
    }

    /// 安全地锁定两个管道节点（避免死锁）
    /// 返回两个节点的锁保护对象
    /// 注意：p1 和 p2 必须不同，否则会发生死锁（如果尝试对同一个锁加锁两次）或返回不安全的别名引用
    fn lock_two<'a, 'b>(
        p1: &'a LockedPipeInode,
        p2: &'b LockedPipeInode,
    ) -> (
        crate::libs::spinlock::SpinLockGuard<'a, InnerPipeInode>,
        crate::libs::spinlock::SpinLockGuard<'b, InnerPipeInode>,
    ) {
        let addr1 = p1 as *const _ as usize;
        let addr2 = p2 as *const _ as usize;

        if addr1 < addr2 {
            (p1.inner.lock(), p2.inner.lock())
        } else {
            let g2 = p2.inner.lock();
            let g1 = p1.inner.lock();
            (g1, g2)
        }
    }

    fn tee_adjusted_skip(
        start_read_pos: usize,
        cur_read_pos: usize,
        buf_size: usize,
        snapshot: usize,
        total: usize,
    ) -> usize {
        let consumed_since_start = if cur_read_pos >= start_read_pos {
            cur_read_pos - start_read_pos
        } else {
            buf_size - start_read_pos + cur_read_pos
        }
        .min(snapshot);
        total.saturating_sub(consumed_since_start)
    }

    pub fn new() -> Arc<Self> {
        let inner = InnerPipeInode {
            self_ref: Weak::default(),
            valid_cnt: 0,
            read_pos: 0,
            write_pos: 0,
            splice_hold: 0,
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
            write_wait_intents: 0,
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
            read_fasync_items: FAsyncItems::default(),
            write_fasync_items: FAsyncItems::default(),
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
        if inode.valid_cnt == 0 {
            return inode.writer == 0;
        }
        inode.splice_hold == 0
    }

    fn writeable(&self) -> bool {
        let inode = self.inner.lock();
        return !inode.buf_full() || inode.reader == 0;
    }

    /// Whether the pipe has at least `need` bytes of free space.
    /// Used to implement PIPE_BUF atomic write semantics.
    fn writeable_len_at_least(&self, need: usize) -> bool {
        let inode = self.inner.lock();
        if inode.reader == 0 {
            return true;
        }
        let used = inode.valid_cnt.max(0) as usize;
        inode.buf_size.saturating_sub(used) >= need
    }

    /// Sleep for pipe space after publishing the writer's intent under the
    /// pipe lock. Publishing before dropping the lock closes the gap where a
    /// reader could create space without observing the waiter.
    fn wait_for_write_space<'a, F>(
        &'a self,
        mut guard: crate::libs::spinlock::SpinLockGuard<'a, InnerPipeInode>,
        required: Option<usize>,
        before_wait: F,
    ) -> (
        crate::libs::spinlock::SpinLockGuard<'a, InnerPipeInode>,
        Result<(), SystemError>,
    )
    where
        F: FnOnce(),
    {
        guard.write_wait_intents = guard
            .write_wait_intents
            .checked_add(1)
            .expect("pipe writer wait-intent count overflowed");
        drop(guard);

        before_wait();
        let result = if let Some(need) = required {
            wq_wait_event_interruptible!(
                self.write_wait_queue,
                self.writeable_len_at_least(need),
                {}
            )
        } else {
            wq_wait_event_interruptible!(self.write_wait_queue, self.writeable(), {})
        };

        let mut guard = self.inner.lock();
        guard.write_wait_intents = guard
            .write_wait_intents
            .checked_sub(1)
            .expect("pipe writer wait-intent count underflowed");
        let result = result.map_err(|_| SystemError::ERESTARTSYS);
        (guard, result)
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

        let writer_wake = Self::writer_wake_mode(&inner);
        let pollflag = inner.poll_both_ends();
        drop(inner);

        Self::wake_waiters(&self.write_wait_queue, writer_wake);
        let _ = EventPoll::wakeup_epoll(&self.epitems, pollflag);

        Ok(new_size)
    }

    /// 获取管道缓冲区大小
    pub fn get_pipe_size(&self) -> usize {
        self.inner.lock().buf_size
    }

    /// 当前管道中可读的字节数（不阻塞、不睡眠）
    pub fn readable_len(&self) -> usize {
        let guard = self.inner.lock();
        if guard.splice_hold > 0 {
            return 0;
        }
        guard.valid_cnt.max(0) as usize
    }

    /// 当前管道中可写的空闲字节数（不阻塞、不睡眠）
    pub fn writable_len(&self) -> usize {
        let guard = self.inner.lock();
        let used = guard.valid_cnt.max(0) as usize;
        guard.buf_size.saturating_sub(used)
    }

    fn write_bytes(inner_guard: &mut InnerPipeInode, buf: &[u8], to_write: usize) {
        let buf_size = inner_guard.buf_size;
        let start = inner_guard.write_pos as usize;
        let first = core::cmp::min(to_write, buf_size - start);
        let second = to_write - first;
        inner_guard.data[start..start + first].copy_from_slice(&buf[..first]);
        if second > 0 {
            inner_guard.data[0..second].copy_from_slice(&buf[first..to_write]);
        }
        inner_guard.write_pos = (inner_guard.write_pos + to_write as i32) % buf_size as i32;
        inner_guard.valid_cnt += to_write as i32;
    }

    /// Nonblocking write helper for splice(2) paths that must ignore the pipe FD's O_NONBLOCK flag.
    /// This never sleeps; it returns EAGAIN when no space is available.
    pub fn write_from_splice_nonblock(&self, buf: &[u8]) -> Result<usize, SystemError> {
        let len = buf.len();
        if len == 0 {
            return Ok(0);
        }

        let mut inner_guard = self.inner.lock();

        if inner_guard.reader == 0 {
            drop(inner_guard);
            Self::send_sigpipe();
            return Err(SystemError::EPIPE);
        }

        if inner_guard.data.is_empty() {
            let buf_size = inner_guard.buf_size;
            inner_guard.data = vec![0u8; buf_size];
        }

        let buf_size = inner_guard.buf_size;
        let available = buf_size.saturating_sub(inner_guard.valid_cnt.max(0) as usize);
        let atomic_write = len <= PIPE_BUF;

        if atomic_write && available < len {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        if available == 0 {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        let to_write = if atomic_write {
            len
        } else {
            len.min(available)
        };

        let was_readable = inner_guard.valid_cnt > 0 && inner_guard.splice_hold == 0;
        Self::write_bytes(&mut inner_guard, buf, to_write);
        let reader_wake = if !was_readable && inner_guard.splice_hold == 0 {
            WakeMode::One
        } else {
            WakeMode::None
        };

        let pollflag = inner_guard.poll_both_ends();
        drop(inner_guard);
        Self::wake_waiters(&self.read_wait_queue, reader_wake);
        let _ = EventPoll::wakeup_epoll(&self.epitems, pollflag);
        self.read_fasync_items.send_sigio(FASYNC_POLL_IN);

        Ok(to_write)
    }

    /// Wait for pipe space before file->pipe splice reads from the input file.
    ///
    /// The caller must pass the maximum number of bytes it can actually read
    /// from the input file for this splice attempt. DragonOS pipes are byte-ring
    /// based, so requests up to PIPE_BUF wait for the complete readable chunk;
    /// larger requests wait for any space and may complete partially.
    pub fn wait_writable_for_splice(&self, len: usize) -> Result<usize, SystemError> {
        if len == 0 {
            return Ok(0);
        }

        let need_atomic = len <= PIPE_BUF;
        let mut guard = self.inner.lock();
        loop {
            if guard.reader == 0 {
                drop(guard);
                Self::send_sigpipe();
                return Err(SystemError::EPIPE);
            }

            let used = guard.valid_cnt.max(0) as usize;
            let space = guard.buf_size.saturating_sub(used);
            if (need_atomic && space >= len) || (!need_atomic && space > 0) {
                return Ok(if need_atomic { len } else { len.min(space) });
            }

            let required = need_atomic.then_some(len);
            let (next_guard, wait_result) = self.wait_for_write_space(guard, required, || {});
            guard = next_guard;
            wait_result?;
        }
    }

    /// Wait until the pipe has any writable byte for file->pipe splice.
    ///
    /// This matches Linux `wait_for_space()` for inputs whose exact readable
    /// length is not known before calling into the file. The caller can then
    /// cap the read by the returned byte space.
    pub fn wait_writable_any_for_splice(&self) -> Result<usize, SystemError> {
        let mut guard = self.inner.lock();
        loop {
            if guard.reader == 0 {
                drop(guard);
                Self::send_sigpipe();
                return Err(SystemError::EPIPE);
            }

            let used = guard.valid_cnt.max(0) as usize;
            let space = guard.buf_size.saturating_sub(used);
            if space > 0 {
                return Ok(space);
            }

            let (next_guard, wait_result) = self.wait_for_write_space(guard, None, || {});
            guard = next_guard;
            wait_result?;
        }
    }

    /// 从管道中“窥视”最多 `len` 字节数据到 `buf`，但不消耗管道数据。
    ///
    /// 返回实际拷贝的字节数（可能小于 `len`）。不会睡眠。
    pub fn peek_into(&self, len: usize, buf: &mut [u8]) -> usize {
        self.peek_into_from(0, len, buf)
    }

    /// 从管道中“窥视”从当前 read_pos 起偏移 `skip` 字节后的内容（不消耗）。
    ///
    /// `skip` 必须小于等于当前可读字节数（否则返回 0）。不会睡眠。
    pub fn peek_into_from(&self, skip: usize, len: usize, buf: &mut [u8]) -> usize {
        if len == 0 {
            return 0;
        }
        let guard = self.inner.lock();
        if guard.valid_cnt <= 0 {
            return 0;
        }
        if guard.data.is_empty() {
            return 0;
        }

        let available = guard.valid_cnt as usize;
        if skip > available {
            return 0;
        }
        if skip == available {
            return 0;
        }

        let available = available - skip;
        let num = core::cmp::min(len, available).min(buf.len());
        let buf_size = guard.buf_size;
        let start = (guard.read_pos as usize + skip) % buf_size;

        let first = core::cmp::min(num, buf_size - start);
        let second = num.saturating_sub(first);
        buf[0..first].copy_from_slice(&guard.data[start..start + first]);
        if second > 0 {
            buf[first..num].copy_from_slice(&guard.data[0..second]);
        }
        num
    }

    pub(crate) fn splice_peek_hold_from_blocking(
        &self,
        len: usize,
        buf: &mut [u8],
        nonblock: bool,
    ) -> Result<usize, SystemError> {
        let mut did_wait = false;
        loop {
            let mut guard = self.inner.lock();
            if guard.valid_cnt == 0 {
                if guard.writer == 0 {
                    return Ok(0);
                }
                if nonblock {
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                drop(guard);
                wq_wait_event_interruptible!(self.read_wait_queue, self.readable(), {})?;
                did_wait = true;
                continue;
            }

            if guard.splice_hold > 0 {
                if nonblock {
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                drop(guard);
                wq_wait_event_interruptible!(self.read_wait_queue, self.readable(), {})?;
                did_wait = true;
                continue;
            }

            let mut num = guard.valid_cnt as usize;
            if len < num {
                num = len;
            }
            if buf.len() < num {
                let pass_baton = did_wait && guard.splice_hold == 0;
                drop(guard);
                if pass_baton {
                    Self::wake_waiters(&self.read_wait_queue, WakeMode::One);
                }
                return Err(SystemError::EINVAL);
            }

            let start = guard.read_pos as usize;
            let buf_size = guard.buf_size;
            let first = core::cmp::min(num, buf_size - start);
            let second = num.saturating_sub(first);
            buf[0..first].copy_from_slice(&guard.data[start..start + first]);
            if second > 0 {
                buf[first..num].copy_from_slice(&guard.data[0..second]);
            }

            guard.splice_hold = num;
            return Ok(num);
        }
    }

    pub(crate) fn splice_finish_hold(&self, consumed: usize) {
        let mut guard = self.inner.lock();
        let held = guard.splice_hold;
        if held == 0 {
            return;
        }

        let consume = consumed.min(held);
        if consume > 0 {
            let buf_size = guard.buf_size;
            guard.read_pos = (guard.read_pos + consume as i32) % buf_size as i32;
            guard.valid_cnt -= consume as i32;
        }
        guard.splice_hold = 0;
        let reader_wake = if guard.valid_cnt > 0 {
            WakeMode::One
        } else if guard.writer == 0 {
            WakeMode::All
        } else {
            WakeMode::None
        };
        let writer_wake = if consume > 0 {
            Self::writer_wake_mode(&guard)
        } else {
            WakeMode::None
        };
        let pollflag = guard.poll_both_ends();
        drop(guard);
        self.wake_pipe_waiters(reader_wake, writer_wake);
        let _ = EventPoll::wakeup_epoll(&self.epitems, pollflag);
        if reader_wake != WakeMode::None {
            self.read_fasync_items.send_sigio(FASYNC_POLL_IN);
        }
        if consume > 0 {
            self.write_fasync_items.send_sigio(FASYNC_POLL_OUT);
        }
    }

    /// Helper: Wait until the pipe is readable (has data).
    /// Returns:
    /// - Ok(true): Data is available.
    /// - Ok(false): EOF (no writers and no data).
    /// - Err(e): Interrupted or EAGAIN.
    fn wait_readable(&self, nonblock: bool) -> Result<bool, SystemError> {
        let mut did_wait = false;
        loop {
            let (avail, has_writer, held) = {
                let guard = self.inner.lock();
                (
                    guard.valid_cnt.max(0) as usize,
                    guard.writer > 0,
                    guard.splice_hold > 0,
                )
            };

            if avail > 0 && !held {
                if did_wait {
                    Self::wake_waiters(&self.read_wait_queue, WakeMode::One);
                }
                return Ok(true);
            }
            if avail == 0 && !has_writer {
                if did_wait {
                    Self::wake_waiters(&self.read_wait_queue, WakeMode::All);
                }
                return Ok(false);
            }
            if nonblock {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            wq_wait_event_interruptible!(self.read_wait_queue, self.readable(), {})?;
            did_wait = true;
        }
    }

    /// Helper: Wait until the pipe is writable (has space).
    /// Returns:
    /// - Ok(()): Space is available.
    /// - Err(e): Interrupted, EAGAIN, or EPIPE (no readers).
    fn wait_writable(&self, nonblock: bool) -> Result<(), SystemError> {
        let mut guard = self.inner.lock();
        loop {
            let space = guard
                .buf_size
                .saturating_sub(guard.valid_cnt.max(0) as usize);
            if space > 0 {
                return Ok(());
            }
            if guard.reader == 0 {
                drop(guard);
                Self::send_sigpipe();
                return Err(SystemError::EPIPE);
            }
            if nonblock {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            let (next_guard, wait_result) = self.wait_for_write_space(guard, None, || {});
            guard = next_guard;
            wait_result?;
        }
    }

    /// Helper: Transfer a chunk of data from `src` to `dst`.
    /// - `len`: Max bytes to transfer.
    /// - `consume`: Whether to consume data from `src` (splice) or just peek (tee).
    /// - `skip_calculator`: Closure to calculate skip amount based on `src` state (used for tee).
    ///
    /// Returns actual copied bytes. 0 means retry (buffers full/empty temporarily).
    fn transfer_chunk<F>(
        src: &LockedPipeInode,
        dst: &LockedPipeInode,
        len: usize,
        consume: bool,
        skip_calculator: F,
    ) -> Result<usize, SystemError>
    where
        F: FnOnce(&InnerPipeInode) -> usize,
    {
        // Lock both pipes
        let (mut in_guard, mut out_guard) = Self::lock_two(src, dst);

        // Re-check conditions under lock
        let in_avail = in_guard.valid_cnt.max(0) as usize;
        let out_space = out_guard
            .buf_size
            .saturating_sub(out_guard.valid_cnt.max(0) as usize);

        if out_guard.reader == 0 {
            drop(in_guard);
            drop(out_guard);
            Self::send_sigpipe();
            return Err(SystemError::EPIPE);
        }

        if in_avail == 0 || in_guard.splice_hold > 0 || out_space == 0 {
            return Ok(0);
        }

        let skip = skip_calculator(&in_guard);
        // Ensure skip doesn't exceed available data
        if skip >= in_avail {
            // Nothing to copy due to skip
            return Ok(0);
        }
        let real_in_avail = in_avail - skip;

        let want = len.min(4096);
        let chunk = core::cmp::min(want, core::cmp::min(real_in_avail, out_space));

        if chunk == 0 {
            return Ok(0);
        }

        let out_was_readable = out_guard.valid_cnt > 0 && out_guard.splice_hold == 0;

        // Copy data directly
        let copied = out_guard.copy_from_other(&in_guard, chunk, skip);

        if consume {
            let buf_size = in_guard.buf_size;
            in_guard.read_pos = (in_guard.read_pos + copied as i32) % buf_size as i32;
            in_guard.valid_cnt -= copied as i32;
        }

        let src_reader_wake = if consume && in_guard.valid_cnt == 0 && in_guard.writer == 0 {
            WakeMode::All
        } else {
            WakeMode::None
        };
        let src_writer_wake = if consume {
            Self::writer_wake_mode(&in_guard)
        } else {
            WakeMode::None
        };
        let dst_reader_wake = if !out_was_readable && out_guard.splice_hold == 0 {
            WakeMode::One
        } else {
            WakeMode::None
        };

        let in_poll = consume.then(|| in_guard.poll_both_ends());
        let out_poll = out_guard.poll_both_ends();
        drop(in_guard);
        drop(out_guard);

        src.wake_pipe_waiters(src_reader_wake, src_writer_wake);
        Self::wake_waiters(&dst.read_wait_queue, dst_reader_wake);
        if let Some(in_poll) = in_poll {
            let _ = EventPoll::wakeup_epoll(&src.epitems, in_poll);
        }
        let _ = EventPoll::wakeup_epoll(&dst.epitems, out_poll);
        if consume {
            src.write_fasync_items.send_sigio(FASYNC_POLL_OUT);
        }
        dst.read_fasync_items.send_sigio(FASYNC_POLL_IN);

        Ok(copied)
    }

    /// splice(2): 将本管道中的数据移动到目标管道（消耗输入数据）。
    ///
    /// 语义对齐 Linux fs/splice.c: splice_pipe_to_pipe()/wait_for_space()/ipipe_prep/opipe_prep。
    pub fn splice_to_pipe(
        &self,
        out: &LockedPipeInode,
        len: usize,
        flags: SpliceFlags,
    ) -> Result<usize, SystemError> {
        if len == 0 {
            return Ok(0);
        }
        if core::ptr::eq(self, out) {
            return Err(SystemError::EINVAL);
        }
        let nonblock = flags.contains(SpliceFlags::SPLICE_F_NONBLOCK);

        loop {
            // Wait for input data
            if !self.wait_readable(nonblock)? {
                return Ok(0); // EOF
            }

            // Wait for output space
            out.wait_writable(nonblock)?;

            // Try transfer
            let copied = Self::transfer_chunk(self, out, len, true, |_| 0)?;
            if copied > 0 {
                return Ok(copied);
            }
        }
    }

    /// tee(2): 将本管道中的数据复制到目标管道，但不消耗本管道数据。
    ///
    /// 参考 Linux 语义：当 input 为空且仍有 writer 时，阻塞或返回 EAGAIN；
    /// 当 output 满且仍有 reader 时，阻塞或返回 EAGAIN。
    pub fn tee_to(
        &self,
        out: &LockedPipeInode,
        len: usize,
        flags: crate::filesystem::vfs::syscall::SpliceFlags,
    ) -> Result<usize, SystemError> {
        if len == 0 {
            return Ok(0);
        }
        if core::ptr::eq(self, out) {
            return Err(SystemError::EINVAL);
        }
        let nonblock =
            flags.contains(crate::filesystem::vfs::syscall::SpliceFlags::SPLICE_F_NONBLOCK);

        let mut total: usize = 0;
        let mut in_avail_snapshot: Option<usize> = None;
        let mut in_read_pos_snapshot: Option<usize> = None;
        let mut in_buf_size_snapshot: Option<usize> = None;

        while total < len {
            if let Some(snapshot) = in_avail_snapshot {
                if total >= snapshot {
                    return Ok(total);
                }
            } else {
                // Check if readable
                if !self.wait_readable(nonblock)? {
                    // EOF
                    return Ok(total);
                }
            }

            // Check output space
            // tee has special nonblock handling: return total if > 0
            let (out_space, out_has_readers) = {
                let guard = out.inner.lock();
                let used = guard.valid_cnt.max(0) as usize;
                (guard.buf_size.saturating_sub(used), guard.reader > 0)
            };
            if !out_has_readers {
                Self::send_sigpipe();
                return if total > 0 {
                    Ok(total)
                } else {
                    Err(SystemError::EPIPE)
                };
            }
            if out_space == 0 {
                if nonblock || total > 0 {
                    if total > 0 {
                        return Ok(total);
                    }
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                // force blocking wait
                out.wait_writable(false)?;
            }

            let copied = match Self::transfer_chunk(self, out, len - total, false, |in_guard| {
                if in_avail_snapshot.is_none() {
                    in_avail_snapshot = Some(in_guard.valid_cnt.max(0) as usize);
                    in_read_pos_snapshot = Some(in_guard.read_pos.max(0) as usize);
                    in_buf_size_snapshot = Some(in_guard.buf_size);
                }

                let snapshot = in_avail_snapshot.unwrap();
                if total >= snapshot {
                    return usize::MAX; // Signal to skip everything
                }

                let mut skip = total;
                if let (Some(start_read_pos), Some(start_buf_size)) =
                    (in_read_pos_snapshot, in_buf_size_snapshot)
                {
                    if start_buf_size == in_guard.buf_size {
                        let cur_read_pos = in_guard.read_pos.max(0) as usize;
                        skip = Self::tee_adjusted_skip(
                            start_read_pos,
                            cur_read_pos,
                            start_buf_size,
                            snapshot,
                            total,
                        );
                    }
                }
                skip
            }) {
                Ok(copied) => copied,
                Err(SystemError::EPIPE) if total > 0 => return Ok(total),
                Err(err) => return Err(err),
            };

            if copied == 0 {
                // Snapshot may be stale if another reader drained the pipe; refresh on next loop.
                in_avail_snapshot = None;
                in_read_pos_snapshot = None;
                in_buf_size_snapshot = None;
                if nonblock && total > 0 {
                    return Ok(total);
                }
                continue;
            }

            total += copied;
        }

        Ok(total)
    }

    /// 是否存在写端（用于判断空管道时返回 EOF 还是 EAGAIN）
    pub fn has_writers(&self) -> bool {
        self.inner.lock().writer > 0
    }

    /// 是否存在读端（用于判断满管道时返回 EPIPE 还是 EAGAIN）
    pub fn has_readers(&self) -> bool {
        self.inner.lock().reader > 0
    }
}

#[cfg(test)]
mod tests {
    use super::{LockedPipeInode, PIPE_MIN_SIZE};

    #[test]
    fn tee_adjusted_skip_no_consumption() {
        assert_eq!(LockedPipeInode::tee_adjusted_skip(0, 0, 100, 80, 50), 50);
    }

    #[test]
    fn tee_adjusted_skip_with_consumption() {
        assert_eq!(LockedPipeInode::tee_adjusted_skip(0, 30, 100, 80, 50), 20);
    }

    #[test]
    fn tee_adjusted_skip_consumption_exceeds_total() {
        assert_eq!(LockedPipeInode::tee_adjusted_skip(0, 80, 100, 80, 50), 0);
    }

    #[test]
    fn tee_adjusted_skip_wraparound() {
        assert_eq!(LockedPipeInode::tee_adjusted_skip(90, 10, 100, 80, 50), 30);
    }

    #[test]
    fn transfer_does_not_consume_splice_held_data() {
        let src = LockedPipeInode::new();
        let dst = LockedPipeInode::new();
        {
            let mut src_guard = src.inner.lock();
            src_guard.reader = 1;
            src_guard.writer = 1;
            src_guard.data = alloc::vec![0u8; PIPE_MIN_SIZE];
            src_guard.data[..4].copy_from_slice(b"held");
            src_guard.valid_cnt = 4;
            src_guard.write_pos = 4;
        }
        {
            let mut dst_guard = dst.inner.lock();
            dst_guard.reader = 1;
            dst_guard.writer = 1;
        }

        let mut held = [0u8; 4];
        assert_eq!(
            src.splice_peek_hold_from_blocking(held.len(), &mut held, true),
            Ok(4)
        );
        assert_eq!(&held, b"held");
        assert_eq!(
            LockedPipeInode::transfer_chunk(&src, &dst, 4, true, |_| 0),
            Ok(0)
        );
        {
            let src_guard = src.inner.lock();
            assert_eq!(src_guard.valid_cnt, 4);
            assert_eq!(src_guard.read_pos, 0);
        }

        src.splice_finish_hold(0);
        assert_eq!(
            LockedPipeInode::transfer_chunk(&src, &dst, 4, true, |_| 0),
            Ok(4)
        );
        assert_eq!(src.inner.lock().valid_cnt, 0);
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

    fn add_fasync(
        &self,
        fasync_item: FAsyncItem,
        private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        let FilePrivateData::Pipefs(pipe_data) = private_data else {
            return Err(SystemError::EBADF);
        };

        let flags = pipe_data.flags;
        if !flags.is_write_only() {
            self.read_fasync_items.add(fasync_item.clone());
        }
        if !flags.is_read_only() {
            self.write_fasync_items.add(fasync_item);
        }
        Ok(())
    }

    fn remove_fasync(
        &self,
        file: &Weak<crate::filesystem::vfs::file::File>,
        private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        if let FilePrivateData::Pipefs(pipe_data) = private_data {
            let flags = pipe_data.flags;
            if !flags.is_write_only() {
                self.read_fasync_items.remove(file);
            }
            if !flags.is_read_only() {
                self.write_fasync_items.remove(file);
            }
        } else {
            self.read_fasync_items.remove(file);
            self.write_fasync_items.remove(file);
        }
        Ok(())
    }
}

impl IndexNode for LockedPipeInode {
    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        data_guard: MutexGuard<FilePrivateData>,
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
        if len == 0 {
            return Ok(0);
        }
        // log::debug!("pipe flags: {:?}", flags);
        // 加锁
        let mut inner_guard = self.inner.lock();
        let mut did_wait = false;

        while inner_guard.valid_cnt == 0 || inner_guard.splice_hold > 0 {
            if inner_guard.valid_cnt == 0 && inner_guard.writer == 0 {
                return Ok(0);
            }

            if flags.contains(FileFlags::O_NONBLOCK) {
                drop(inner_guard);
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }

            drop(inner_guard);
            wq_wait_event_interruptible!(self.read_wait_queue, self.readable(), {})?;
            did_wait = true;

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

        let reader_wake = if inner_guard.valid_cnt == 0 && inner_guard.writer == 0 {
            WakeMode::All
        } else if did_wait && inner_guard.valid_cnt > 0 && inner_guard.splice_hold == 0 {
            WakeMode::One
        } else {
            WakeMode::None
        };
        let writer_wake = Self::writer_wake_mode(&inner_guard);
        let pollflag = inner_guard.poll_both_ends();
        drop(inner_guard);
        self.wake_pipe_waiters(reader_wake, writer_wake);
        // 唤醒epoll中等待的进程（忽略错误，因为状态已更新，这是尽力而为的通知）
        let _ = EventPoll::wakeup_epoll(&self.epitems, pollflag);
        self.write_fasync_items.send_sigio(FASYNC_POLL_OUT);

        //返回读取的字节数
        return Ok(num);
    }

    fn open(
        &self,
        mut data: MutexGuard<FilePrivateData>,
        flags: &crate::filesystem::vfs::file::FileFlags,
    ) -> Result<(), SystemError> {
        // O_PATH: 只获取文件描述符，不实际打开文件进行读写操作
        // 参考 Linux fs/open.c: do_dentry_open() 对 O_PATH 的处理
        // O_PATH 打开不应触发 FIFO 的阻塞等待语义
        if flags.contains(FileFlags::O_PATH) {
            log::debug!("pipe.rs: O_PATH detected, returning early");
            *data = FilePrivateData::Pipefs(PipeFsPrivateData { flags: *flags });
            return Ok(());
        }

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

    fn close(&self, data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        let flags: FileFlags;
        if let FilePrivateData::Pipefs(pipe_data) = &*data {
            flags = pipe_data.flags;
        } else {
            return Err(SystemError::EBADF);
        }
        let accflags = flags.access_flags();

        // O_PATH: 只获取文件描述符，不需要reader/writer计数
        // 参考 Linux 对 O_PATH 的处理，close() 不应影响管道的读写端计数
        if flags.contains(FileFlags::O_PATH) {
            return Ok(());
        }

        let mut guard = self.inner.lock();
        match accflags {
            FileFlags::O_RDONLY => {
                assert!(guard.reader > 0);
                guard.reader -= 1;
            }
            FileFlags::O_WRONLY => {
                assert!(guard.writer > 0);
                guard.writer -= 1;
            }
            FileFlags::O_RDWR => {
                assert!(guard.reader > 0);
                assert!(guard.writer > 0);
                guard.reader -= 1;
                guard.writer -= 1;
            }
            _ => {}
        }

        // Linux pipe_release() wakes both wait queues and notifies both fasync
        // sides only when close leaves exactly one endpoint class present.
        let release_notify = (guard.reader == 0) != (guard.writer == 0);
        let pollflag = if release_notify {
            guard.poll_both_ends()
        } else {
            EPollEventType::empty()
        };
        drop(guard);

        if release_notify {
            self.read_wait_queue.wakeup_all(None);
            self.write_wait_queue.wakeup_all(None);
            let _ = EventPoll::wakeup_epoll(&self.epitems, pollflag);
            self.read_fasync_items.send_sigio(FASYNC_POLL_IN);
            self.write_fasync_items.send_sigio(FASYNC_POLL_OUT);
        }

        return Ok(());
    }

    fn write_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let flags: FileFlags;
        if let FilePrivateData::Pipefs(pdata) = &*data {
            flags = pdata.flags;
        } else {
            return Err(SystemError::EBADF);
        }

        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }

        drop(data);
        if len == 0 {
            return Ok(0);
        }

        let mut inner_guard = self.inner.lock();
        let atomic_write = len <= PIPE_BUF;

        if inner_guard.reader == 0 {
            drop(inner_guard);
            Self::send_sigpipe();
            return Err(SystemError::EPIPE);
        }

        if inner_guard.data.is_empty() {
            let buf_size = inner_guard.buf_size;
            inner_guard.data = vec![0u8; buf_size];
        }

        let mut total_written: usize = 0;
        let mut reader_wake = WakeMode::None;
        let mut progress_pending = false;

        while total_written < len {
            if inner_guard.reader == 0 {
                let pollflag = progress_pending.then(|| inner_guard.poll_both_ends());
                drop(inner_guard);
                if let Some(pollflag) = pollflag {
                    self.publish_write_progress(reader_wake, pollflag);
                }
                Self::send_sigpipe();
                return if total_written > 0 {
                    Ok(total_written)
                } else {
                    Err(SystemError::EPIPE)
                };
            }

            let remaining = len - total_written;
            let buf_size = inner_guard.buf_size;
            let available_space = buf_size - inner_guard.valid_cnt as usize;
            let need_wait = if atomic_write && total_written == 0 {
                available_space < len
            } else {
                available_space == 0
            };

            if need_wait {
                let pending_poll = progress_pending.then(|| inner_guard.poll_both_ends());
                let pending_reader_wake = reader_wake;
                progress_pending = false;
                reader_wake = WakeMode::None;

                if flags.contains(FileFlags::O_NONBLOCK) {
                    drop(inner_guard);
                    if let Some(pollflag) = pending_poll {
                        self.publish_write_progress(pending_reader_wake, pollflag);
                    }
                    if total_written > 0 {
                        return Ok(total_written);
                    }
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }

                let required = (atomic_write && total_written == 0).then_some(len);
                let (guard, wait_result) = self.wait_for_write_space(inner_guard, required, || {
                    if let Some(pollflag) = pending_poll {
                        self.publish_write_progress(pending_reader_wake, pollflag);
                    }
                });
                inner_guard = guard;

                if let Err(err) = wait_result {
                    return if total_written > 0 {
                        Ok(total_written)
                    } else {
                        Err(err)
                    };
                }
                continue;
            }

            let to_write = core::cmp::min(remaining, available_space);
            let was_readable = inner_guard.valid_cnt > 0 && inner_guard.splice_hold == 0;
            Self::write_bytes(
                &mut inner_guard,
                &buf[total_written..total_written + to_write],
                to_write,
            );
            total_written += to_write;
            progress_pending = true;
            if !was_readable && inner_guard.splice_hold == 0 {
                reader_wake = WakeMode::One;
            }
        }

        let pollflag = inner_guard.poll_both_ends();
        drop(inner_guard);
        if progress_pending {
            self.publish_write_progress(reader_wake, pollflag);
        }

        Ok(total_written)
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
        _private_data: MutexGuard<FilePrivateData>,
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
