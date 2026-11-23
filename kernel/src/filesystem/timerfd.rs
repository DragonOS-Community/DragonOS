use crate::filesystem::vfs::file::FileMode;
use alloc::{
    boxed::Box,
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::any::Any;

use system_error::SystemError;

use crate::{
    filesystem::{
        anon_inode::{anon_inode_metadata, anon_inode_path, AnonInodeFs},
        epoll::{event_poll::EventPoll, EPollEventType, EPollItem},
        vfs::{
            file::{FileFlags, FilePrivateData},
            FileSystem, IndexNode, InodeMode, Metadata, OpenFileBehavior, PollableInode,
            PostWriteSyncPolicy,
        },
    },
    libs::{
        mutex::{Mutex, MutexGuard},
        spinlock::{SpinLock, SpinLockGuard},
        wait_queue::WaitQueue,
    },
    process::{posix_timer::PosixItimerspec, ProcessManager},
    syscall::user_buffer::UserBuffer,
    time::{
        absolute_timer::{AbsoluteClock, AbsoluteTimer, PreparedAbsoluteTimer},
        syscall::PosixClockID,
        timekeeping::realtime_now_with_clock_set_seq,
        timer::TimerFunction,
        PosixTimeSpec, NSEC_PER_SEC,
    },
};

use super::epoll::event_poll::EPollItemList;

const KTIME_MAX_NS: u64 = i64::MAX as u64;
const KTIME_SEC_MAX: i64 = i64::MAX / NSEC_PER_SEC as i64;

lazy_static::lazy_static! {
    // Registry operations only run in task context.  A sleeping mutex keeps
    // Vec allocation and O(N) cleanup out of irq-disabled spinlock sections.
    static ref REALTIME_TIMERFDS: Mutex<BTreeMap<usize, Weak<TimerFdInode>>> =
        Mutex::new(BTreeMap::new());
}

bitflags! {
    pub struct TimerFdCreateFlags: u32 {
        const TFD_NONBLOCK = FileFlags::O_NONBLOCK.bits();
        const TFD_CLOEXEC = FileFlags::O_CLOEXEC.bits();
    }

    pub struct TimerFdSettimeFlags: u32 {
        const TFD_TIMER_ABSTIME = 1;
        const TFD_TIMER_CANCEL_ON_SET = 2;
    }
}

#[derive(Debug)]
struct TimerFdState {
    clock_id: PosixClockID,
    interval_ns: u64,
    next_expiry_ns: Option<u64>,
    deadline_domain: AbsoluteClock,
    timer: Option<Arc<AbsoluteTimer>>,
    ticks: u64,
    expired: bool,
    cancel_pending: bool,
    generation: u64,
    observed_clock_set_seq: Option<u64>,
    shutdown: bool,
}

impl TimerFdState {
    fn current_spec(&self, expired: bool) -> PosixItimerspec {
        let mut value = PosixTimeSpec::default();
        if let Some(mut deadline) = self.next_expiry_ns {
            let now = self.deadline_domain.now_ns();
            if expired && self.interval_ns != 0 && now >= deadline {
                let periods = (now - deadline) / self.interval_ns + 1;
                deadline = deadline
                    .saturating_add(self.interval_ns.saturating_mul(periods))
                    .min(KTIME_MAX_NS);
            }
            value = PosixTimeSpec::from_ns(deadline.saturating_sub(now));
        }
        PosixItimerspec {
            it_interval: PosixTimeSpec::from_ns(self.interval_ns),
            it_value: value,
        }
    }
}

#[derive(Debug)]
pub struct TimerFdInode {
    state: SpinLock<TimerFdState>,
    operation: Mutex<()>,
    wait_queue: WaitQueue,
    epitems: EPollItemList,
    metadata: Metadata,
    self_weak: Weak<TimerFdInode>,
}

impl TimerFdInode {
    pub fn new(clock_id: PosixClockID) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            state: SpinLock::new(TimerFdState {
                clock_id,
                interval_ns: 0,
                next_expiry_ns: None,
                deadline_domain: Self::absolute_domain(clock_id),
                timer: None,
                ticks: 0,
                expired: false,
                cancel_pending: false,
                generation: 0,
                observed_clock_set_seq: None,
                shutdown: false,
            }),
            operation: Mutex::new(()),
            wait_queue: WaitQueue::default(),
            epitems: EPollItemList::default(),
            metadata: anon_inode_metadata(InodeMode::S_IRUSR | InodeMode::S_IWUSR),
            self_weak: weak.clone(),
        })
    }

    fn absolute_domain(clock_id: PosixClockID) -> AbsoluteClock {
        match clock_id {
            PosixClockID::Realtime | PosixClockID::RealtimeAlarm => AbsoluteClock::Realtime,
            PosixClockID::Boottime | PosixClockID::BoottimeAlarm => AbsoluteClock::Boottime,
            _ => AbsoluteClock::Monotonic,
        }
    }

    fn relative_domain(clock_id: PosixClockID) -> AbsoluteClock {
        match clock_id {
            PosixClockID::Boottime | PosixClockID::BoottimeAlarm => AbsoluteClock::Boottime,
            _ => AbsoluteClock::Monotonic,
        }
    }

    fn timespec_to_ktime_ns(value: PosixTimeSpec) -> Result<u64, SystemError> {
        if !value.is_valid_timeout() {
            return Err(SystemError::EINVAL);
        }
        if value.tv_sec >= KTIME_SEC_MAX {
            return Ok(KTIME_MAX_NS);
        }
        Ok((value.tv_sec as u64) * NSEC_PER_SEC as u64 + value.tv_nsec as u64)
    }

    pub fn validate_spec(value: &PosixItimerspec) -> Result<(), SystemError> {
        Self::timespec_to_ktime_ns(value.it_value)?;
        Self::timespec_to_ktime_ns(value.it_interval)?;
        Ok(())
    }

    fn is_realtime_absolute(clock_id: PosixClockID, flags: TimerFdSettimeFlags) -> bool {
        matches!(
            clock_id,
            PosixClockID::Realtime | PosixClockID::RealtimeAlarm
        ) && flags.contains(TimerFdSettimeFlags::TFD_TIMER_ABSTIME)
    }

    fn registry_add(&self) {
        REALTIME_TIMERFDS
            .lock()
            .insert(self as *const Self as usize, self.self_weak.clone());
    }

    fn registry_remove(&self) {
        REALTIME_TIMERFDS
            .lock()
            .remove(&(self as *const Self as usize));
    }

    fn prepare_backend(
        &self,
        deadline_ns: u64,
        domain: AbsoluteClock,
        generation: u64,
    ) -> Result<PreparedAbsoluteTimer, SystemError> {
        let expiry = Box::try_new(TimerFdExpiry {
            inode: self.self_weak.clone(),
            generation,
        })
        .map_err(|_| SystemError::ENOMEM)?;
        AbsoluteTimer::try_prepare(domain, PosixTimeSpec::from_ns(deadline_ns), expiry)
    }

    fn notify_readable(&self) {
        self.wait_queue.wakeup_all(None);
        let _ = EventPoll::wakeup_epoll(&self.epitems, EPollEventType::EPOLLIN);
    }

    fn has_ticks(&self) -> bool {
        self.state.lock_irqsave().ticks != 0
    }

    fn consume_cancel_locked(state: &mut TimerFdState) {
        let backend_expired = state.expired;
        state.cancel_pending = false;
        state.ticks = 0;
        state.expired = false;
        if backend_expired {
            // Linux does not rearm an already-expired timer when read returns
            // ECANCELED.  Keep cancel-list membership, but logically disarm
            // it so a later clock set cannot resurrect the old deadline.
            state.next_expiry_ns = None;
        }
    }

    /// The caller holds operation. An expired timer has no backend, so its
    /// state is stable while the successor is prepared outside the spinlock.
    fn prepare_periodic_rearm<'a>(
        &'a self,
        mut state: SpinLockGuard<'a, TimerFdState>,
    ) -> Result<
        (
            SpinLockGuard<'a, TimerFdState>,
            Option<PreparedAbsoluteTimer>,
        ),
        SystemError,
    > {
        let Some(deadline) = state.next_expiry_ns else {
            return Ok((state, None));
        };
        if !state.expired || state.interval_ns == 0 {
            return Ok((state, None));
        }
        let domain = state.deadline_domain;
        let generation = state.generation;
        let now = domain.now_ns();
        let periods = now
            .checked_sub(deadline)
            .map_or(0, |elapsed| elapsed / state.interval_ns + 1);
        let next = deadline
            .saturating_add(state.interval_ns.saturating_mul(periods))
            .min(KTIME_MAX_NS);
        drop(state);
        let backend = self.prepare_backend(next, domain, generation)?;
        state = self.state.lock_irqsave();
        // A realtime step back can give zero periods. Linux retracts the
        // preceding callback's contribution before rearming the same deadline.
        state.ticks = state.ticks.wrapping_add(periods.wrapping_sub(1));
        state.expired = false;
        state.next_expiry_ns = Some(next);
        state.timer = Some(backend.timer());
        Ok((state, Some(backend)))
    }

    fn read_ticks(&self, nonblock: bool) -> Result<u64, SystemError> {
        loop {
            if !nonblock && !self.has_ticks() {
                if ProcessManager::current_pcb().has_pending_signal_fast() {
                    return Err(SystemError::ERESTARTSYS);
                }
                wq_wait_event_interruptible!(self.wait_queue, self.has_ticks(), {})?;
            }

            let _operation = self.operation.lock();
            let mut state = self.state.lock_irqsave();
            if state.cancel_pending {
                Self::consume_cancel_locked(&mut state);
                return Err(SystemError::ECANCELED);
            }
            if state.ticks == 0 {
                if nonblock {
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                continue;
            }

            let (mut state, backend) = self.prepare_periodic_rearm(state)?;
            let ticks = state.ticks;
            state.ticks = 0;
            state.expired = false;
            if state.interval_ns == 0 {
                state.next_expiry_ns = None;
            }
            drop(state);
            if let Some(backend) = backend {
                backend.activate();
            }
            if ticks == 0 && nonblock {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            return Ok(ticks);
        }
    }

    pub fn gettime(&self) -> Result<PosixItimerspec, SystemError> {
        let _operation = self.operation.lock();
        let (state, backend) = self.prepare_periodic_rearm(self.state.lock_irqsave())?;
        let value = state.current_spec(state.expired);
        drop(state);
        if let Some(backend) = backend {
            backend.activate();
        }
        Ok(value)
    }

    pub fn settime(
        &self,
        flags: TimerFdSettimeFlags,
        value: PosixItimerspec,
    ) -> Result<PosixItimerspec, SystemError> {
        let interval_ns = Self::timespec_to_ktime_ns(value.it_interval)?;
        let initial_ns = Self::timespec_to_ktime_ns(value.it_value)?;
        let _operation = self.operation.lock();

        let (clock_id, registered, generation) = {
            let state = self.state.lock_irqsave();
            (
                state.clock_id,
                state.observed_clock_set_seq.is_some(),
                state.generation.wrapping_add(1),
            )
        };
        let cancel_on_set = Self::is_realtime_absolute(clock_id, flags)
            && flags.contains(TimerFdSettimeFlags::TFD_TIMER_CANCEL_ON_SET);
        let new_registration = cancel_on_set && !registered;
        if new_registration {
            // A clock step must either be in our snapshot or find this inode
            // in the registry and wait for operation to finish publishing it.
            self.registry_add();
        }

        let absolute = flags.contains(TimerFdSettimeFlags::TFD_TIMER_ABSTIME);
        let domain = if absolute {
            Self::absolute_domain(clock_id)
        } else {
            Self::relative_domain(clock_id)
        };
        let (now, observed_clock_set_seq) = if cancel_on_set {
            let (now, seq) = realtime_now_with_clock_set_seq();
            (now.to_ktime_ns(), Some(seq))
        } else {
            (domain.now_ns(), None)
        };
        let deadline = if initial_ns == 0 {
            None
        } else if absolute {
            Some(initial_ns)
        } else {
            Some(now.saturating_add(initial_ns).min(KTIME_MAX_NS))
        };
        let immediate = deadline.is_some_and(|expiry| expiry <= now);
        let backend = if let Some(deadline) = deadline.filter(|_| !immediate) {
            match self.prepare_backend(deadline, domain, generation) {
                Ok(backend) => Some(backend),
                Err(error) => {
                    if new_registration {
                        self.registry_remove();
                    }
                    return Err(error);
                }
            }
        } else {
            None
        };

        let (old, old_backend, report_canceled) = {
            let mut state = self.state.lock_irqsave();
            let old_expired =
                state.expired || state.timer.as_ref().is_some_and(|timer| timer.fired());
            let old = state.current_spec(old_expired);
            let old_backend = core::mem::replace(
                &mut state.timer,
                backend.as_ref().map(PreparedAbsoluteTimer::timer),
            );
            state.generation = generation;
            state.interval_ns = interval_ns;
            state.next_expiry_ns = deadline;
            state.deadline_domain = domain;
            state.ticks = u64::from(immediate);
            state.expired = immediate;
            state.observed_clock_set_seq = observed_clock_set_seq;
            state.cancel_pending &= cancel_on_set;
            let report_canceled = initial_ns != 0 && state.cancel_pending;
            if report_canceled {
                state.cancel_pending = false;
            }
            (old, old_backend, report_canceled)
        };

        if let Some(old_backend) = old_backend {
            old_backend.cancel();
        }
        if registered && !cancel_on_set {
            self.registry_remove();
        }
        if let Some(backend) = backend {
            backend.activate();
        }
        if immediate {
            self.notify_readable();
        }
        if report_canceled {
            Err(SystemError::ECANCELED)
        } else {
            Ok(old)
        }
    }

    fn clock_was_set(&self) {
        let _operation = self.operation.lock();
        let (_, seq) = realtime_now_with_clock_set_seq();
        let mut state = self.state.lock_irqsave();
        if state
            .observed_clock_set_seq
            .is_none_or(|observed| observed == seq)
        {
            return;
        }
        state.observed_clock_set_seq = Some(seq);
        state.cancel_pending = true;
        state.ticks = state.ticks.wrapping_add(1);
        drop(state);
        self.notify_readable();
    }

    pub fn is_alarm(&self) -> bool {
        matches!(
            self.state.lock_irqsave().clock_id,
            PosixClockID::RealtimeAlarm | PosixClockID::BoottimeAlarm
        )
    }
}

#[derive(Debug)]
struct TimerFdExpiry {
    inode: Weak<TimerFdInode>,
    generation: u64,
}

impl TimerFunction for TimerFdExpiry {
    fn run(&mut self) -> Result<(), SystemError> {
        let Some(inode) = self.inode.upgrade() else {
            return Ok(());
        };
        let mut state = inode.state.lock_irqsave();
        if state.shutdown || state.generation != self.generation || state.next_expiry_ns.is_none() {
            return Ok(());
        }
        state.timer = None;
        state.expired = true;
        state.ticks = state.ticks.wrapping_add(1);
        drop(state);
        inode.notify_readable();
        Ok(())
    }
}

impl PollableInode for TimerFdInode {
    fn poll(&self, _private_data: &FilePrivateData) -> Result<usize, SystemError> {
        Ok(if self.has_ticks() {
            EPollEventType::EPOLLIN.bits() as usize
        } else {
            0
        })
    }

    fn add_epitem(
        &self,
        epitem: Arc<EPollItem>,
        _private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        self.epitems.add(epitem);
        Ok(())
    }

    fn remove_epitem(
        &self,
        epitem: &Arc<EPollItem>,
        _private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        self.epitems.remove(epitem)
    }
}

impl IndexNode for TimerFdInode {
    fn configure_open_file(&self, _data: &FilePrivateData, behavior: &mut OpenFileBehavior) {
        behavior.mode.remove(FileMode::FMODE_CAN_WRITE);
        behavior.post_write_sync = PostWriteSyncPolicy::NotApplicable;
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn has_noop_llseek(&self) -> bool {
        true
    }

    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> = Ok(());
        result.map_err(Into::into)
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        let _operation = self.operation.lock();
        let (backend, registry_registered) = {
            let mut state = self.state.lock_irqsave();
            state.shutdown = true;
            state.generation = state.generation.wrapping_add(1);
            state.next_expiry_ns = None;
            let registry_registered = state.observed_clock_set_seq.take().is_some();
            (state.timer.take(), registry_registered)
        };
        if registry_registered {
            self.registry_remove();
        }
        if let Some(backend) = backend {
            backend.cancel();
        }
        Ok(())
    }

    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> = (|| {
            if len < core::mem::size_of::<u64>() {
                return Err(SystemError::EINVAL);
            }
            let nonblock = matches!(
                &*data,
                FilePrivateData::TimerFd(flags) if flags.contains(FileFlags::O_NONBLOCK)
            );
            drop(data);
            let ticks = self.read_ticks(nonblock)?;
            if ticks == 0 {
                return Ok(0);
            }
            buf[..8].copy_from_slice(&ticks.to_ne_bytes());
            Ok(8)
        })();
        result.map_err(Into::into)
    }

    fn user_read_mode(&self) -> crate::filesystem::vfs::UserReadMode {
        crate::filesystem::vfs::UserReadMode::Scalar
    }

    fn read_user_at(
        &self,
        _offset: usize,
        len: usize,
        writer: &mut UserBuffer<'_>,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<Option<usize>, SystemError> {
        if len < core::mem::size_of::<u64>() {
            return Err(SystemError::EINVAL);
        }
        let nonblock = matches!(
            &*data,
            FilePrivateData::TimerFd(flags) if flags.contains(FileFlags::O_NONBLOCK)
        );
        drop(data);
        let ticks = self.read_ticks(nonblock)?;
        if ticks == 0 {
            return Ok(Some(0));
        }
        writer.write_one(0, &ticks)?;
        Ok(Some(8))
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> = Err(SystemError::EINVAL);
        result.map_err(Into::into)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(self.metadata.clone())
    }

    fn stat_mode(&self, metadata: &Metadata) -> InodeMode {
        metadata.mode
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        AnonInodeFs::instance()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        Err(SystemError::EINVAL)
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        Ok(self)
    }

    fn absolute_path(&self) -> Result<String, SystemError> {
        Ok(anon_inode_path("[timerfd]"))
    }
}

/// Notify timerfds that requested cancellation after a discontinuous wall-clock set.
/// The timekeeper write lock must not be held by the caller.
pub fn timerfd_clock_was_set() {
    let snapshot = {
        let mut registry = REALTIME_TIMERFDS.lock();
        registry.retain(|_, entry| entry.strong_count() != 0);
        registry.values().cloned().collect::<Vec<_>>()
    };
    for entry in snapshot {
        if let Some(inode) = entry.upgrade() {
            inode.clock_was_set();
        }
    }
}
