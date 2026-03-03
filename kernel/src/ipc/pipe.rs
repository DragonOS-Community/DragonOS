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
            file::FileFlags, vcore::generate_inode_id, FilePrivateData, FileSystem, FileType,
            FsInfo, IndexNode, InodeFlags, InodeMode, Magic, Metadata, PollableInode, SuperBlock,
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
    splice_hold: usize,
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
            if !inner_guard.had_reader {
                return Err(SystemError::ENXIO);
            }
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

        Self::write_bytes(&mut inner_guard, buf, to_write);

        if (inner_guard.valid_cnt as usize) < inner_guard.buf_size {
            self.write_wait_queue
                .wakeup(Some(ProcessState::Blocked(true)));
        }
        self.read_wait_queue
            .wakeup(Some(ProcessState::Blocked(true)));

        let pollflag = inner_guard.poll_both_ends();
        drop(inner_guard);
        let _ = EventPoll::wakeup_epoll(&self.epitems, pollflag);

        Ok(to_write)
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
                continue;
            }

            if guard.splice_hold > 0 {
                if nonblock {
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                drop(guard);
                wq_wait_event_interruptible!(self.read_wait_queue, self.readable(), {})?;
                continue;
            }

            let mut num = guard.valid_cnt as usize;
            if len < num {
                num = len;
            }
            if buf.len() < num {
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

        if guard.valid_cnt > 0 || guard.writer == 0 {
            self.read_wait_queue
                .wakeup(Some(ProcessState::Blocked(true)));
        }
        self.write_wait_queue
            .wakeup(Some(ProcessState::Blocked(true)));
        let pollflag = guard.poll_both_ends();
        drop(guard);
        let _ = EventPoll::wakeup_epoll(&self.epitems, pollflag);
    }

    /// Helper: Wait until the pipe is readable (has data).
    /// Returns:
    /// - Ok(true): Data is available.
    /// - Ok(false): EOF (no writers and no data).
    /// - Err(e): Interrupted or EAGAIN.
    fn wait_readable(&self, nonblock: bool) -> Result<bool, SystemError> {
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
                return Ok(true);
            }
            if avail == 0 && !has_writer {
                return Ok(false);
            }
            if nonblock {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            wq_wait_event_interruptible!(self.read_wait_queue, self.readable(), {})?;
        }
    }

    /// Helper: Wait until the pipe is writable (has space).
    /// Returns:
    /// - Ok(()): Space is available.
    /// - Err(e): Interrupted, EAGAIN, or EPIPE (no readers).
    fn wait_writable(&self, nonblock: bool) -> Result<(), SystemError> {
        loop {
            let space = self.writable_len();
            if space > 0 {
                return Ok(());
            }
            if !self.has_readers() {
                let _ = send_kernel_signal_to_current(Signal::SIGPIPE);
                return Err(SystemError::EPIPE);
            }
            if nonblock {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            wq_wait_event_interruptible!(self.write_wait_queue, self.writeable(), {})?;
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

        if in_avail == 0 || out_space == 0 {
            return Ok(0);
        }

        if out_guard.reader == 0 {
            let _ = send_kernel_signal_to_current(Signal::SIGPIPE);
            return Err(SystemError::EPIPE);
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

        // Copy data directly
        let copied = out_guard.copy_from_other(&in_guard, chunk, skip);

        if consume {
            let buf_size = in_guard.buf_size;
            in_guard.read_pos = (in_guard.read_pos + copied as i32) % buf_size as i32;
            in_guard.valid_cnt -= copied as i32;
        }

        // Wakeups
        if consume {
            if in_guard.valid_cnt > 0 {
                src.read_wait_queue
                    .wakeup(Some(ProcessState::Blocked(true)));
            }
            src.write_wait_queue
                .wakeup(Some(ProcessState::Blocked(true)));
        }

        dst.read_wait_queue
            .wakeup(Some(ProcessState::Blocked(true)));
        if (out_guard.valid_cnt as usize) < out_guard.buf_size {
            dst.write_wait_queue
                .wakeup(Some(ProcessState::Blocked(true)));
        }

        let in_poll = in_guard.poll_both_ends();
        let out_poll = out_guard.poll_both_ends();
        drop(in_guard);
        drop(out_guard);

        let _ = EventPoll::wakeup_epoll(&src.epitems, in_poll);
        let _ = EventPoll::wakeup_epoll(&dst.epitems, out_poll);

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
            if out.writable_len() == 0 {
                if !out.has_readers() {
                    let _ = send_kernel_signal_to_current(Signal::SIGPIPE);
                    return Err(SystemError::EPIPE);
                }
                if nonblock || total > 0 {
                    if total > 0 {
                        return Ok(total);
                    }
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                // force blocking wait
                out.wait_writable(false)?;
            }

            let copied = Self::transfer_chunk(self, out, len - total, false, |in_guard| {
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
            })?;

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
    use super::LockedPipeInode;

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
        // log::debug!("pipe flags: {:?}", flags);
        // 加锁
        let mut inner_guard = self.inner.lock();

        while inner_guard.valid_cnt == 0 || inner_guard.splice_hold > 0 {
            if inner_guard.valid_cnt == 0 && inner_guard.writer == 0 {
                return Ok(0);
            }

            if inner_guard.valid_cnt == 0 {
                self.write_wait_queue
                    .wakeup(Some(ProcessState::Blocked(true)));
            }

            if flags.contains(FileFlags::O_NONBLOCK) {
                drop(inner_guard);
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }

            drop(inner_guard);
            wq_wait_event_interruptible!(self.read_wait_queue, self.readable(), {})?;

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
        let pollflag = inner_guard.poll_both_ends();
        drop(inner_guard);
        // 唤醒epoll中等待的进程（忽略错误，因为状态已更新，这是尽力而为的通知）
        let _ = EventPoll::wakeup_epoll(&self.epitems, pollflag);

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
                // 读端耗尽意味着写端应收到 POLLERR，唤醒等待者与 epoll。
                // 注意：这里需要使用写端的flags来获取EPOLLERR事件
                // 因为poll()中只在!flags.is_read_only()时才设置EPOLLERR
                let poll_data = FilePrivateData::Pipefs(PipeFsPrivateData {
                    flags: FileFlags::O_WRONLY,
                });
                let pollflag = guard
                    .poll(&poll_data)
                    .map(|v| EPollEventType::from_bits_truncate(v as u32))
                    .unwrap_or(EPollEventType::EPOLLERR);

                drop(guard); // 先释放 inner 锁，避免死锁
                             // 唤醒所有等待的写端（不进行状态过滤，因为进程可能已经被其他操作唤醒但还未从队列中移除）
                self.write_wait_queue.wakeup_all(None);
                // 唤醒所有依赖 epoll 的等待者，确保 ERR 事件可见
                let _ = EventPoll::wakeup_epoll(&self.epitems, pollflag);
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
        data: MutexGuard<FilePrivateData>,
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

        // PIPE_BUF atomicity: if len <= PIPE_BUF, writes must be all-or-nothing.
        // We implement this by waiting for enough room before writing the first byte.
        let atomic_write = len <= PIPE_BUF;

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
                        if let Err(e) = send_kernel_signal_to_current(Signal::SIGPIPE) {
                            log::error!("Failed to send SIGPIPE for pipe write: {:?}", e);
                        }
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

            // 如果没有足够空间需要等待
            // - non-atomic writes: only wait when pipe is full
            // - atomic writes (<= PIPE_BUF): wait until we have room for the entire write
            let need_wait = if atomic_write && total_written == 0 {
                available_space < len
            } else {
                available_space == 0
            };

            if need_wait {
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

                let r = if atomic_write && total_written == 0 {
                    wq_wait_event_interruptible!(
                        self.write_wait_queue,
                        self.writeable_len_at_least(len),
                        {}
                    )
                } else {
                    wq_wait_event_interruptible!(self.write_wait_queue, self.writeable(), {})
                };

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
                        if let Err(e) = send_kernel_signal_to_current(Signal::SIGPIPE) {
                            log::error!("Failed to send SIGPIPE for pipe write: {:?}", e);
                        }
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

            Self::write_bytes(
                &mut inner_guard,
                &buf[total_written..total_written + to_write],
                to_write,
            );
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

        let pollflag = inner_guard.poll_both_ends();

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
