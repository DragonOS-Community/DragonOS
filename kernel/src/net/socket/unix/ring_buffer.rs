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

/// unix socket的接收队列
///
/// todo 在unix socket中使用的T是u8,后续应该改成抽象的包，而不是原始的u8数组，
#[derive(Debug)]
pub struct RingBuffer<T> {
    buffer: RwLock<Vec<T>>,
    head: AtomicUsize,
    tail: AtomicUsize,
    /// Consumer has performed SHUT_RD (peer writes must fail with EPIPE).
    recv_shutdown: AtomicBool,
    /// Producer has performed SHUT_WR (consumer reads return EOF once drained).
    send_shutdown: AtomicBool,
    /// SCM_RIGHTS (file descriptor passing) queue associated with this byte stream.
    ///
    /// For SOCK_STREAM, ancillary data is delivered in the order it was sent and
    /// is associated with the next bytes read.
    scm_rights_queue: SpinLock<VecDeque<Vec<Arc<File>>>>,
}

#[derive(Debug)]
pub struct Producer<T, R: Deref<Target = RingBuffer<T>>> {
    ring_buffer: R,
    phantom: PhantomData<T>,
}

#[derive(Debug)]
pub struct Consumer<T, R: Deref<Target = RingBuffer<T>>> {
    ring_buffer: R,
    _marker: PhantomData<T>,
}

pub type RbProducer<T> = Producer<T, Arc<RingBuffer<T>>>;
pub type RbConsumer<T> = Consumer<T, Arc<RingBuffer<T>>>;

impl<T> RingBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity.is_power_of_two(),
            "capacity must be a power of two"
        );

        let mut buffer = Vec::with_capacity(capacity);
        // 预先填充缓冲区以确保其长度等于容量
        buffer.resize_with(capacity, || unsafe { core::mem::zeroed() });

        Self {
            buffer: RwLock::new(buffer),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            recv_shutdown: AtomicBool::new(false),
            send_shutdown: AtomicBool::new(false),
            scm_rights_queue: SpinLock::new(VecDeque::new()),
        }
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

    pub fn push_scm_rights(&self, files: Vec<Arc<File>>) {
        if files.is_empty() {
            return;
        }
        self.scm_rights_queue.lock().push_back(files);
    }

    pub fn pop_scm_rights(&self) -> Option<Vec<Arc<File>>> {
        self.scm_rights_queue.lock().pop_front()
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
        self.buffer.read().capacity()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

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
        if rb.is_full() {
            return None;
        }

        let tail = rb.tail();
        let index = tail.0 & (rb.capacity() - 1);

        let mut write_guard = rb.buffer.write();
        if index >= rb.len() {
            write_guard.push(item);
        } else {
            write_guard[index] = item;
        }

        rb.advance_tail(tail, 1);
        Some(())
    }

    pub fn push_slice(&mut self, items: &[T]) -> Option<()> {
        let rb = &self.ring_buffer;
        let nitems = items.len();
        if rb.free_len() < nitems {
            return None;
        }
        let capacity = rb.capacity();

        let tail = rb.tail();
        let offset = tail.0 & (capacity - 1);

        // Write items in two parts if necessary
        let mut start = offset;
        let mut remaining_items = items;

        let mut write_guard = rb.buffer.write();
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

impl<T, R: Deref<Target = RingBuffer<T>>> Producer<T, R> {
    pub fn is_recv_shutdown(&self) -> bool {
        self.ring_buffer.is_recv_shutdown()
    }

    pub fn is_send_shutdown(&self) -> bool {
        self.ring_buffer.is_send_shutdown()
    }

    pub fn set_send_shutdown(&self) {
        self.ring_buffer.set_send_shutdown()
    }

    pub fn push_scm_rights(&self, files: Vec<Arc<File>>) {
        self.ring_buffer.push_scm_rights(files)
    }
}

#[inherit_methods(from = "self.ring_buffer")]
impl<T, R: Deref<Target = RingBuffer<T>>> Producer<T, R> {
    pub fn capacity(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn is_full(&self) -> bool;
    pub fn len(&self) -> usize;
    pub fn free_len(&self) -> usize;
    pub fn head(&self) -> Wrapping<usize>;
    pub fn tail(&self) -> Wrapping<usize>;
}

impl<T: Pod, R: Deref<Target = RingBuffer<T>>> Consumer<T, R> {
    pub fn pop(&mut self) -> Option<T> {
        let rb = &self.ring_buffer;
        if rb.is_empty() {
            return None;
        }

        let head = rb.head();
        let index = head.0 & (rb.capacity() - 1);

        let read_guard = rb.buffer.read();
        // 因为T是Pod类型，所以可以安全地进行复制
        let item = read_guard[index];

        // 更新 head 指针
        rb.advance_head(head, 1);

        Some(item)
    }

    pub fn pop_slice(&mut self, buf: &mut [T]) -> Option<()> {
        let rb = &self.ring_buffer;
        let nitems = buf.len();
        if rb.len() < nitems {
            return None;
        }

        let head = rb.head();
        let offset = head.0 & (rb.capacity() - 1);

        let read_guard = rb.buffer.read();

        let mut start = offset;
        let mut remaining_len = nitems;
        let mut buf_start = 0;

        // 如果需要读取的数据环绕了缓冲区的末尾
        if start + nitems > rb.capacity() {
            let first_part_len = rb.capacity() - start;
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
        Some(())
    }

    /// Peek (read without consuming) a slice from the ring buffer.
    ///
    /// Returns `None` if there aren't enough elements available.
    pub fn peek_slice(&self, buf: &mut [T]) -> Option<()> {
        let rb = &self.ring_buffer;
        let nitems = buf.len();
        if rb.len() < nitems {
            return None;
        }

        let head = rb.head();
        let offset = head.0 & (rb.capacity() - 1);

        let read_guard = rb.buffer.read();

        let mut start = offset;
        let mut remaining_len = nitems;
        let mut buf_start = 0;

        // If the data wraps around the end of the buffer.
        if start + nitems > rb.capacity() {
            let first_part_len = rb.capacity() - start;
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
}

impl<T, R: Deref<Target = RingBuffer<T>>> Consumer<T, R> {
    pub fn is_recv_shutdown(&self) -> bool {
        self.ring_buffer.is_recv_shutdown()
    }

    pub fn is_send_shutdown(&self) -> bool {
        self.ring_buffer.is_send_shutdown()
    }

    pub fn set_recv_shutdown(&self) {
        self.ring_buffer.set_recv_shutdown()
    }

    pub fn pop_scm_rights(&self) -> Option<Vec<Arc<File>>> {
        self.ring_buffer.pop_scm_rights()
    }
}

#[inherit_methods(from = "self.ring_buffer")]
impl<T, R: Deref<Target = RingBuffer<T>>> Consumer<T, R> {
    pub fn capacity(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn is_full(&self) -> bool;
    pub fn len(&self) -> usize;
    pub fn free_len(&self) -> usize;
    pub fn head(&self) -> Wrapping<usize>;
    pub fn tail(&self) -> Wrapping<usize>;
}
