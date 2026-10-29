use super::klog::{LogMessage, LOG_RECORD_CAPACITY};
use crate::{
    debug::klog::loglevel::KERNEL_LOG_LEVEL,
    exception::tasklet::{tasklet_schedule, Tasklet},
    filesystem::epoll::{
        event_poll::{EPollItemList, EventPoll},
        EPollEventType,
    },
    libs::{lazy_init::Lazy, paged_vec::PagedVec, spinlock::SpinLock, wait_queue::WaitQueue},
    process::cred::{capable, CAPFlags},
};
use alloc::sync::Arc;
use core::sync::atomic::{compiler_fence, Ordering};
use kdepends::ringbuffer::{AllocRingBuffer, RingBuffer};
use log::info;
use system_error::SystemError;

const KMSG_BUFFER_CAPACITY: usize = 1024;
pub(super) const KMSG_TEXT_CAPACITY: usize = KMSG_BUFFER_CAPACITY * LOG_RECORD_CAPACITY;
pub static mut KMSG: Option<SpinLock<Kmsg>> = None;
static KMSG_WAIT: WaitQueue = WaitQueue::default();
pub(super) static KMSG_EPITEMS: EPollItemList = EPollItemList::new();
// Explicitly published after softirq/tasklet initialization. Early records are
// retained normally, before any userspace reader can register for notification.
static KMSG_NOTIFICATION: Lazy<Arc<Tasklet>> = Lazy::new();

pub fn kmsg_notification_init() {
    KMSG_NOTIFICATION.init(Tasklet::new(
        |_, _| {
            KMSG_WAIT.wakeup_all(None);
            let _ = EventPoll::wakeup_epoll(
                &KMSG_EPITEMS,
                EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
            );
        },
        0,
        None,
    ));
}

pub fn kmsg_init() {
    info!("kmsg_init");
    let kmsg = SpinLock::new(Kmsg::new());
    compiler_fence(Ordering::SeqCst);
    unsafe { KMSG = Some(kmsg) };
    compiler_fence(Ordering::SeqCst);
    info!("kmsg_init done");
}

/// Log storage, snapshot visibility, and the shared syslog/proc read cursor.
/// All cursor reservations are short ring-lock transactions; formatting,
/// protected copies, waiting, notification, and final destruction are outside.
pub struct Kmsg {
    buffer: AllocRingBuffer<Arc<LogMessage>>,
    next_seq: u64,
    clear_seq: u64,
    read_seq: u64,
    read_partial: usize,
    saved_console_level: Option<u8>,
}

impl Kmsg {
    pub fn new() -> Self {
        Self {
            buffer: AllocRingBuffer::new(KMSG_BUFFER_CAPACITY),
            next_seq: 0,
            clear_seq: 0,
            read_seq: 0,
            read_partial: 0,
            saved_console_level: None,
        }
    }

    #[must_use = "evicted message must be dropped outside the KMSG lock"]
    pub fn push(&mut self, msg: Arc<LogMessage>) -> Option<Arc<LogMessage>> {
        let evicted = if self.buffer.is_full() {
            self.buffer.dequeue()
        } else {
            None
        };
        self.buffer.push(msg);
        self.next_seq += 1;
        evicted
    }

    fn first_seq(&self) -> u64 {
        self.next_seq - self.buffer.len() as u64
    }

    fn normalize_reader(&mut self) {
        let first = self.first_seq();
        if self.read_seq < first {
            self.read_seq = first;
            self.read_partial = 0;
        }
    }

    pub(super) fn has_unread(&mut self) -> bool {
        self.normalize_reader();
        self.read_seq < self.next_seq
    }

    pub(super) fn unread_bytes(&mut self) -> usize {
        self.normalize_reader();
        self.buffer
            .iter()
            .skip((self.read_seq - self.first_seq()) as usize)
            .map(|record| record.as_bytes().len())
            .sum::<usize>()
            - self.read_partial
    }

