use crate::filesystem::vfs::syscall::SpliceFlags;
use crate::libs::mutex::{Mutex, MutexGuard};
use crate::{
    arch::{ipc::signal::Signal, MMArch},
    filesystem::{
        epoll::{
            event_poll::{EPollItemList, EventPoll},
            EPollEventType, EPollItem,
        },
        vfs::{
            fasync::{FAsyncItem, FAsyncItems, FASYNC_POLL_IN, FASYNC_POLL_OUT},
            file::FileFlags,
            merge_metadata_masked,
            vcore::generate_inode_id,
            FilePrivateData, FileSystem, FileType, FsInfo, IndexNode, InodeFlags, InodeMode, Magic,
            Metadata, MetadataUpdate, PollableInode, SetMetadataMask, SuperBlock,
        },
    },
    ipc::signal::send_kernel_signal_to_current,
    libs::{spinlock::SpinLock, wait_queue::WaitQueue},
    mm::MemoryManagementArch,
    process::ProcessState,
    syscall::user_access::UserBufferWriter,
    time::PosixTimeSpec,
};
use core::any::Any;

use crate::cgroup::memory::{accounting::KernelByteKind, object::ObjectAccount, owned::OwnedBox};
use crate::mm::allocation::MemoryAllocationError;
use alloc::sync::{Arc, Weak};
use system_error::SystemError;
pub(crate) mod buffer;
mod identity;
mod read;
mod write;
use buffer::{PipeBuffer, PipeSegment};
pub use identity::LockedPipeInode;

/// 管道缓冲区默认大小（Linux 默认 65536 字节）
pub const PIPE_BUFF_SIZE: usize = 65536;

/// 管道缓冲区最小大小（一页大小，Linux 保证原子写入的最小单位）
pub const PIPE_MIN_SIZE: usize = 4096;

/// PIPE_BUF: writes of <= PIPE_BUF must be atomic.
/// Linux guarantees PIPE_BUF is at least a page (4096).
pub const PIPE_BUF: usize = PIPE_MIN_SIZE;

/// The sole writer waiting for an existing source-splice reservation.
const HOLD_RELEASE_WAIT_TAG: usize = 1;

/// 管道缓冲区最大大小（Linux 默认为 1MB）
pub const PIPE_MAX_SIZE: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum WakeMode {
    #[default]
    None,
    One,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PipeWriterWake {
    None,
    One,
    All,
}

/// Every writer waits for the same free-slot predicate. Intent publication
/// remains under `inner` so exclusive wakeups cannot race waiter admission.
#[derive(Debug, Default)]
struct PipeWriteWaiters {
    count: usize,
}

impl PipeWriteWaiters {
    fn register(&mut self) {
        self.count = self
            .count
            .checked_add(1)
            .expect("pipe writer wait count overflow");
    }
    fn unregister(&mut self) {
        self.count = self
            .count
            .checked_sub(1)
            .expect("pipe writer wait count underflow");
    }
    fn is_empty(&self) -> bool {
        self.count == 0
    }
    fn wake_action(&self, available: usize, has_readers: bool) -> PipeWriterWake {
        if self.is_empty() {
            PipeWriterWake::None
        } else if !has_readers {
            PipeWriterWake::All
        } else if available == 0 {
            PipeWriterWake::None
        } else {
            PipeWriterWake::One
        }
    }
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
    fn page_cache_writeback_domain(
        &self,
    ) -> Option<&Arc<crate::filesystem::page_cache::PageCacheWritebackDomain>> {
        None
    }

    fn root_inode(&self) -> Arc<dyn IndexNode> {
        // PipeFS 没有真正的根 inode，但我们需要实现这个方法
        // 返回一个空的 pipe inode 作为占位符
        LockedPipeInode::new_fifo_shell()
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
    pub(crate) runtime: Option<Arc<PipeRuntime>>,
}

impl PipeFsPrivateData {
    pub fn new(flags: FileFlags) -> Self {
        return PipeFsPrivateData {
            flags,
            runtime: None,
        };
    }

    pub fn set_flags(&mut self, flags: FileFlags) {
        self.flags = flags;
    }
}

/// @brief 管道文件i节点(锁)
#[derive(Debug)]
pub struct PipeRuntime {
    data: OwnedBox<PipeRuntimeData>,
}

impl core::ops::Deref for PipeRuntime {
    type Target = PipeRuntimeData;
    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

#[derive(Debug)]
pub struct PipeRuntimeData {
    /// Serializes writes and output-side splice operations for this pipe.
    ///
    /// Lock order is `writer_transaction` -> `inner`. A blocking writer drops
    /// this mutex only after publishing its wait predicate under `inner`, then
    /// reacquires it before rechecking or committing data.
    writer_transaction: Mutex<()>,
    inner: SpinLock<InnerPipeInode>,
    read_wait_queue: WaitQueue,
    write_wait_queue: WaitQueue,
    /// 用于 FIFO 打开时的阻塞等待（等待另一端打开）
    open_wait_queue: WaitQueue,
    // Match pipe_poll's distinct rd_wait/wr_wait sources. Registrations follow
    // the OFD access mode, not the requested event mask (O_RDWR uses both).
    read_epitems: EPollItemList,
    write_epitems: EPollItemList,
    read_fasync_items: FAsyncItems,
    write_fasync_items: FAsyncItems,
}

/// A serialized writer transaction. Dropping it releases the writer mutex
/// before passing the exclusive writer-wakeup baton.
pub(crate) struct PipeWriterTransaction<'a> {
    pipe: &'a PipeRuntime,
    guard: Option<MutexGuard<'a, ()>>,
    pass_baton: bool,
}

impl PipeWriterTransaction<'_> {
    fn release_for_wait(&mut self) {
        drop(self.guard.take());
    }

    fn reacquire_after_wait(&mut self) {
        debug_assert!(self.guard.is_none());
        self.guard = Some(self.pipe.writer_transaction.lock());
    }

    fn pass_baton_if_waiters_remain(&mut self, inner: &InnerPipeInode) {
        self.pass_baton |= !inner.write_waiters.is_empty();
    }
}

impl Drop for PipeWriterTransaction<'_> {
    fn drop(&mut self) {
        drop(self.guard.take());
        if self.pass_baton {
            let writer_wake = {
                let mut inner = self.pipe.inner.lock();
                PipeRuntime::writer_wake_action(&mut inner)
            };
            self.pipe.wake_writers(writer_wake);
        }
    }
}

