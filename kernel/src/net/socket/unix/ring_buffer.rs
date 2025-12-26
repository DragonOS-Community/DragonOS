use crate::filesystem::vfs::file::File;
use crate::libs::pod::Pod;
use crate::libs::rwlock::RwLock;
use crate::libs::spinlock::SpinLock;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::num::Wrapping;
use core::ops::Deref;
use core::sync::atomic::{AtomicBool, AtomicUsize};
use inherit_methods_macro::inherit_methods;
use system_error::SystemError;

use super::UCred;

#[derive(Debug, Clone)]
struct ScmRecord {
    /// Absolute byte offset (in ring-buffer head/tail counter space) where this
    /// record begins.
    start: Wrapping<usize>,
    /// Length in bytes of this record's payload in the stream.
    len: usize,
    cred: Option<UCred>,
    rights: Vec<Arc<File>>,
}

impl ScmRecord {
    #[inline]
    fn end(&self) -> Wrapping<usize> {
        self.start + Wrapping(self.len)
    }
}

#[derive(Debug, Clone)]
pub(super) struct StreamRecvmsgPlan {
    pub(super) bytes: usize,
    pub(super) cred: Option<UCred>,
    pub(super) rights: Vec<Arc<File>>,
    pub(super) rights_start: Option<Wrapping<usize>>,
}

/// unix socket的接收队列
///
/// todo 在unix socket中使用的T是u8,后续应该改成抽象的包，而不是原始的u8数组，
#[derive(Debug)]
pub struct RingBuffer<T: Pod> {
    buffer: RwLock<Vec<T>>,
    head: AtomicUsize,
    tail: AtomicUsize,
    /// Consumer has performed SHUT_RD (peer writes must fail with EPIPE).
    recv_shutdown: AtomicBool,
    /// Producer has performed SHUT_WR (consumer reads return EOF once drained).
    send_shutdown: AtomicBool,

    /// Pending connection reset to be reported on the peer's next read.
    /// This is used to emulate Linux AF_UNIX stream behavior when one end is
    /// closed with unread data in its receive queue.
    connreset_pending: AtomicBool,
    /// Record (per-write) queue associated with this byte stream.
    ///
    /// Linux semantics (as exercised by gVisor): unix stream control messages
    /// are associated with the data produced by a single write/sendmsg call.
    /// recvmsg coalescing must respect record boundaries based on SCM_RIGHTS
    /// and SCM_CREDENTIALS.
    ///
    /// When data is consumed via read/recv (not recvmsg), control messages are
    /// discarded as soon as any of their associated data is read.
    scm_queue: SpinLock<VecDeque<ScmRecord>>,
}

#[derive(Debug)]
pub struct Producer<T: Pod, R: Deref<Target = RingBuffer<T>>> {
    ring_buffer: R,
    phantom: PhantomData<T>,
}

#[derive(Debug)]
pub struct Consumer<T: Pod, R: Deref<Target = RingBuffer<T>>> {
    ring_buffer: R,
    _marker: PhantomData<T>,
}

pub type RbProducer<T> = Producer<T, Arc<RingBuffer<T>>>;
pub type RbConsumer<T> = Consumer<T, Arc<RingBuffer<T>>>;