    fn reserve_read(&mut self, limit: usize, allow_partial: bool) -> Option<ReadPart> {
        self.normalize_reader();
        // RingBuffer::get wraps its index; it is not a bounds-checked Vec get.
        if self.read_seq >= self.next_seq {
            return None;
        }
        let index = (self.read_seq - self.first_seq()) as usize;
        let record = self.buffer.get(index)?;
        let start = self.read_partial;
        let available = record.as_bytes().len() - start;
        if !allow_partial && available > limit {
            return None;
        }
        let len = available.min(limit);
        let record = record.clone();
        // Linux reserves before usercopy. A fault does not rewind this shared
        // stream, and another reader may reserve the following fragment.
        if len == available {
            self.read_seq += 1;
            self.read_partial = 0;
        } else {
            self.read_partial += len;
        }
        Some(ReadPart { record, start, len })
    }

    /// Caller reserves all bounded snapshot slots before taking this lock.
    pub(super) fn capture(&self, snapshot: &mut LogSnapshot) {
        assert!(snapshot.records.is_empty());
        let first_seq = self.first_seq();
        let start_seq = first_seq.max(self.clear_seq);
        snapshot.first_seq = start_seq;
        snapshot.end_seq = self.next_seq;
        for msg in self.buffer.iter().skip((start_seq - first_seq) as usize) {
            snapshot.records.push(msg.clone());
        }
    }

    pub fn clear(&mut self) {
        self.clear_through(self.next_seq);
    }

    pub fn clear_through(&mut self, seq: u64) {
        self.clear_seq = self.clear_seq.max(seq.min(self.next_seq));
    }

    pub(super) fn console_off(&mut self) {
        if self.saved_console_level.is_none() {
            self.saved_console_level = Some(KERNEL_LOG_LEVEL.get_console_level());
        }
        let _ = KERNEL_LOG_LEVEL.set_console_level(KERNEL_LOG_LEVEL.get_minimum_level());
    }

    pub(super) fn console_on(&mut self) {
        if let Some(level) = self.saved_console_level.take() {
            let _ = KERNEL_LOG_LEVEL.set_console_level(level);
        }
    }

    pub(super) fn set_level(&mut self, level: i32) -> Result<usize, SystemError> {
        if !(1..=8).contains(&level) {
            return Err(SystemError::EINVAL);
        }
        KERNEL_LOG_LEVEL
            .set_console_level((level as u8).max(KERNEL_LOG_LEVEL.get_minimum_level()))?;
        self.saved_console_level = None;
        Ok(0)
    }
}

struct ReadPart {
    record: Arc<LogMessage>,
    start: usize,
    len: usize,
}

pub(super) fn kmsg() -> Result<&'static SpinLock<Kmsg>, SystemError> {
    unsafe { KMSG.as_ref().ok_or(SystemError::ENODEV) }
}

pub(super) fn check_syslog_permission(action: i32) -> Result<(), SystemError> {
    if action == 3
        || action == 10
        || capable(CAPFlags::CAP_SYSLOG)
        || capable(CAPFlags::CAP_SYS_ADMIN)
    {
        Ok(())
    } else {
        Err(SystemError::EPERM)
    }
}

/// Logging may originate inside scheduler or epoll locks. Like Linux printk's
/// irq_work, a preborn tasklet keeps all reader wakeups out of that call stack.
/// Scheduling and callback publication use already prepared nodes.
pub(crate) fn notify_readers() {
    if let Some(tasklet) = KMSG_NOTIFICATION.try_get() {
        tasklet_schedule(tasklet);
    }
}

pub(super) fn read_stream(
    limit: usize,
    mut copy: impl FnMut(&[u8]) -> Result<(), SystemError>,
) -> Result<usize, SystemError> {
    let ring = kmsg()?;
    let mut copied = 0;
    while copied < limit {
        let part = ring
            .lock_irqsave()
            .reserve_read(limit - copied, copied == 0);
        if let Some(part) = part {
            if let Err(error) = copy(&part.record.as_bytes()[part.start..part.start + part.len]) {
                return if copied == 0 { Err(error) } else { Ok(copied) };
            }
            copied += part.len;
        } else if copied != 0 {
            break;
        } else {
            KMSG_WAIT
                .wait_until_interruptible(|| ring.lock_irqsave().has_unread().then_some(()))?;
        }
    }
    Ok(copied)
}

