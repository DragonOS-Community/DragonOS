use core::fmt::Debug;

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};
use system_error::SystemError;

use crate::{
    filesystem::epoll::{event_poll::EventPoll, EPollEventType},
    libs::{
        spinlock::{SpinLock, SpinLockGuard},
        wait_queue::WaitQueue,
    },
};

use super::tty_core::TtyCore;

pub const TTY_PORT_RX_BUF_SIZE: usize = 8192;
pub const TTY_PORT_RX_CHUNK_SIZE: usize = 256;

#[derive(Debug, Default)]
pub struct TtyInputDrain {
    pub copied: usize,
    pub delivered: usize,
    pub still_pending: bool,
    pub blocked: bool,
    pub freed_room: usize,
}

#[derive(Debug, Eq, PartialEq)]
pub enum TtyInputByteResult {
    Enqueued,
    NoRoom,
    NoData,
    NoTarget,
}

#[derive(Debug)]
struct TtyInputQueue {
    buf: Box<[u8; TTY_PORT_RX_BUF_SIZE]>,
    head: usize,
    len: usize,
    generation: usize,
    draining: bool,
}

impl TtyInputQueue {
    fn new() -> Self {
        Self {
            buf: Box::new([0; TTY_PORT_RX_BUF_SIZE]),
            head: 0,
            len: 0,
            generation: 0,
            draining: false,
        }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn room(&self) -> usize {
        TTY_PORT_RX_BUF_SIZE - self.len
    }

    fn push_slice(&mut self, buf: &[u8]) -> usize {
        let accepted = buf.len().min(self.room());
        for (i, c) in buf[..accepted].iter().enumerate() {
            let idx = (self.head + self.len + i) % TTY_PORT_RX_BUF_SIZE;
            self.buf[idx] = *c;
        }
        self.len += accepted;
        accepted
    }

    fn push_byte(&mut self, c: u8) {
        debug_assert!(self.room() != 0);
        let idx = (self.head + self.len) % TTY_PORT_RX_BUF_SIZE;
        self.buf[idx] = c;
        self.len += 1;
    }

    fn copy_front(&self, out: &mut [u8]) -> (usize, usize) {
        let copied = out.len().min(self.len);
        for (i, slot) in out[..copied].iter_mut().enumerate() {
            *slot = self.buf[(self.head + i) % TTY_PORT_RX_BUF_SIZE];
        }
        (copied, self.generation)
    }

    fn advance_front(&mut self, count: usize) -> usize {
        let count = count.min(self.len);
        self.head = (self.head + count) % TTY_PORT_RX_BUF_SIZE;
        self.len -= count;
        if self.len == 0 {
            self.head = 0;
        }
        count
    }

    fn clear_buffer(&mut self) -> usize {
        let cleared = self.len;
        self.head = 0;
        self.len = 0;
        self.generation = self.generation.wrapping_add(1);
        cleared
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct TtyPortData {
    flags: i32,
    iflags: TtyPortState,
    tty: Weak<TtyCore>,
    /// 内部tty，即与port直接相连的
    internal_tty: Weak<TtyCore>,
}

impl Default for TtyPortData {
    fn default() -> Self {
        Self::new()
    }
}

impl TtyPortData {
    pub fn new() -> Self {
        Self {
            flags: 0,
            iflags: TtyPortState::Initialized,
            tty: Weak::new(),
            internal_tty: Weak::new(),
        }
    }

    pub fn internal_tty(&self) -> Option<Arc<TtyCore>> {
        self.internal_tty.upgrade()
    }

    pub fn tty(&self) -> Option<Arc<TtyCore>> {
        self.tty.upgrade()
    }
}

#[allow(dead_code)]
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum TtyPortState {
    Initialized,
    Suspended,
    Active,
    CtsFlow,
    CheckCD,
    KOPENED,
}

pub trait TtyPort: Sync + Send + Debug {
    fn port_data(&self) -> SpinLockGuard<'_, TtyPortData>;

    /// 获取Port的状态
    fn state(&self) -> TtyPortState {
        self.port_data().iflags
    }

    /// 为port设置tty
    fn setup_internal_tty(&self, tty: Weak<TtyCore>) {
        self.port_data().internal_tty = tty;
    }

    /// 作为客户端的tty ports接收数据
    fn receive_buf(&self, buf: &[u8], _flags: &[u8], count: usize) -> Result<usize, SystemError> {
        let tty = self.port_data().internal_tty().ok_or(SystemError::ENODEV)?;
        let ld = tty.ldisc();
        let ret = ld.receive_buf2(tty.clone(), buf, None, count);
        if let Err(SystemError::ENOSYS) = ret {
            return ld.receive_buf(tty, buf, None, count);
        }
        let event: usize = ld.poll(tty.clone())?;
        let pollflag = EPollEventType::from_bits_truncate(event as u32);
        EventPoll::wakeup_epoll(tty.core().epitems(), pollflag)?;
        ret
    }

    fn internal_tty(&self) -> Option<Arc<TtyCore>> {
        self.port_data().internal_tty()
    }

    fn input_room(&self) -> usize;

    fn enqueue_input(&self, buf: &[u8]) -> usize;

    /// Atomically reserves one byte of input room, then invokes `producer` and
    /// commits the produced byte while holding the input queue lock. `producer`
    /// must be a non-sleeping destructive byte read and must not re-enter TTY locks.
    fn enqueue_input_byte_with(
        &self,
        producer: &mut dyn FnMut() -> Option<u8>,
    ) -> TtyInputByteResult;

    fn has_input(&self) -> bool;

    fn clear_input(&self) -> usize;

    fn clear_input_from_receive(&self) -> usize;

    fn drain_input_to_ldisc(&self, max_count: usize) -> Result<TtyInputDrain, SystemError>;
}

#[derive(Debug)]
pub struct DefaultTtyPort {
    port_data: SpinLock<TtyPortData>,
    input_queue: SpinLock<TtyInputQueue>,
    input_drain_wq: WaitQueue,
}

impl DefaultTtyPort {
    pub fn new() -> Self {
        Self {
            port_data: SpinLock::new(TtyPortData::new()),
            input_queue: SpinLock::new(TtyInputQueue::new()),
            input_drain_wq: WaitQueue::default(),
        }
    }
}

impl TtyPort for DefaultTtyPort {
    fn port_data(&self) -> SpinLockGuard<'_, TtyPortData> {
        self.port_data.lock_irqsave()
    }

    fn input_room(&self) -> usize {
        self.input_queue.lock_irqsave().room()
    }

    fn enqueue_input(&self, buf: &[u8]) -> usize {
        self.input_queue.lock_irqsave().push_slice(buf)
    }

    fn enqueue_input_byte_with(
        &self,
        producer: &mut dyn FnMut() -> Option<u8>,
    ) -> TtyInputByteResult {
        let mut queue = self.input_queue.lock_irqsave();
        if queue.room() == 0 {
            return TtyInputByteResult::NoRoom;
        }
        let Some(c) = producer() else {
            return TtyInputByteResult::NoData;
        };
        queue.push_byte(c);
        TtyInputByteResult::Enqueued
    }

    fn has_input(&self) -> bool {
        !self.input_queue.lock_irqsave().is_empty()
    }

    fn clear_input(&self) -> usize {
        self.input_drain_wq.wait_until(|| {
            let mut queue = self.input_queue.lock_irqsave();
            if queue.draining {
                return None;
            }
            Some(queue.clear_buffer())
        })
    }

    fn clear_input_from_receive(&self) -> usize {
        self.input_queue.lock_irqsave().clear_buffer()
    }

    fn drain_input_to_ldisc(&self, max_count: usize) -> Result<TtyInputDrain, SystemError> {
        let mut chunk = [0u8; TTY_PORT_RX_CHUNK_SIZE];
        let max_count = max_count.min(TTY_PORT_RX_CHUNK_SIZE);
        let (copied, generation) = {
            let mut queue = self.input_queue.lock_irqsave();
            let (copied, generation) = queue.copy_front(&mut chunk[..max_count]);
            if copied != 0 {
                queue.draining = true;
            }
            (copied, generation)
        };

        if copied == 0 {
            return Ok(TtyInputDrain::default());
        }

        let receive_result = self.receive_buf(&chunk[..copied], &[], copied);
        let mut queue = self.input_queue.lock_irqsave();
        queue.draining = false;
        self.input_drain_wq.wakeup_all(None);

        let delivered = match receive_result {
            Ok(delivered) => delivered.min(copied),
            Err(SystemError::ENODEV) => {
                let freed_room = queue.clear_buffer();
                return Ok(TtyInputDrain {
                    copied,
                    freed_room,
                    ..TtyInputDrain::default()
                });
            }
            Err(err) => return Err(err),
        };

        if queue.generation != generation {
            return Ok(TtyInputDrain {
                copied,
                delivered,
                still_pending: !queue.is_empty(),
                blocked: false,
                freed_room: 0,
            });
        }

        let freed_room = queue.advance_front(delivered);
        let still_pending = !queue.is_empty();
        Ok(TtyInputDrain {
            copied,
            delivered,
            still_pending,
            blocked: delivered < copied,
            freed_room,
        })
    }
}