/// @brief 管道文件i节点(无锁)
#[derive(Debug)]
pub struct InnerPipeInode {
    /// 管道内可读的数据数
    valid_cnt: i32,
    read_pos: i32,
    write_pos: i32,
    splice_hold: Option<usize>,
    input_copy_active: bool,
    /// 管道缓冲区数据（使用 Vec 支持动态大小）
    data: PipeBuffer,
    /// 当前缓冲区大小
    buf_size: usize,
    /// INode 元数据
    reader: u32,
    writer: u32,
    /// Pipe-space predicates published by writers before they sleep.
    /// Protected by `PipeRuntime::inner`.
    write_waiters: PipeWriteWaiters,
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
    fn read_reserved(&self) -> bool {
        self.splice_hold.is_some() || self.input_copy_active
    }
    fn has_slots(&self) -> bool {
        self.data.segment_count() != 0
    }
    fn pop_empty(&mut self) -> Option<PipeSegment> {
        if self.data.first_segment().is_some_and(|slot| slot.len == 0) {
            self.data.pop_front()
        } else {
            None
        }
    }
    pub fn poll(&self, private_data: &FilePrivateData) -> Result<usize, SystemError> {
        let mut events = EPollEventType::empty();

        let flags = if let FilePrivateData::Pipefs(PipeFsPrivateData { flags, .. }) = private_data {
            flags
        } else {
            return Err(SystemError::EBADFD);
        };

        if !flags.is_write_only() {
            if self.has_slots() && self.splice_hold.is_none() {
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
            if !self.buf_full() {
                events.insert(EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM);
            }

            // 没有读者
            if self.reader == 0 {
                events.insert(EPollEventType::EPOLLERR);
            }
        }

        Ok(events.bits() as usize)
    }

    fn free_slots(&self) -> usize {
        self.buf_size / MMArch::PAGE_SIZE - self.data.segment_count()
    }
    fn writable_bytes(&self) -> usize {
        self.free_slots() * MMArch::PAGE_SIZE
    }
    fn buf_full(&self) -> bool {
        self.free_slots() == 0
    }
    fn consume(&mut self, len: usize) {
        self.data.consume(len);
        self.read_pos = (self.read_pos + len as i32) % self.buf_size as i32;
        self.valid_cnt -= len as i32;
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

        let dst_space = self.writable_bytes();
        let source_segment = match src.data.segment_at(skip) {
            Some(segment) => segment,
            None => return 0,
        };
        let actual_copy = count
            .min(src_real_avail)
            .min(dst_space)
            .min(source_segment.len);

        if actual_copy == 0 {
            return 0;
        }

        // 确保目标缓冲区已分配
        assert!(
            !self.data.is_empty(),
            "transfer output storage prepared before input consumption"
        );

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
        self.data.push_segment(PipeSegment {
            len: actual_copy,
            mergeable: source_segment.mergeable && actual_copy == source_segment.len && skip == 0,
            ..source_segment
        });

        actual_copy
    }
}

impl PipeRuntime {
    pub(crate) fn run_accounting_selftest() -> Result<bool, MemoryAllocationError> {
        use crate::cgroup::memory::{MemoryAccount, MemoryPolicy};
        use crate::mm::allocator::kernel_allocator::allocation_footprint;
        let root = MemoryAccount::new(0x77661f, None)?;
        let account = MemoryAccount::new(0x776620, Some(root.clone()))?;
        let creator = ObjectAccount::new(&account)?;
        let body_layout = core::alloc::Layout::new::<PipeRuntimeData>();
        let body_bytes = allocation_footprint(body_layout);
        let kind = KernelByteKind::SlabUnreclaimable.for_allocation(body_layout) as usize;
        let runtime = Self::try_new(false, &creator)?;
        let weak = Arc::downgrade(&runtime);
        let mut ok = account.snapshot().kernel_bytes[kind] == body_bytes;
        // Admission is page based across this owner's receipts. Fill the
        // body's spare budget so the descriptor needs a distinct new page.
        let padding_bytes = (MMArch::PAGE_SIZE as u64 - body_bytes % MMArch::PAGE_SIZE as u64)
            % MMArch::PAGE_SIZE as u64;
        let padding = creator.try_charge(KernelByteKind::SlabUnreclaimable, padding_bytes)?;
        let baseline = account.snapshot();
        account.set_policy(MemoryPolicy {
            max: Some(baseline.current),
            ..Default::default()
        });
        {
            let transaction = runtime.begin_writer_transaction();
            ok &= matches!(
                runtime.prepare_output_storage(&transaction, &creator),
                Err(MemoryAllocationError::Charge(_))
            );
            let inner = runtime.inner.lock();
            ok &= inner.data.is_empty() && inner.valid_cnt == 0 && inner.buf_size == PIPE_BUFF_SIZE;
        }
        let after_rejection = account.snapshot();
        ok &= after_rejection.current == baseline.current
            && after_rejection.kernel_bytes == baseline.kernel_bytes;
        let ring_layout =
            core::alloc::Layout::array::<u8>(PIPE_BUFF_SIZE).map_err(|_| SystemError::ENOMEM)?;
        // Receipts report bytes, whereas the admission policy and current
        // counter use pages. Only the ring fits; metadata must need a new page.
        let ring_pages = allocation_footprint(ring_layout).div_ceil(MMArch::PAGE_SIZE as u64);
        account.set_policy(MemoryPolicy {
            max: Some(baseline.current + ring_pages),
            ..Default::default()
        });
        {
            let transaction = runtime.begin_writer_transaction();
            ok &= matches!(
                runtime.prepare_output_storage(&transaction, &creator),
                Err(MemoryAllocationError::Charge(_))
            );
            ok &= runtime.inner.lock().data.is_empty();
        }
        let after_rejection = account.snapshot();
        ok &= after_rejection.current == baseline.current
            && after_rejection.kernel_bytes == baseline.kernel_bytes;
        account.set_policy(MemoryPolicy::default());
        {
            let transaction = runtime.begin_writer_transaction();
            runtime.prepare_output_storage(&transaction, &creator)?;
        }
        let segment_layout =
            core::alloc::Layout::array::<PipeSegment>(PIPE_BUFF_SIZE / MMArch::PAGE_SIZE)
                .map_err(|_| SystemError::ENOMEM)?;
        let mut expected = baseline.kernel_bytes;
        for layout in [ring_layout, segment_layout] {
            let kind = KernelByteKind::SlabUnreclaimable.for_allocation(layout) as usize;
            expected[kind] += allocation_footprint(layout);
        }
        ok &= account.snapshot().kernel_bytes == expected;
        drop(padding);
        drop(runtime);
        ok &= weak.upgrade().is_none()
            && account.current_pages() == 0
            && root.current_pages() == 0
            && account.snapshot().kernel_bytes
                == [0; crate::cgroup::memory::accounting::KERNEL_BYTE_KIND_COUNT];
        drop(weak);
        Ok(ok)
    }