pub(super) struct LogSnapshot {
    records: PagedVec<Arc<LogMessage>, KMSG_BUFFER_CAPACITY>,
    first_seq: u64,
    end_seq: u64,
}

impl LogSnapshot {
    pub fn try_new() -> Result<Self, SystemError> {
        let mut records = PagedVec::new();
        records.try_reserve(KMSG_BUFFER_CAPACITY)?;
        Ok(Self {
            records,
            first_seq: 0,
            end_seq: 0,
        })
    }

    pub fn byte_len(&self) -> usize {
        self.records
            .iter()
            .map(|record| record.as_bytes().len())
            .sum()
    }

    /// READ_ALL selects newest complete records. READ_CLEAR commits only the
    /// complete prefix of that selection whose protected copies succeeded.
    pub fn read_all(
        &self,
        limit: usize,
        mut copy: impl FnMut(&[u8]) -> Result<(), SystemError>,
    ) -> (Result<usize, SystemError>, u64) {
        let mut remaining = self.byte_len();
        let mut skip = 0;
        for record in self.records.iter() {
            if remaining <= limit {
                break;
            }
            remaining -= record.as_bytes().len();
            skip += 1;
        }
        let mut cutoff = self.first_seq + skip as u64;
        let mut copied = 0;
        for record in self.records.iter().skip(skip) {
            if let Err(error) = copy(record.as_bytes()) {
                return (Err(error), cutoff);
            }
            copied += record.as_bytes().len();
            cutoff += 1;
        }
        debug_assert_eq!(cutoff, self.end_seq);
        (Ok(copied), cutoff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{debug::klog::loglevel::LogLevel, time::PosixTimeSpec};

    fn record(text: &str) -> Arc<LogMessage> {
        Arc::new(
            LogMessage::try_new(
                PosixTimeSpec {
                    tv_sec: 1,
                    tv_nsec: 0,
                },
                LogLevel::WARN,
                format_args!("{}", text),
            )
            .unwrap(),
        )
    }

    #[test]
    fn drained_reader_does_not_wrap_to_retained_record() {
        let mut ring = Kmsg::new();
        assert!(ring.reserve_read(4096, true).is_none());
        drop(ring.push(record("first")));
        assert!(ring.reserve_read(4096, true).is_some());
        assert_eq!(ring.unread_bytes(), 0);
        assert!(ring.reserve_read(4096, true).is_none());
        assert_eq!(ring.read_seq, ring.next_seq);
    }

    #[test]
    fn partial_reservation_and_clear_use_independent_cursors() {
        let mut ring = Kmsg::new();
        let record = record("partial");
        let length = record.as_bytes().len();
        drop(ring.push(record));
        let head = ring.reserve_read(1, true).unwrap();
        assert_eq!(head.len, 1);
        assert_eq!(ring.unread_bytes(), length - 1);
        ring.clear();
        let tail = ring.reserve_read(4096, true).unwrap();
        assert_eq!(tail.start, 1);
        assert_eq!(tail.len, length - 1);
        assert!(!ring.has_unread());
    }

    #[test]
    fn overwritten_partial_reader_restarts_at_oldest_survivor() {
        let mut ring = Kmsg::new();
        drop(ring.push(record("evicted")));
        assert!(ring.reserve_read(1, true).is_some());
        for _ in 0..KMSG_BUFFER_CAPACITY {
            drop(ring.push(record("retained")));
        }
        let part = ring.reserve_read(4096, true).unwrap();
        assert_eq!(part.start, 0);
        assert_eq!(part.record.as_bytes(), record("retained").as_bytes());
    }

    #[test]
    fn following_record_is_not_partially_reserved() {
        let mut ring = Kmsg::new();
        drop(ring.push(record("first")));
        drop(ring.push(record("second")));
        assert!(ring.reserve_read(4096, true).is_some());
        let before = ring.unread_bytes();
        assert!(ring.reserve_read(1, false).is_none());
        assert_eq!(ring.unread_bytes(), before);
    }
}
