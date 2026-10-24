use super::klog::LogMessage;
use crate::{
    debug::klog::loglevel::LogLevel,
    libs::{paged_vec::PagedVec, spinlock::SpinLock},
};
use alloc::sync::Arc;
use core::{
    fmt::{self, Write},
    sync::atomic::{compiler_fence, Ordering},
};
use kdepends::ringbuffer::{AllocRingBuffer, RingBuffer};
use log::info;
use system_error::SystemError;

const KMSG_BUFFER_CAPACITY: usize = 1024;
pub static mut KMSG: Option<SpinLock<Kmsg>> = None;

pub fn kmsg_init() {
    info!("kmsg_init");
    let kmsg = SpinLock::new(Kmsg::new());
    compiler_fence(Ordering::SeqCst);
    unsafe { KMSG = Some(kmsg) };
    compiler_fence(Ordering::SeqCst);
    info!("kmsg_init done");
}

pub struct Kmsg {
    buffer: AllocRingBuffer<Arc<LogMessage>>,
    console_loglevel: LogLevel,
    // Monotonic record identifiers, independent of ring indices. Exhausting
    // u64 would require over 584 years at one billion records per second.
    next_seq: u64,
    clear_seq: u64,
}

impl Kmsg {
    pub fn new() -> Self {
        Self {
            buffer: AllocRingBuffer::new(KMSG_BUFFER_CAPACITY),
            console_loglevel: LogLevel::DEFAULT,
            next_seq: 0,
            clear_seq: 0,
        }
    }

    /// Both the new record allocation and eviction destruction belong outside
    /// the IRQ-safe ring lock. Readers only clone the immutable record owner.
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

    /// The caller reserves the complete bounded snapshot before taking the
    /// ring lock. This method only clones owners into already prepared slots.
    pub(super) fn capture(&mut self, snapshot: &mut LogSnapshot, kind: SnapshotKind) {
        assert!(snapshot.records.is_empty());
        let first_seq = self.next_seq - self.buffer.len() as u64;
        let start_seq = if matches!(kind, SnapshotKind::Visible) {
            first_seq.max(self.clear_seq)
        } else {
            first_seq
        };
        snapshot.first_seq = start_seq;
        snapshot.end_seq = self.next_seq;
        if matches!(kind, SnapshotKind::Prefix) {
            snapshot.level = self.console_loglevel.clone();
            self.console_loglevel = LogLevel::DEFAULT;
        }
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

    pub fn set_level(&mut self, log_level: usize) -> Result<usize, SystemError> {
        self.console_loglevel = match log_level {
            1 => LogLevel::EMERG,
            2 => LogLevel::ALERT,
            3 => LogLevel::CRIT,
            4 => LogLevel::ERR,
            5 => LogLevel::WARN,
            6 => LogLevel::NOTICE,
            7 => LogLevel::INFO,
            8 => LogLevel::DEBUG,
            _ => return Err(SystemError::EINVAL),
        };
        Ok(0)
    }
}

pub(super) enum SnapshotKind {
    Prefix,
    Visible,
    Retained,
}

pub(super) struct LogSnapshot {
    records: PagedVec<Arc<LogMessage>, KMSG_BUFFER_CAPACITY>,
    first_seq: u64,
    end_seq: u64,
    level: LogLevel,
}

impl LogSnapshot {
    pub fn try_new() -> Result<Self, SystemError> {
        let mut records = PagedVec::new();
        records.try_reserve(KMSG_BUFFER_CAPACITY)?;
        Ok(Self {
            records,
            first_seq: 0,
            end_seq: 0,
            level: LogLevel::DEFAULT,
        })
    }

    pub fn byte_len(&self) -> usize {
        self.records
            .iter()
            .map(|record| formatted_len(record))
            .sum()
    }

    /// Linux READ_ALL returns the newest complete records that fit. The
    /// returned cutoff includes skipped prefix and complete copied records,
    /// but never a record whose copy faults partway through.
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
            remaining -= formatted_len(record);
            skip += 1;
        }
        let mut cutoff = self.first_seq + skip as u64;
        let mut sink = CopySink {
            copy: &mut copy,
            copied: 0,
            limit,
            error: None,
        };
        for record in self.records.iter().skip(skip) {
            if write!(&mut sink, "{}", record).is_err() {
                return (Err(sink.error.unwrap_or(SystemError::EFAULT)), cutoff);
            }
            cutoff += 1;
        }
        debug_assert_eq!(cutoff, self.end_seq);
        (Ok(sink.copied), cutoff)
    }

    /// Preserve the existing READ/proc prefix policy and one-shot level
    /// filtering; snapshot ownership and formatting are shared, not cursors.
    pub fn read_prefix(
        &self,
        limit: usize,
        mut copy: impl FnMut(&[u8]) -> Result<(), SystemError>,
    ) -> Result<usize, SystemError> {
        let mut sink = CopySink {
            copy: &mut copy,
            copied: 0,
            limit,
            error: None,
        };
        for record in self.records.iter() {
            if (self.level == LogLevel::DEFAULT || record.level() == self.level)
                && write!(&mut sink, "{}", record).is_err()
            {
                break;
            }
        }
        match sink.error {
            Some(error) => Err(error),
            None => Ok(sink.copied),
        }
    }
}

fn formatted_len(record: &LogMessage) -> usize {
    struct Counter(usize);
    impl Write for Counter {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            self.0 += text.len();
            Ok(())
        }
    }
    let mut counter = Counter(0);
    let _ = write!(&mut counter, "{}", record);
    counter.0
}

struct CopySink<'a, F> {
    copy: &'a mut F,
    copied: usize,
    limit: usize,
    error: Option<SystemError>,
}
impl<F: FnMut(&[u8]) -> Result<(), SystemError>> Write for CopySink<'_, F> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let count = text.len().min(self.limit - self.copied);
        if count != 0 {
            if let Err(error) = (self.copy)(&text.as_bytes()[..count]) {
                self.error = Some(error);
                return Err(fmt::Error);
            }
            self.copied += count;
        }
        if count == text.len() {
            Ok(())
        } else {
            Err(fmt::Error)
        }
    }
}