    #[cfg(test)]
    fn new() -> Arc<Self> {
        let account = crate::cgroup::memory::MemoryAccount::new(0, None).unwrap();
        let creator = ObjectAccount::new(&account).unwrap();
        Self::try_new(false, &creator).unwrap()
    }
    #[inline]
    pub(crate) fn begin_writer_transaction(&self) -> PipeWriterTransaction<'_> {
        PipeWriterTransaction {
            pipe: self,
            guard: Some(self.writer_transaction.lock()),
            pass_baton: false,
        }
    }

    /// Final pipe-to-pipe transfers serialize with both pipes' user copies.
    /// Enter without either transaction held; use the same address order as
    /// `lock_two` so simultaneous transfers in opposite directions cannot ABBA.
    /// Readiness waits and output allocation belong before this boundary.
    fn begin_transfer_transactions<'a, 'b>(
        src: &'a Self,
        dst: &'b Self,
    ) -> (PipeWriterTransaction<'a>, PipeWriterTransaction<'b>) {
        debug_assert!(!core::ptr::eq(src, dst));
        if (src as *const Self as usize) < (dst as *const Self as usize) {
            (
                src.begin_writer_transaction(),
                dst.begin_writer_transaction(),
            )
        } else {
            let output = dst.begin_writer_transaction();
            let input = src.begin_writer_transaction();
            (input, output)
        }
    }