impl<T: Pod> RingBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity.is_power_of_two(),
            "capacity must be a power of two"
        );

        let mut buffer = Vec::with_capacity(capacity);
        // 预先填充缓冲区以确保其长度等于容量（Pod 保证 zeroed 安全）
        buffer.resize_with(capacity, T::new_zeroed);

        Self {
            buffer: RwLock::new(buffer),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            recv_shutdown: AtomicBool::new(false),
            send_shutdown: AtomicBool::new(false),
            connreset_pending: AtomicBool::new(false),
            scm_queue: SpinLock::new(VecDeque::new()),
        }
    }

    pub fn set_connreset_pending(&self) {
        self.connreset_pending
            .store(true, core::sync::atomic::Ordering::Release);
    }

    pub fn take_connreset_pending(&self) -> bool {
        self.connreset_pending
            .swap(false, core::sync::atomic::Ordering::AcqRel)
    }

    pub fn set_recv_shutdown(&self) {
        self.recv_shutdown
            .store(true, core::sync::atomic::Ordering::Release);
    }

    pub fn is_recv_shutdown(&self) -> bool {
        self.recv_shutdown
            .load(core::sync::atomic::Ordering::Acquire)
    }

    pub fn set_send_shutdown(&self) {
        self.send_shutdown
            .store(true, core::sync::atomic::Ordering::Release);
    }

    pub fn is_send_shutdown(&self) -> bool {
        self.send_shutdown
            .load(core::sync::atomic::Ordering::Acquire)
    }

    pub fn push_scm_at(
        &self,
        offset: Wrapping<usize>,
        len: usize,
        cred: Option<UCred>,
        rights: Vec<Arc<File>>,
    ) {
        if len == 0 {
            // Zero-length messages must not make ancillary data visible.
            return;
        }
        self.scm_queue.lock().push_back(ScmRecord {
            start: offset,
            len,
            cred,
            rights,
        });
    }

    pub fn peek_scm_at(&self, offset: Wrapping<usize>) -> Option<(Option<UCred>, Vec<Arc<File>>)> {
        let q = self.scm_queue.lock();
        q.front()
            .filter(|r| r.start <= offset && r.end() > offset)
            .map(|r| (r.cred, r.rights.clone()))
    }

    pub fn next_scm_offset_after(&self, offset: Wrapping<usize>) -> Option<Wrapping<usize>> {
        let q = self.scm_queue.lock();
        q.iter().find(|r| r.start > offset).map(|r| r.start)
    }

    /// Discard record metadata for records whose start is strictly before `head`.
    /// Used by read/recv (not recvmsg).
    pub fn advance_scm_to(&self, head: Wrapping<usize>) {
        let mut q = self.scm_queue.lock();
        while let Some(front) = q.front() {
            if front.start < head {
                q.pop_front();
            } else {
                break;
            }
        }
    }

    /// Advance record metadata to `head`, keeping the current record if `head`
    /// lies within it. Used by recvmsg.
    pub fn advance_records_to(&self, head: Wrapping<usize>) {
        let mut q = self.scm_queue.lock();
        while let Some(front) = q.front() {
            if front.end() <= head {
                q.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn clear_rights_at(&self, start: Wrapping<usize>) {
        let mut q = self.scm_queue.lock();
        for r in q.iter_mut() {
            if r.start == start {
                r.rights.clear();
                break;
            }
        }
    }

    pub(super) fn plan_stream_recvmsg(&self, max: usize, want_creds: bool) -> StreamRecvmsgPlan {
        if max == 0 {
            return StreamRecvmsgPlan {
                bytes: 0,
                cred: None,
                rights: Vec::new(),
                rights_start: None,
            };
        }

        let head = self.head();
        let q = self.scm_queue.lock();
        if q.is_empty() {
            return StreamRecvmsgPlan {
                bytes: max,
                cred: None,
                rights: Vec::new(),
                rights_start: None,
            };
        }

        // Find the first record whose end is after head.
        let mut idx: Option<usize> = None;
        for (i, r) in q.iter().enumerate() {
            if r.end() > head {
                if r.start > head {
                    // Missing metadata for current stream bytes.
                    return StreamRecvmsgPlan {
                        bytes: max,
                        cred: None,
                        rights: Vec::new(),
                        rights_start: None,
                    };
                }
                idx = Some(i);
                break;
            }
        }
        let Some(mut i) = idx else {
            return StreamRecvmsgPlan {
                bytes: max,
                cred: None,
                rights: Vec::new(),
                rights_start: None,
            };
        };

        let base_cred = if want_creds { q[i].cred } else { None };
        let mut pos = head;
        let mut remaining = max;
        let mut bytes = 0usize;
        let mut rights: Vec<Arc<File>> = Vec::new();
        let mut rights_start: Option<Wrapping<usize>> = None;

        while remaining != 0 {
            if i >= q.len() {
                bytes += remaining;
                break;
            }

            let r = &q[i];
            if want_creds && r.cred != base_cred {
                break;
            }

            let end = r.end();
            let avail_in_rec = (end - pos).0;
            if avail_in_rec == 0 {
                i += 1;
                continue;
            }

            let take = core::cmp::min(remaining, avail_in_rec);
            bytes += take;
            remaining -= take;
            pos += Wrapping(take);

            if rights_start.is_none() && !r.rights.is_empty() {
                rights = r.rights.clone();
                rights_start = Some(r.start);
                break;
            }

            if remaining == 0 {
                break;
            }

            if pos == end {
                i += 1;
            } else {
                break;
            }
        }

        StreamRecvmsgPlan {
            bytes,
            cred: base_cred,
            rights,
            rights_start,
        }
    }

    pub fn split(self) -> (RbProducer<T>, RbConsumer<T>) {
        let arc = Arc::new(self);
        let producer = Producer {
            ring_buffer: arc.clone(),
            phantom: PhantomData,
        };
        let consumer = Consumer {
            ring_buffer: arc,
            _marker: PhantomData,
        };
        (producer, consumer)
    }

    pub fn capacity(&self) -> usize {
        // Use the vector length as the effective ring capacity.
        // Vec::capacity() may change independently (realloc/grow), which would break
        // the ring-buffer modulo/mask arithmetic.
        self.buffer.read().len()
    }

    /// Resize the ring buffer to a new power-of-two capacity.
    ///
    /// Keeps head/tail counters in the same absolute space and remaps the stored
    /// data to the new backing buffer. This preserves SCM offsets, which are
    /// expressed in the same head/tail counter space.
    ///
    /// Safety/concurrency: protected by the internal buffer write lock; readers/writers
    /// take the buffer lock for accessing the backing storage.
    ///
    /// # Errors
    ///
    /// Returns `SystemError::EINVAL` if `new_capacity` is not a power of two.
    /// Returns `SystemError::ENOBUFS` if `new_capacity` is too small for existing data.
    pub fn resize(&self, new_capacity: usize) -> Result<(), SystemError> {
        if !new_capacity.is_power_of_two() {
            return Err(SystemError::EINVAL);
        }

        let mut guard = self.buffer.write();
        let old_capacity = guard.len();
        if new_capacity == old_capacity {
            return Ok(());
        }

        let head = self.head();
        let tail = self.tail();
        let len = (tail - head).0;
        if len > new_capacity {
            return Err(SystemError::ENOBUFS);
        }

        let old = core::mem::take(&mut *guard);
        let mut new_buf = Vec::with_capacity(new_capacity);
        new_buf.resize_with(new_capacity, T::new_zeroed);

        for i in 0..len {
            let pos = head.0.wrapping_add(i);
            let old_idx = pos & (old_capacity - 1);
            let new_idx = pos & (new_capacity - 1);
            new_buf[new_idx] = old[old_idx];
        }

        *guard = new_buf;
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[allow(unused)]
    pub fn is_full(&self) -> bool {
        self.free_len() == 0
    }

    pub fn len(&self) -> usize {
        // 计算当前缓冲区中的元素数量
        (self.tail() - self.head()).0
    }

    pub fn free_len(&self) -> usize {
        self.capacity() - self.len()
    }

    pub fn head(&self) -> Wrapping<usize> {
        Wrapping(self.head.load(core::sync::atomic::Ordering::Acquire))
    }

    pub fn tail(&self) -> Wrapping<usize> {
        Wrapping(self.tail.load(core::sync::atomic::Ordering::Acquire))
    }

    #[allow(unused)]
    pub fn clear(&self) {
        self.head.store(0, core::sync::atomic::Ordering::Release);
        self.tail.store(0, core::sync::atomic::Ordering::Release);
    }
}

impl<T: Pod> RingBuffer<T> {
    #[allow(unused)]
    pub fn push(&mut self, item: T) -> Option<()> {
        let mut producer = Producer {
            ring_buffer: self,
            phantom: PhantomData,
        };
        producer.push(item)
    }

    #[allow(unused)]
    pub fn push_slice(&mut self, items: &[T]) -> Option<()> {
        let mut producer = Producer {
            ring_buffer: self,
            phantom: PhantomData,
        };
        producer.push_slice(items)
    }

    #[allow(unused)]
    pub fn pop(&mut self) -> Option<T> {
        let mut consumer = Consumer {
            ring_buffer: self,
            _marker: PhantomData,
        };
        consumer.pop()
    }

    #[allow(unused)]
    pub fn pop_slice(&mut self, buf: &mut [T]) -> Option<()> {
        let mut consumer = Consumer {
            ring_buffer: self,
            _marker: PhantomData,
        };
        consumer.pop_slice(buf)
    }

    pub(self) fn advance_tail(&self, mut tail: Wrapping<usize>, len: usize) {
        tail += len;
        self.tail
            .store(tail.0, core::sync::atomic::Ordering::Release);
    }

    pub(self) fn advance_head(&self, mut head: Wrapping<usize>, len: usize) {
        head += len;
        self.head
            .store(head.0, core::sync::atomic::Ordering::Release);
    }
}

impl<T: Pod, R: Deref<Target = RingBuffer<T>>> Producer<T, R> {
    pub fn push(&mut self, item: T) -> Option<()> {
        let rb = &self.ring_buffer;

        // Synchronize with resize(): compute capacity and index under the same lock
        // so the modulo/mask arithmetic matches the actual backing storage layout.
        let mut write_guard = rb.buffer.write();
        let capacity = write_guard.len();

        let head = rb.head();
        let tail = rb.tail();
        let len = (tail - head).0;
        if len >= capacity {
            return None;
        }

        let index = tail.0 & (capacity - 1);
        write_guard[index] = item;

        rb.advance_tail(tail, 1);
        Some(())
    }

    pub fn push_slice(&mut self, items: &[T]) -> Option<()> {
        let rb = &self.ring_buffer;
        let nitems = items.len();

        if nitems == 0 {
            return Some(());
        }

        // Synchronize with resize(): capacity/offset must be derived from the same
        // backing buffer we are about to write to.
        let mut write_guard = rb.buffer.write();
        let capacity = write_guard.len();

        let head = rb.head();
        let tail = rb.tail();
        let len = (tail - head).0;
        if capacity - len < nitems {
            return None;
        }

        let offset = tail.0 & (capacity - 1);

        // Write items in two parts if necessary
        let mut start = offset;
        let mut remaining_items = items;

        if start + nitems > capacity {
            let first_part = &remaining_items[..capacity - start];
            write_guard[start..start + first_part.len()].copy_from_slice(first_part);

            start = 0;
            remaining_items = &remaining_items[first_part.len()..];
        }

        write_guard[start..start + remaining_items.len()].copy_from_slice(remaining_items);

        // Advance the tail by the number of items written
        rb.advance_tail(tail, nitems);
        Some(())
    }
}

impl<T: Pod, R: Deref<Target = RingBuffer<T>>> Producer<T, R> {
    pub fn is_recv_shutdown(&self) -> bool {
        self.ring_buffer.is_recv_shutdown()
    }

    pub fn is_send_shutdown(&self) -> bool {
        self.ring_buffer.is_send_shutdown()
    }

    pub fn set_send_shutdown(&self) {
        self.ring_buffer.set_send_shutdown()
    }

    pub fn push_scm_at(
        &self,
        offset: Wrapping<usize>,
        len: usize,
        cred: Option<UCred>,
        rights: Vec<Arc<File>>,
    ) {
        self.ring_buffer.push_scm_at(offset, len, cred, rights)
    }

    pub fn resize(&self, new_capacity: usize) -> Result<(), SystemError> {
        self.ring_buffer.resize(new_capacity)
    }

    pub fn set_connreset_pending(&self) {
        self.ring_buffer.set_connreset_pending()
    }
}

#[inherit_methods(from = "self.ring_buffer")]
impl<T: Pod, R: Deref<Target = RingBuffer<T>>> Producer<T, R> {
    pub fn capacity(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn is_full(&self) -> bool;
    pub fn len(&self) -> usize;
    pub fn free_len(&self) -> usize;
    pub fn head(&self) -> Wrapping<usize>;
    pub fn tail(&self) -> Wrapping<usize>;
}

impl<T: Pod, R: Deref<Target = RingBuffer<T>>> Consumer<T, R> {
    pub fn take_connreset_pending(&self) -> bool {
        self.ring_buffer.take_connreset_pending()
    }

    pub fn pop(&mut self) -> Option<T> {
        let rb = &self.ring_buffer;

        // Synchronize with resize(): compute capacity/index under the same lock.
        let read_guard = rb.buffer.read();
        let capacity = read_guard.len();

        let head = rb.head();
        let tail = rb.tail();
        let len = (tail - head).0;
        if len == 0 {
            return None;
        }

        let index = head.0 & (capacity - 1);
        // 因为T是Pod类型，所以可以安全地进行复制
        let item = read_guard[index];

        // 更新 head 指针
        rb.advance_head(head, 1);
        rb.advance_scm_to(head + Wrapping(1));

        Some(item)
    }

    pub fn pop_slice(&mut self, buf: &mut [T]) -> Option<()> {
        let rb = &self.ring_buffer;
        let nitems = buf.len();

        if nitems == 0 {
            return Some(());
        }

        // Synchronize with resize(): capacity/offset must match backing storage.
        let read_guard = rb.buffer.read();
        let capacity = read_guard.len();

        let head = rb.head();
        let tail = rb.tail();
        let available = (tail - head).0;
        if available < nitems {
            return None;
        }

        let offset = head.0 & (capacity - 1);

        let mut start = offset;
        let mut remaining_len = nitems;
        let mut buf_start = 0;

        // 如果需要读取的数据环绕了缓冲区的末尾
        if start + nitems > capacity {
            let first_part_len = capacity - start;
            buf[..first_part_len].copy_from_slice(&read_guard[start..start + first_part_len]);

            start = 0;
            remaining_len -= first_part_len;
            buf_start = first_part_len;
        }

        // 读取剩余部分（或者是整个数据块，如果没有环绕）
        buf[buf_start..buf_start + remaining_len]
            .copy_from_slice(&read_guard[start..start + remaining_len]);

        // 更新 head 指针
        rb.advance_head(head, nitems);
        rb.advance_scm_to(head + Wrapping(nitems));
        Some(())
    }

    /// Pop a slice while preserving record metadata for partially-consumed
    /// records. This is used by recvmsg.
    pub fn pop_slice_preserve_records(&mut self, buf: &mut [T]) -> Option<()> {
        let rb = &self.ring_buffer;
        let nitems = buf.len();

        if nitems == 0 {
            return Some(());
        }

        // Synchronize with resize(): capacity/offset must match backing storage.
        let read_guard = rb.buffer.read();
        let capacity = read_guard.len();

        let head = rb.head();
        let tail = rb.tail();
        let available = (tail - head).0;
        if available < nitems {
            return None;
        }

        let offset = head.0 & (capacity - 1);

        let mut start = offset;
        let mut remaining_len = nitems;
        let mut buf_start = 0;

        if start + nitems > capacity {
            let first_part_len = capacity - start;
            buf[..first_part_len].copy_from_slice(&read_guard[start..start + first_part_len]);

            start = 0;
            remaining_len -= first_part_len;
            buf_start = first_part_len;
        }

        buf[buf_start..buf_start + remaining_len]
            .copy_from_slice(&read_guard[start..start + remaining_len]);

        rb.advance_head(head, nitems);
        rb.advance_records_to(head + Wrapping(nitems));
        Some(())
    }

    /// Peek (read without consuming) a slice from the ring buffer.
    ///
    /// Returns `None` if there aren't enough elements available.
    pub fn peek_slice(&self, buf: &mut [T]) -> Option<()> {
        let rb = &self.ring_buffer;
        let nitems = buf.len();

        if nitems == 0 {
            return Some(());
        }

        // Synchronize with resize(): capacity/offset must match backing storage.
        let read_guard = rb.buffer.read();
        let capacity = read_guard.len();

        let head = rb.head();
        let tail = rb.tail();
        let available = (tail - head).0;
        if available < nitems {
            return None;
        }

        let offset = head.0 & (capacity - 1);

        let mut start = offset;
        let mut remaining_len = nitems;
        let mut buf_start = 0;

        // If the data wraps around the end of the buffer.
        if start + nitems > capacity {
            let first_part_len = capacity - start;
            buf[..first_part_len].copy_from_slice(&read_guard[start..start + first_part_len]);

            start = 0;
            remaining_len -= first_part_len;
            buf_start = first_part_len;
        }

        // Read the remaining part.
        buf[buf_start..buf_start + remaining_len]
            .copy_from_slice(&read_guard[start..start + remaining_len]);

        Some(())
    }

    /// Peek a slice starting at an absolute offset in the ring buffer.
    ///
    /// `offset` is expressed in the same head/tail counter space as `head()` / `tail()`.
    /// Returns `None` if `offset` is outside the readable range or there isn't enough data.
    pub fn peek_slice_at(&self, offset: Wrapping<usize>, buf: &mut [T]) -> Option<()> {
        let rb = &self.ring_buffer;
        let nitems = buf.len();
        if nitems == 0 {
            return Some(());
        }

        let head = rb.head();
        let available = rb.len();
        let dist_from_head = (offset - head).0;
        if dist_from_head > available || dist_from_head + nitems > available {
            return None;
        }

        // Synchronize with resize(): capacity/offset must match backing storage.
        let read_guard = rb.buffer.read();
        let capacity = read_guard.len();
        let start_index = offset.0 & (capacity - 1);

        let mut start = start_index;
        let mut remaining_len = nitems;
        let mut buf_start = 0;

        if start + nitems > capacity {
            let first_part_len = capacity - start;
            buf[..first_part_len].copy_from_slice(&read_guard[start..start + first_part_len]);
            start = 0;
            remaining_len -= first_part_len;
            buf_start = first_part_len;
        }

        buf[buf_start..buf_start + remaining_len]
            .copy_from_slice(&read_guard[start..start + remaining_len]);
        Some(())
    }
}

impl<T: Pod, R: Deref<Target = RingBuffer<T>>> Consumer<T, R> {
    #[allow(dead_code)]
    pub fn is_recv_shutdown(&self) -> bool {
        self.ring_buffer.is_recv_shutdown()
    }

    pub fn is_send_shutdown(&self) -> bool {
        self.ring_buffer.is_send_shutdown()
    }

    pub fn set_recv_shutdown(&self) {
        self.ring_buffer.set_recv_shutdown()
    }

    pub fn peek_scm_at(&self, offset: Wrapping<usize>) -> Option<(Option<UCred>, Vec<Arc<File>>)> {
        self.ring_buffer.peek_scm_at(offset)
    }

    pub fn next_scm_offset_after(&self, offset: Wrapping<usize>) -> Option<Wrapping<usize>> {
        self.ring_buffer.next_scm_offset_after(offset)
    }

    pub(super) fn plan_stream_recvmsg(&self, max: usize, want_creds: bool) -> StreamRecvmsgPlan {
        self.ring_buffer.plan_stream_recvmsg(max, want_creds)
    }

    pub fn clear_rights_at(&self, start: Wrapping<usize>) {
        self.ring_buffer.clear_rights_at(start)
    }

    pub fn resize(&self, new_capacity: usize) -> Result<(), SystemError> {
        self.ring_buffer.resize(new_capacity)
    }
}

#[inherit_methods(from = "self.ring_buffer")]
impl<T: Pod, R: Deref<Target = RingBuffer<T>>> Consumer<T, R> {
    pub fn capacity(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn is_full(&self) -> bool;
    pub fn len(&self) -> usize;
    pub fn free_len(&self) -> usize;
    pub fn head(&self) -> Wrapping<usize>;
    pub fn tail(&self) -> Wrapping<usize>;
}