    #[inline]
    fn writer_wake_action(inner: &mut InnerPipeInode) -> PipeWriterWake {
        let available = inner.writable_bytes();
        inner.write_waiters.wake_action(available, inner.reader > 0)
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
    fn wake_writers(&self, action: PipeWriterWake) {
        match action {
            PipeWriterWake::None => {}
            PipeWriterWake::All => {
                self.write_wait_queue
                    .wakeup_all(Some(ProcessState::Blocked(true)));
            }
            PipeWriterWake::One => {
                self.write_wait_queue.wake_one_tagged(0);
            }
        }
    }

    #[inline]
    fn wake_pipe_waiters(&self, readers: WakeMode, writers: PipeWriterWake) {
        Self::wake_waiters(&self.read_wait_queue, readers);
        self.wake_writers(writers);
    }

    fn send_sigpipe() {
        if let Err(err) = send_kernel_signal_to_current(Signal::SIGPIPE) {
            log::error!("Failed to send SIGPIPE for pipe write: {:?}", err);
        }
    }

    fn publish_write_progress(&self, reader_wake: WakeMode) {
        Self::wake_waiters(&self.read_wait_queue, reader_wake);
        self.notify_readers(EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM);
        self.read_fasync_items.send_sigio(FASYNC_POLL_IN);
    }

    fn notify_readers(&self, key: EPollEventType) {
        let _ = EventPoll::wakeup_epoll(&self.read_epitems, key);
    }

    fn notify_writers(&self, key: EPollEventType) {
        let _ = EventPoll::wakeup_epoll(&self.write_epitems, key);
    }

    /// 安全地锁定两个管道节点（避免死锁）
    /// 返回两个节点的锁保护对象
    /// 注意：p1 和 p2 必须不同，否则会发生死锁（如果尝试对同一个锁加锁两次）或返回不安全的别名引用
    fn lock_two<'a, 'b>(
        p1: &'a PipeRuntime,
        p2: &'b PipeRuntime,
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

    pub(super) fn try_new(
        is_fifo: bool,
        creator: &Arc<ObjectAccount>,
    ) -> Result<Arc<Self>, MemoryAllocationError> {
        let storage = OwnedBox::prepare(creator, KernelByteKind::SlabUnreclaimable)?
            .try_allocate()
            .map_err(|_| SystemError::ENOMEM)?;
        let inner = InnerPipeInode {
            valid_cnt: 0,
            read_pos: 0,
            write_pos: 0,
            splice_hold: None,
            input_copy_active: false,
            had_reader: false,
            data: PipeBuffer::default(),
            buf_size: PIPE_BUFF_SIZE,

            reader: 0,
            writer: 0,
            write_waiters: PipeWriteWaiters::default(),
            is_fifo,
            r_counter: 0, // 初始化读端计数器
            w_counter: 0, // 初始化写端计数器
        };
        let data = storage.init(PipeRuntimeData {
            writer_transaction: Mutex::new(()),
            inner: SpinLock::new(inner),
            read_wait_queue: WaitQueue::default(),
            write_wait_queue: WaitQueue::default(),
            open_wait_queue: WaitQueue::default(),
            read_epitems: EPollItemList::default(),
            write_epitems: EPollItemList::default(),
            read_fasync_items: FAsyncItems::default(),
            write_fasync_items: FAsyncItems::default(),
        });
        Arc::try_new(Self { data }).map_err(|_| SystemError::ENOMEM.into())
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
        !inode.read_reserved() && (inode.has_slots() || inode.writer == 0)
    }

    fn writeable(&self) -> bool {
        let inode = self.inner.lock();
        return !inode.buf_full() || inode.reader == 0;
    }

    /// Sleep for pipe space after publishing the writer's intent under the
    /// pipe lock. Publishing before dropping the lock closes the gap where a
    /// reader could create space without observing the waiter.
    fn wait_for_write_space<'a, F>(
        &'a self,
        mut guard: crate::libs::spinlock::SpinLockGuard<'a, InnerPipeInode>,
        transaction: &mut PipeWriterTransaction<'a>,
        before_wait: F,
    ) -> (
        crate::libs::spinlock::SpinLockGuard<'a, InnerPipeInode>,
        Result<(), SystemError>,
    )
    where
        F: FnOnce(),
    {
        guard.write_waiters.register();
        drop(guard);

        before_wait();
        transaction.release_for_wait();
        let result = self
            .write_wait_queue
            .wait_event_interruptible_tagged(0, || self.writeable());

        transaction.reacquire_after_wait();
        let mut guard = self.inner.lock();
        guard.write_waiters.unregister();
        transaction.pass_baton_if_waiters_remain(&guard);
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
    pub fn set_pipe_size(&self, size: usize) -> Result<usize, MemoryAllocationError> {
        // 验证请求的大小
        // Linux 限制：不能超过 /proc/sys/fs/pipe-max-size（默认 1MB）
        // 大于 i32::MAX 的值是无效的（因为在 64 位系统上 long long 可能传入超大值）
        if size > PIPE_MAX_SIZE || size > i32::MAX as usize {
            return Err(SystemError::EINVAL.into());
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

        let transaction = self.begin_writer_transaction();
        let inner = self.inner.lock();

        // 如果新大小小于当前数据量，返回 EBUSY
        if new_size / page_size < inner.data.segment_count() {
            return Err(SystemError::EBUSY.into());
        }

        let old_size = inner.buf_size;
        if new_size == old_size {
            return Ok(new_size);
        }

        let allocated = !inner.data.is_empty();
        drop(inner);
        let actor =
            crate::cgroup::memory::object_for_task(&crate::process::ProcessManager::current_pcb());
        let mut new_data = if allocated {
            PipeBuffer::try_new(new_size, &actor)?
        } else {
            PipeBuffer::default()
        };
        // The writer transaction excludes writes/resize/close. Readers can
        // advance while allocation runs; copy only the current live range.
        let mut inner = self.inner.lock();
        if inner.valid_cnt > 0 {
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
            inner.write_pos = (data_len % new_size) as i32;
        } else {
            // 没有数据，只需更新大小
            // 如果缓冲区已分配，需要重新分配（大小改变）
            // 如果缓冲区未分配，保持 data 为空（延迟分配）
            // 重置读写位置（应该已经为0）
            inner.read_pos = 0;
            inner.write_pos = 0;
        }

        new_data.copy_segments_from(&inner.data);
        let retired = core::mem::replace(&mut inner.data, new_data);
        inner.buf_size = new_size;

        let writer_wake = Self::writer_wake_action(&mut inner);
        drop(inner);
        drop(transaction);
        drop(retired);

        self.wake_writers(writer_wake);
        self.notify_writers(EPollEventType::empty());

        Ok(new_size)
    }

    /// 获取管道缓冲区大小
    pub fn get_pipe_size(&self) -> usize {
        self.inner.lock().buf_size
    }

    /// Reserve actual output storage before any input consumption. The caller
    /// keeps its writer transaction, but no pipe spinlock crosses allocation.
    pub(crate) fn prepare_output_storage(
        &self,
        transaction: &PipeWriterTransaction<'_>,
        actor: &Arc<ObjectAccount>,
    ) -> Result<(), MemoryAllocationError> {
        debug_assert!(core::ptr::eq(transaction.pipe, self));
        let size = {
            let inner = self.inner.lock();
            if !inner.data.is_empty() {
                return Ok(());
            }
            inner.buf_size
        };
        let prepared = PipeBuffer::try_new(size, actor)?;
        let retired = {
            let mut inner = self.inner.lock();
            assert_eq!(inner.buf_size, size);
            assert!(inner.data.is_empty());
            core::mem::replace(&mut inner.data, prepared)
        };
        drop(retired);
        Ok(())
    }

    /// 当前管道中可读的字节数（不阻塞、不睡眠）
    pub fn readable_len(&self) -> usize {
        let guard = self.inner.lock();
        if guard.read_reserved() {
            return 0;
        }
        guard.valid_cnt.max(0) as usize
    }

    /// Free logical-slot byte capacity, excluding possible tail merges.
    pub fn writable_len(&self) -> usize {
        let guard = self.inner.lock();
        guard.writable_bytes()
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

    /// Preserve exclusive reader-wakeup progress when a selected waiter exits
    /// without consuming data (for example, because a signal interrupted it).
    fn finish_interrupted_reader_wait(&self) {
        let reader_wake = {
            let inner = self.inner.lock();
            if inner.has_slots() && !inner.read_reserved() {
                WakeMode::One
            } else if inner.valid_cnt == 0 && inner.writer == 0 {
                WakeMode::All
            } else {
                WakeMode::None
            }
        };
        Self::wake_waiters(&self.read_wait_queue, reader_wake);
    }

    fn wait_for_readable_event(&self) -> Result<(), SystemError> {
        let result = wq_wait_event_interruptible!(self.read_wait_queue, self.readable(), {});
        if result.is_err() {
            self.finish_interrupted_reader_wait();
        }
        result
    }

    /// Commit file data to this pipe while holding its writer transaction.
    /// Space and reader presence were checked before the input read; close,
    /// resize, and competing writers take the same transaction mutex, so this
    /// commit never needs to drop the transaction and wait.
    pub(crate) fn write_from_splice_locked(
        &self,
        transaction: &mut PipeWriterTransaction<'_>,
        buf: &[u8],
        source_offset: usize,
    ) -> Result<usize, SystemError> {
        debug_assert!(core::ptr::eq(transaction.pipe, self));
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

        assert!(
            !inner_guard.data.is_empty(),
            "splice output prepared before reading input"
        );

        let available = inner_guard.writable_bytes();
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

        let was_readable = inner_guard.valid_cnt > 0 && !inner_guard.read_reserved();
        let to_write = to_write.min(MMArch::PAGE_SIZE - source_offset);
        Self::write_bytes(&mut inner_guard, buf, to_write);
        inner_guard.data.push_segment(PipeSegment {
            offset: source_offset,
            len: to_write,
            mergeable: false,
            packet: false,
        });
        let reader_wake = if !was_readable && !inner_guard.read_reserved() {
            WakeMode::One
        } else {
            WakeMode::None
        };

        transaction.pass_baton_if_waiters_remain(&inner_guard);
        drop(inner_guard);
        Self::wake_waiters(&self.read_wait_queue, reader_wake);
        self.notify_readers(EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM);
        self.read_fasync_items.send_sigio(FASYNC_POLL_IN);

        Ok(to_write)
    }

    /// Wait until the pipe has any writable byte for file->pipe splice.
    ///
    /// This matches Linux `wait_for_space()` for inputs whose exact readable
    /// length is not known before calling into the file. The caller can then
    /// cap the read by the returned byte space while retaining the writer
    /// transaction through the input read and non-waiting commit.
    pub(crate) fn wait_writable_any_for_splice<'a>(
        &'a self,
        transaction: &mut PipeWriterTransaction<'a>,
    ) -> Result<usize, SystemError> {
        debug_assert!(core::ptr::eq(transaction.pipe, self));
        let mut guard = self.inner.lock();
        loop {
            if guard.reader == 0 {
                drop(guard);
                Self::send_sigpipe();
                return Err(SystemError::EPIPE);
            }

            let space = guard.writable_bytes();
            if space > 0 {
                return Ok(space);
            }

            let (next_guard, wait_result) = self.wait_for_write_space(guard, transaction, || {});
            guard = next_guard;
            if let Err(err) = wait_result {
                drop(guard);
                return Err(err);
            }
        }
    }

    /// Check file->pipe splice output conditions without sleeping while the
    /// writer transaction is held. This check happens before consuming input.
    pub(crate) fn writable_for_splice_nonblock(
        &self,
        transaction: &PipeWriterTransaction<'_>,
        wanted: Option<usize>,
    ) -> Result<usize, SystemError> {
        debug_assert!(core::ptr::eq(transaction.pipe, self));
        let guard = self.inner.lock();
        if guard.reader == 0 {
            drop(guard);
            Self::send_sigpipe();
            return Err(SystemError::EPIPE);
        }
        let space = guard.writable_bytes();
        if space == 0 || wanted.is_some_and(|len| len <= PIPE_BUF && space < len) {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        Ok(wanted.map_or(space, |len| len.min(space)))
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
            let mut guard = self.lock_read_state();
            if !guard.read_reserved() {
                let mut freed = false;
                while guard.pop_empty().is_some() {
                    freed = true;
                }
                if freed {
                    let wake = Self::writer_wake_action(&mut guard);
                    drop(guard);
                    self.wake_writers(wake);
                    self.notify_writers(EPollEventType::empty());
                    self.write_fasync_items.send_sigio(FASYNC_POLL_OUT);
                    continue;
                }
                if guard.valid_cnt > 0 {
                    let num = (guard.valid_cnt as usize).min(len);
                    if buf.len() < num {
                        drop(guard);
                        Self::wake_waiters(&self.read_wait_queue, WakeMode::One);
                        return Err(SystemError::EINVAL);
                    }
                    let start = guard.read_pos as usize;
                    let first = num.min(guard.buf_size - start);
                    buf[..first].copy_from_slice(&guard.data[start..start + first]);
                    if num > first {
                        buf[first..num].copy_from_slice(&guard.data[..num - first]);
                    }
                    guard.splice_hold = Some(num);
                    return Ok(num);
                }
                if guard.writer == 0 {
                    return Ok(0);
                }
            }
            if nonblock {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            drop(guard);
            self.wait_for_readable_event()?;
        }
    }

    pub(crate) fn splice_finish_hold(&self, consumed: usize) {
        let mut guard = self.inner.lock();
        let Some(held) = guard.splice_hold else {
            return;
        };
        let slots_before = guard.data.segment_count();

        let consume = consumed.min(held);
        if consume > 0 {
            guard.consume(consume);
        }
        guard.splice_hold = None;
        let freed_slots = guard.data.segment_count() < slots_before;
        let reader_wake = if guard.has_slots() {
            WakeMode::One
        } else if guard.writer == 0 {
            WakeMode::All
        } else {
            WakeMode::None
        };
        let writer_wake = if freed_slots {
            Self::writer_wake_action(&mut guard)
        } else {
            PipeWriterWake::None
        };
        drop(guard);
        self.read_wait_queue.wake_one_tagged(HOLD_RELEASE_WAIT_TAG);
        self.wake_pipe_waiters(reader_wake, writer_wake);
        if freed_slots {
            self.notify_writers(EPollEventType::empty());
        }
        if reader_wake != WakeMode::None {
            self.notify_readers(EPollEventType::empty());
        }
        if reader_wake != WakeMode::None {
            self.read_fasync_items.send_sigio(FASYNC_POLL_IN);
        }
        if freed_slots {
            self.write_fasync_items.send_sigio(FASYNC_POLL_OUT);
        }
    }

    /// Readers wait for an in-flight writer copy just as Linux takes the
    /// pipe mutex before testing O_NONBLOCK. Never wait with `inner` held.
    /// Callers must not already own this pipe's writer transaction.
    fn lock_read_state(&self) -> crate::libs::spinlock::SpinLockGuard<'_, InnerPipeInode> {
        loop {
            let inner = self.inner.lock();
            if !inner.input_copy_active {
                return inner;
            }
            drop(inner);
            // The active input copy owns this transaction until its data or
            // fault is published. Recheck after releasing the barrier.
            drop(self.begin_writer_transaction());
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
            let inner = self.lock_read_state();
            if !inner.read_reserved() {
                if inner.has_slots() {
                    drop(inner);
                    if did_wait {
                        Self::wake_waiters(&self.read_wait_queue, WakeMode::One);
                    }
                    return Ok(true);
                }
                if inner.writer == 0 {
                    drop(inner);
                    if did_wait {
                        Self::wake_waiters(&self.read_wait_queue, WakeMode::All);
                    }
                    return Ok(false);
                }
            }
            if nonblock {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            drop(inner);
            self.wait_for_readable_event()?;
            did_wait = true;
        }
    }

    /// Helper: Wait until the pipe is writable (has space).
    /// Returns:
    /// - Ok(()): Space is available while the destination writer transaction
    ///   remains held.
    /// - Err(e): Interrupted, EAGAIN, or EPIPE (no readers).
    fn wait_writable<'a>(
        &'a self,
        transaction: &mut PipeWriterTransaction<'a>,
        nonblock: bool,
    ) -> Result<(), SystemError> {
        debug_assert!(core::ptr::eq(transaction.pipe, self));
        let mut guard = self.inner.lock();
        loop {
            if guard.reader == 0 {
                drop(guard);
                Self::send_sigpipe();
                return Err(SystemError::EPIPE);
            }
            let space = guard.writable_bytes();
            if space > 0 {
                return Ok(());
            }
            if nonblock {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            let (next_guard, wait_result) = self.wait_for_write_space(guard, transaction, || {});
            guard = next_guard;
            if let Err(err) = wait_result {
                drop(guard);
                return Err(err);
            }
        }
    }

    /// Copy one bounded slot snapshot under the established address-ordered
    /// inner locks. No allocation, user access or waiting occurs here.
    fn transfer_slots(
        src: &PipeRuntime,
        dst: &PipeRuntime,
        source_transaction: &PipeWriterTransaction<'_>,
        transaction: &mut PipeWriterTransaction<'_>,
        len: usize,
        consume: bool,
    ) -> Result<(usize, usize), SystemError> {
        debug_assert!(core::ptr::eq(source_transaction.pipe, src));
        debug_assert!(core::ptr::eq(transaction.pipe, dst));
        debug_assert!(source_transaction.guard.is_some() && transaction.guard.is_some());
        let (mut input, mut output) = Self::lock_two(src, dst);
        debug_assert!(!input.input_copy_active);
        if output.reader == 0 {
            drop(input);
            drop(output);
            Self::send_sigpipe();
            return Err(SystemError::EPIPE);
        }
        if input.read_reserved() {
            return Ok((0, 0));
        }
        let snapshot = input.data.segment_count();
        let mut total = 0;
        let mut slots = 0;
        let mut skip = 0;
        while slots < snapshot && total < len && !output.buf_full() {
            let index = if consume { 0 } else { slots };
            let segment = input.data.get_segment(index).expect("snapshot slot");
            let count = segment.len.min(len - total);
            if count != 0 {
                let copied = output.copy_from_other(&input, count, skip);
                assert_eq!(copied, count);
            } else {
                output.data.push_segment(segment);
            }
            output.data.set_last_segment(PipeSegment {
                len: count,
                mergeable: consume && count == segment.len && segment.mergeable,
                ..segment
            });
            if consume {
                if segment.len == 0 {
                    input.data.pop_front();
                } else {
                    input.consume(count);
                }
            } else {
                skip += count;
            }
            total += count;
            slots += 1;
        }
        let source_reader_wake = if consume && !input.has_slots() && input.writer == 0 {
            WakeMode::All
        } else {
            WakeMode::None
        };
        let source_wake = if consume && slots != 0 {
            Self::writer_wake_action(&mut input)
        } else {
            PipeWriterWake::None
        };
        transaction.pass_baton_if_waiters_remain(&output);
        drop(input);
        drop(output);
        src.wake_pipe_waiters(source_reader_wake, source_wake);
        if consume && slots != 0 {
            src.notify_writers(EPollEventType::empty());
            src.write_fasync_items.send_sigio(FASYNC_POLL_OUT);
        }
        if source_reader_wake != WakeMode::None {
            src.notify_readers(EPollEventType::empty());
        }
        // Linux splice_pipe_to_pipe()/link_pipe() notify the destination only
        // for positive bytes. A zero-only slot transfer changes fresh poll
        // readiness, but does not queue a read event for existing waiters.
        if total != 0 {
            dst.publish_write_progress(WakeMode::One);
        }
        Ok((total, slots))
    }

    pub fn splice_to_pipe(
        &self,
        out: &PipeRuntime,
        len: usize,
        flags: SpliceFlags,
    ) -> Result<usize, MemoryAllocationError> {
        if len == 0 {
            return Ok(0);
        }
        if core::ptr::eq(self, out) {
            return Err(SystemError::EINVAL.into());
        }
        let nonblock = flags.contains(SpliceFlags::SPLICE_F_NONBLOCK);
        let mut total = 0;
        loop {
            match self.wait_readable(nonblock || total != 0) {
                Ok(true) => {}
                Ok(false) => return Ok(total),
                Err(_) if total != 0 => return Ok(total),
                Err(error) => return Err(error.into()),
            }
            let mut transaction = out.begin_writer_transaction();
            if let Err(error) = out.wait_writable(&mut transaction, nonblock || total != 0) {
                return if total != 0 {
                    Ok(total)
                } else {
                    Err(error.into())
                };
            }
            let actor = crate::cgroup::memory::object_for_task(
                &crate::process::ProcessManager::current_pcb(),
            );
            out.prepare_output_storage(&transaction, &actor)?;
            // Allocated storage survives consumption and resize. Release the
            // single-end preparation before entering the ordered pair.
            drop(transaction);
            let (source_transaction, mut transaction) =
                Self::begin_transfer_transactions(self, out);
            let (bytes, slots) = match Self::transfer_slots(
                self,
                out,
                &source_transaction,
                &mut transaction,
                len - total,
                true,
            ) {
                Ok(progress) => progress,
                Err(_) if total != 0 => return Ok(total),
                Err(error) => return Err(error.into()),
            };
            total += bytes;
            if total == len || total != 0 {
                return Ok(total);
            }
            // Moving an empty slot is progress, but a zero byte return is EOF
            // only after rechecking source writer presence, just like Linux.
            if slots == 0 && nonblock {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK.into());
            }
        }
    }

    pub fn tee_to(
        &self,
        out: &PipeRuntime,
        len: usize,
        flags: SpliceFlags,
    ) -> Result<usize, MemoryAllocationError> {
        if len == 0 {
            return Ok(0);
        }
        if core::ptr::eq(self, out) {
            return Err(SystemError::EINVAL.into());
        }
        let nonblock = flags.contains(SpliceFlags::SPLICE_F_NONBLOCK);
        loop {
            if !self.wait_readable(nonblock)? {
                return Ok(0);
            }
            let mut transaction = out.begin_writer_transaction();
            out.wait_writable(&mut transaction, nonblock)?;
            let actor = crate::cgroup::memory::object_for_task(
                &crate::process::ProcessManager::current_pcb(),
            );
            out.prepare_output_storage(&transaction, &actor)?;
            // Allocated storage survives consumption and resize. Release the
            // single-end preparation before entering the ordered pair.
            drop(transaction);
            let (source_transaction, mut transaction) =
                Self::begin_transfer_transactions(self, out);
            let (bytes, slots) =
                Self::transfer_slots(self, out, &source_transaction, &mut transaction, len, false)?;
            if slots != 0 {
                return Ok(bytes);
            }
            if nonblock {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK.into());
            }
        }
    }

    /// 是否存在写端（用于判断空管道时返回 EOF 还是 EAGAIN）
    pub fn has_buffered_slots(&self) -> bool {
        self.inner.lock().has_slots()
    }

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
    use super::{
        ObjectAccount, PipeBuffer, PipeRuntime, PipeSegment, PipeWriteWaiters, PipeWriterWake,
        PIPE_MIN_SIZE,
    };

    #[test]
    fn writer_wake_intent_tracks_exclusive_slot_waiters() {
        let mut waiters = PipeWriteWaiters::default();
        assert_eq!(waiters.wake_action(4096, true), PipeWriterWake::None);
        waiters.register();
        waiters.register();
        assert_eq!(waiters.wake_action(0, true), PipeWriterWake::None);
        assert_eq!(waiters.wake_action(4096, true), PipeWriterWake::One);
        assert_eq!(waiters.wake_action(0, false), PipeWriterWake::All);
        waiters.unregister();
        assert_eq!(waiters.wake_action(4096, true), PipeWriterWake::One);
        waiters.unregister();
        assert!(waiters.is_empty());
    }

    #[test]
    fn transfer_preserves_empty_packet_slots_and_counts_capacity() {
        let src = PipeRuntime::new();
        let dst = PipeRuntime::new();
        let account = crate::cgroup::memory::MemoryAccount::new(1, None).unwrap();
        let creator = ObjectAccount::new(&account).unwrap();
        let storage = PipeBuffer::try_new(PIPE_MIN_SIZE * 2, &creator).unwrap();
        {
            let mut inner = src.inner.lock();
            inner.data = storage;
            inner.buf_size = PIPE_MIN_SIZE * 2;
            inner.reader = 1;
            inner.writer = 1;
            inner.data.push_segment(PipeSegment {
                offset: 0,
                len: 0,
                mergeable: false,
                packet: true,
            });
            inner.data[..3].copy_from_slice(b"abc");
            inner.data.push_segment(PipeSegment {
                offset: 7,
                len: 3,
                mergeable: true,
                packet: false,
            });
            inner.valid_cnt = 3;
            inner.write_pos = 3;
            assert!(inner.buf_full());
        }
        dst.inner.lock().reader = 1;
        let transaction = dst.begin_writer_transaction();
        dst.prepare_output_storage(&transaction, &creator).unwrap();
        drop(transaction);
        let (source_transaction, mut transaction) =
            PipeRuntime::begin_transfer_transactions(&src, &dst);
        assert_eq!(
            PipeRuntime::transfer_slots(
                &src,
                &dst,
                &source_transaction,
                &mut transaction,
                3,
                false
            ),
            Ok((3, 2))
        );
        assert_eq!(src.inner.lock().data.segment_count(), 2);
        {
            let inner = dst.inner.lock();
            let empty = inner.data.get_segment(0).unwrap();
            assert_eq!(empty.len, 0);
            assert!(empty.packet);
            let data = inner.data.get_segment(1).unwrap();
            assert_eq!(data.offset, 7);
            assert!(!data.mergeable);
            assert_eq!(&inner.data[..3], b"abc");
        }
        assert_eq!(
            PipeRuntime::transfer_slots(&src, &dst, &source_transaction, &mut transaction, 3, true),
            Ok((3, 2))
        );
        assert_eq!(src.inner.lock().data.segment_count(), 0);
        assert!(dst.inner.lock().data.get_segment(3).unwrap().mergeable);
    }

    #[test]
    fn transfer_does_not_consume_splice_held_data() {
        let src = PipeRuntime::new();
        let dst = PipeRuntime::new();
        let account = crate::cgroup::memory::MemoryAccount::new(1, None).unwrap();
        let creator = ObjectAccount::new(&account).unwrap();
        let buffer = PipeBuffer::try_new(PIPE_MIN_SIZE, &creator).unwrap();
        {
            let mut src_guard = src.inner.lock();
            src_guard.reader = 1;
            src_guard.writer = 1;
            src_guard.data = buffer;
            src_guard.buf_size = PIPE_MIN_SIZE;
            src_guard.data[..4].copy_from_slice(b"held");
            src_guard.data.push_segment(PipeSegment {
                offset: 0,
                len: 4,
                mergeable: true,
                packet: false,
            });
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
        let transaction = dst.begin_writer_transaction();
        dst.prepare_output_storage(&transaction, &creator).unwrap();
        drop(transaction);
        let (source_transaction, mut transaction) =
            PipeRuntime::begin_transfer_transactions(&src, &dst);
        assert_eq!(
            PipeRuntime::transfer_slots(&src, &dst, &source_transaction, &mut transaction, 4, true),
            Ok((0, 0))
        );
        {
            let src_guard = src.inner.lock();
            assert_eq!(src_guard.valid_cnt, 4);
            assert_eq!(src_guard.read_pos, 0);
        }

        src.splice_finish_hold(0);
        assert_eq!(
            PipeRuntime::transfer_slots(&src, &dst, &source_transaction, &mut transaction, 4, true),
            Ok((4, 1))
        );
        assert_eq!(src.inner.lock().valid_cnt, 0);
    }
}

impl PollableInode for PipeRuntime {
    fn poll(&self, private_data: &FilePrivateData) -> Result<usize, SystemError> {
        self.inner.lock().poll(private_data)
    }

    fn add_epitem(
        &self,
        epitem: Arc<EPollItem>,
        private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        let FilePrivateData::Pipefs(data) = private_data else {
            return Err(SystemError::EBADF);
        };
        if !data.flags.is_write_only() {
            self.read_epitems.add(epitem.clone());
        }
        if !data.flags.is_read_only() {
            self.write_epitems.add(epitem);
        }
        Ok(())
    }

    fn remove_epitem(
        &self,
        epitem: &Arc<EPollItem>,
        private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        let FilePrivateData::Pipefs(data) = private_data else {
            return Err(SystemError::EBADF);
        };
        let read = !data.flags.is_write_only() && self.read_epitems.remove(epitem).is_ok();
        let write = !data.flags.is_read_only() && self.write_epitems.remove(epitem).is_ok();
        if read || write {
            Ok(())
        } else {
            Err(SystemError::ENOENT)
        }
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

    fn release_fasync(
        &self,
        file: &crate::filesystem::vfs::file::File,
        private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        if let FilePrivateData::Pipefs(pipe_data) = private_data {
            let flags = pipe_data.flags;
            if !flags.is_write_only() {
                self.read_fasync_items.remove_file(file);
            }
            if !flags.is_read_only() {
                self.write_fasync_items.remove_file(file);
            }
        } else {
            self.read_fasync_items.remove_file(file);
            self.write_fasync_items.remove_file(file);
        }
        Ok(())
    }
}

impl PipeRuntime {
    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        data_guard: MutexGuard<FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        let data = data_guard.clone();
        drop(data_guard);
        // 获取flags
        let flags: FileFlags;
        if let FilePrivateData::Pipefs(pdata) = &data {
            flags = pdata.flags;
        } else {
            return Err(SystemError::EBADF.into());
        }

        if buf.len() < len {
            return Err(SystemError::EINVAL.into());
        }
        if len == 0 {
            return Ok(0);
        }
        // log::debug!("pipe flags: {:?}", flags);
        // 加锁
        let mut inner_guard = self.lock_read_state();
        let mut did_wait = false;

        while !inner_guard.has_slots() || inner_guard.read_reserved() {
            if !inner_guard.has_slots() && inner_guard.writer == 0 {
                return Ok(0);
            }

            if flags.contains(FileFlags::O_NONBLOCK) {
                drop(inner_guard);
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK.into());
            }

            drop(inner_guard);
            self.wait_for_readable_event()?;
            did_wait = true;

            inner_guard = self.lock_read_state();
        }

        let mut num = 0;
        while num < len && inner_guard.has_slots() {
            let segment = inner_guard
                .data
                .first_segment()
                .expect("pipe bytes have provenance");
            if segment.len == 0 {
                inner_guard.data.pop_front();
                if segment.packet {
                    break;
                }
                if !inner_guard.has_slots() && num == 0 && inner_guard.writer != 0 {
                    let writer_wake = Self::writer_wake_action(&mut inner_guard);
                    drop(inner_guard);
                    self.wake_writers(writer_wake);
                    self.notify_writers(EPollEventType::empty());
                    if flags.contains(FileFlags::O_NONBLOCK) {
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK.into());
                    }
                    self.wait_for_readable_event()?;
                    did_wait = true;
                    inner_guard = self.lock_read_state();
                    // The outer read invocation rechecks empty/held conditions.
                    while !inner_guard.has_slots() || inner_guard.read_reserved() {
                        if !inner_guard.has_slots() && inner_guard.writer == 0 {
                            return Ok(0);
                        }
                        drop(inner_guard);
                        self.wait_for_readable_event()?;
                        inner_guard = self.lock_read_state();
                    }
                }
                continue;
            }
            let count = segment.len.min(len - num);
            let start = inner_guard.read_pos as usize;
            let first = count.min(inner_guard.buf_size - start);
            buf[num..num + first].copy_from_slice(&inner_guard.data[start..start + first]);
            if count > first {
                buf[num + first..num + count].copy_from_slice(&inner_guard.data[..count - first]);
            }
            inner_guard.consume(if segment.packet { segment.len } else { count });
            num += count;
            if segment.packet {
                break;
            }
        }

        let reader_wake = if !inner_guard.has_slots() && inner_guard.writer == 0 {
            WakeMode::All
        } else if did_wait && inner_guard.valid_cnt > 0 && !inner_guard.read_reserved() {
            WakeMode::One
        } else {
            WakeMode::None
        };
        let writer_wake = Self::writer_wake_action(&mut inner_guard);
        drop(inner_guard);
        self.wake_pipe_waiters(reader_wake, writer_wake);
        // 唤醒epoll中等待的进程（忽略错误，因为状态已更新，这是尽力而为的通知）
        self.notify_writers(EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM);
        if reader_wake != WakeMode::None {
            self.notify_readers(EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM);
        }
        self.write_fasync_items.send_sigio(FASYNC_POLL_OUT);

        //返回读取的字节数
        return Ok(num);
    }

    fn open(
        &self,
        mut data: MutexGuard<FilePrivateData>,
        flags: &crate::filesystem::vfs::file::FileFlags,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        // O_PATH: 只获取文件描述符，不实际打开文件进行读写操作
        // 参考 Linux fs/open.c: do_dentry_open() 对 O_PATH 的处理
        // O_PATH 打开不应触发 FIFO 的阻塞等待语义
        if flags.contains(FileFlags::O_PATH) {
            log::debug!("pipe.rs: O_PATH detected, returning early");
            *data = FilePrivateData::Pipefs(PipeFsPrivateData::new(*flags));
            return Ok(());
        }

        let accflags = flags.access_flags();
        let is_nonblock = flags.contains(FileFlags::O_NONBLOCK);
        // The shell already installed the exact runtime pin before admitting
        // this open. Never overwrite it when a FIFO open starts waiting.

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
                        return Err(SystemError::EINTR.into());
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
                        return Err(SystemError::ENXIO.into());
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
                            return Err(SystemError::EINTR.into());
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

    fn close(&self, data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        let flags: FileFlags;
        if let FilePrivateData::Pipefs(pipe_data) = &*data {
            flags = pipe_data.flags;
        } else {
            return Err(SystemError::EBADF);
        }
        let accflags = flags.access_flags();
        drop(data);

        // O_PATH: 只获取文件描述符，不需要reader/writer计数
        // 参考 Linux 对 O_PATH 的处理，close() 不应影响管道的读写端计数
        if flags.contains(FileFlags::O_PATH) {
            return Ok(());
        }

        let transaction = self.begin_writer_transaction();
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
        let writer_wake = if release_notify {
            Self::writer_wake_action(&mut guard)
        } else {
            PipeWriterWake::None
        };
        drop(guard);
        drop(transaction);

        if release_notify {
            self.read_wait_queue.wakeup_all(None);
            self.wake_writers(writer_wake);
            let _ = EventPoll::wakeup_epoll_all(&self.read_epitems, EPollEventType::empty());
            let _ = EventPoll::wakeup_epoll_all(&self.write_epitems, EPollEventType::empty());
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
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        let flags: FileFlags;
        if let FilePrivateData::Pipefs(pdata) = &*data {
            flags = pdata.flags;
        } else {
            return Err(SystemError::EBADF.into());
        }
        drop(data);
        self.write_with_flags(len, buf, flags)
    }

    fn ioctl(
        &self,
        cmd: u32,
        data: usize,
        _private_data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
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
            _ => Err(SystemError::ENOSYS.into()),
        }
    }
}
