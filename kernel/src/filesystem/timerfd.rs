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
    arch::MMArch,
    filesystem::{
        epoll::{event_poll::EventPoll, EPollEventType, EPollItem},
        vfs::{
            file::{FileFlags, FilePrivateData},
            vcore::generate_inode_id,
            FileSystem, FileType, FsInfo, IndexNode, InodeFlags, InodeMode, Magic, Metadata,
            OpenFileBehavior, PollableInode, PostWriteSyncPolicy, SuperBlock,
        },
    },
    libs::{
        mutex::{Mutex, MutexGuard},
        spinlock::SpinLock,
        wait_queue::WaitQueue,
    },
    mm::MemoryManagementArch,
    process::{posix_timer::PosixItimerspec, ProcessManager},
    syscall::user_buffer::UserBuffer,
    time::{
        syscall::{posix_clock_now, PosixClockID},
        timekeeping::realtime_now_with_clock_set_seq,
        timer::{next_n_us_timer_jiffies, Timer, TimerFunction},
        PosixTimeSpec, NSEC_PER_SEC,
    },
};

use super::epoll::event_poll::EPollItemList;

const KTIME_MAX_NS: u64 = i64::MAX as u64;
const KTIME_SEC_MAX: i64 = i64::MAX / NSEC_PER_SEC as i64;

lazy_static::lazy_static! {
    static ref TIMERFD_FS: Arc<TimerFdFs> = Arc::new(TimerFdFs);
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

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum DeadlineDomain {
    Monotonic,
    Realtime,
    Boottime,
}

impl DeadlineDomain {
    fn now_ns(self) -> u64 {
        let clock = match self {
            Self::Monotonic => PosixClockID::Monotonic,
            Self::Realtime => PosixClockID::Realtime,
            Self::Boottime => PosixClockID::Boottime,
        };
        posix_clock_now(clock).to_ktime_ns()
    }
}

#[derive(Debug)]
struct TimerFdState {
    clock_id: PosixClockID,
    interval_ns: u64,
    next_expiry_ns: Option<u64>,
    deadline_domain: DeadlineDomain,
    timer: Option<Arc<Timer>>,
    ticks: u64,
    expired: bool,
    cancel_pending: bool,
    generation: u64,
    settime_flags: TimerFdSettimeFlags,
    observed_clock_set_seq: Option<u64>,
    registry_registered: bool,
    shutdown: bool,
}

impl TimerFdState {
    fn configured_realtime_absolute(&self) -> bool {
        matches!(
            self.clock_id,
            PosixClockID::Realtime | PosixClockID::RealtimeAlarm
        ) && self
            .settime_flags
            .contains(TimerFdSettimeFlags::TFD_TIMER_ABSTIME)
    }

    fn cancel_on_set(&self) -> bool {
        self.configured_realtime_absolute()
            && self
                .settime_flags
                .contains(TimerFdSettimeFlags::TFD_TIMER_CANCEL_ON_SET)
    }

    fn current_spec(&self) -> PosixItimerspec {
        let mut value = PosixTimeSpec::default();
        if let Some(mut deadline) = self.next_expiry_ns {
            let now = self.deadline_domain.now_ns();
            if self.expired && self.interval_ns != 0 && now >= deadline {
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
pub struct TimerFdFs;

impl TimerFdFs {
    fn instance() -> Arc<Self> {
        TIMERFD_FS.clone()
    }
}

impl FileSystem for TimerFdFs {
    fn page_cache_writeback_domain(
        &self,
    ) -> Option<&Arc<crate::filesystem::page_cache::PageCacheWritebackDomain>> {
        None
    }

    fn root_inode(&self) -> Arc<dyn IndexNode> {
        TimerFdInode::new(PosixClockID::Monotonic)
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
        "timerfd"
    }

    fn super_block(&self) -> SuperBlock {
        SuperBlock::new(
            Magic::EVENTFD_MAGIC,
            <MMArch as MemoryManagementArch>::PAGE_SIZE as u64,
            255,
        )
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
                settime_flags: TimerFdSettimeFlags::empty(),
                observed_clock_set_seq: None,
                registry_registered: false,
                shutdown: false,
            }),
            operation: Mutex::new(()),
            wait_queue: WaitQueue::default(),
            epitems: EPollItemList::default(),
            metadata: Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: PosixTimeSpec::default(),
                mtime: PosixTimeSpec::default(),
                ctime: PosixTimeSpec::default(),
                btime: PosixTimeSpec::default(),
                file_type: FileType::File,
                mode: InodeMode::from_bits_truncate(0o600),
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: Default::default(),
                flags: InodeFlags::empty(),
            },
            self_weak: weak.clone(),
        })
    }

    fn absolute_domain(clock_id: PosixClockID) -> DeadlineDomain {
        match clock_id {
            PosixClockID::Realtime | PosixClockID::RealtimeAlarm => DeadlineDomain::Realtime,
            PosixClockID::Boottime | PosixClockID::BoottimeAlarm => DeadlineDomain::Boottime,
            _ => DeadlineDomain::Monotonic,
        }
    }

    fn relative_domain(clock_id: PosixClockID) -> DeadlineDomain {
        match clock_id {
            PosixClockID::Boottime | PosixClockID::BoottimeAlarm => DeadlineDomain::Boottime,
            _ => DeadlineDomain::Monotonic,
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

    fn new_backend(&self, deadline_ns: u64, domain: DeadlineDomain, generation: u64) -> Arc<Timer> {
        let now = domain.now_ns();
        let remaining_us = deadline_ns.saturating_sub(now).div_ceil(1000);
        let expires = next_n_us_timer_jiffies(remaining_us);
        Timer::new(
            Box::new(TimerFdExpiry {
                inode: self.self_weak.clone(),
                generation,
            }),
            expires,
        )
    }

    fn publish_backend(&self, backend: Arc<Timer>, generation: u64) -> bool {
        let mut state = self.state.lock_irqsave();
        if state.shutdown || state.generation != generation || state.next_expiry_ns.is_none() {
            return false;
        }
        state.timer = Some(backend);
        true
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

    fn prepare_periodic_rearm(&self) -> Option<(u64, u64)> {
        let needs_registration = {
            let state = self.state.lock_irqsave();
            state.expired
                && state.interval_ns != 0
                && state.configured_realtime_absolute()
                && !state.registry_registered
        };
        if !needs_registration {
            return None;
        }

        // The caller holds operation. Register before observing realtime so a
        // concurrent clock set is either included in this snapshot or finds
        // this inode and waits for the rearm state to be published.
        self.registry_add();
        let (now, clock_set_seq) = realtime_now_with_clock_set_seq();
        Some((now.to_ktime_ns(), clock_set_seq))
    }

    fn advance_periodic_locked(
        state: &mut TimerFdState,
        registration: Option<(u64, u64)>,
    ) -> Option<(u64, DeadlineDomain, u64)> {
        if !state.expired || state.interval_ns == 0 {
            return None;
        }
        if let Some((_, clock_set_seq)) = registration {
            debug_assert!(state.configured_realtime_absolute());
            debug_assert!(!state.registry_registered);
            state.registry_registered = true;
            state.observed_clock_set_seq = Some(clock_set_seq);
        }
        let deadline = state.next_expiry_ns?;
        let now = registration
            .map(|(now, _)| now)
            .unwrap_or_else(|| state.deadline_domain.now_ns());
        let next = if now >= deadline {
            let periods = (now - deadline) / state.interval_ns + 1;
            state.ticks = state.ticks.wrapping_add(periods - 1);
            deadline
                .saturating_add(state.interval_ns.saturating_mul(periods))
                .min(KTIME_MAX_NS)
        } else {
            // Match Linux hrtimer_forward_now(): after a realtime step back
            // before the old expiry, it returns 0 and timerfd's unsigned
            // `ticks += 0 - 1` retracts the lazy callback contribution before
            // the original deadline is rearmed.
            state.ticks = state.ticks.wrapping_sub(1);
            deadline
        };
        state.expired = false;
        state.next_expiry_ns = Some(next);
        Some((next, state.deadline_domain, state.generation))
    }

    fn rearm_periodic(&self, rearm: Option<(u64, DeadlineDomain, u64)>) -> Result<(), SystemError> {
        let Some((deadline, domain, generation)) = rearm else {
            return Ok(());
        };
        let backend = self.new_backend(deadline, domain, generation);
        if self.publish_backend(backend.clone(), generation) {
            backend.activate();
        }
        Ok(())
    }

    fn read_ticks(&self, nonblock: bool) -> Result<u64, SystemError> {
        loop {
            if !self.has_ticks() {
                if nonblock {
                    let _operation = self.operation.lock();
                    let mut state = self.state.lock_irqsave();
                    if state.cancel_pending {
                        Self::consume_cancel_locked(&mut state);
                        return Err(SystemError::ECANCELED);
                    }
                    if state.ticks == 0 {
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    }
                } else {
                    if ProcessManager::current_pcb().has_pending_signal_fast() {
                        return Err(SystemError::ERESTARTSYS);
                    }
                    wq_wait_event_interruptible!(self.wait_queue, self.has_ticks(), {})?;
                }
            }

            let _operation = self.operation.lock();
            let registration = self.prepare_periodic_rearm();
            let mut state = self.state.lock_irqsave();
            if state.cancel_pending {
                Self::consume_cancel_locked(&mut state);
                return Err(SystemError::ECANCELED);
            }
            if state.ticks == 0 {
                continue;
            }
            let rearm = Self::advance_periodic_locked(&mut state, registration);
            let ticks = state.ticks;
            state.ticks = 0;
            state.expired = false;
            let unregister = state.interval_ns == 0
                && state.registry_registered
                && state.configured_realtime_absolute()
                && !state.cancel_on_set();
            if state.interval_ns == 0 {
                // A consumed one-shot is logically disarmed.  Keeping its old
                // deadline would let a later wall-clock set fire it again.
                state.next_expiry_ns = None;
                if unregister {
                    state.registry_registered = false;
                }
            }
            drop(state);
            if unregister {
                self.registry_remove();
            }
            self.rearm_periodic(rearm)?;
            if ticks == 0 && nonblock {
                // Linux leaves the preinitialized nonblocking result at
                // -EAGAIN when hrtimer_forward_now()==0 retracts the sole
                // lazy expiration after a realtime step backwards.
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            return Ok(ticks);
        }
    }

    pub fn gettime(&self) -> Result<PosixItimerspec, SystemError> {
        let _operation = self.operation.lock();
        let registration = self.prepare_periodic_rearm();
        let mut state = self.state.lock_irqsave();
        let rearm = Self::advance_periodic_locked(&mut state, registration);
        let value = state.current_spec();
        drop(state);
        self.rearm_periodic(rearm)?;
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

        let (clock_id, registry_registered) = {
            let state = self.state.lock_irqsave();
            (state.clock_id, state.registry_registered)
        };
        let realtime_absolute = Self::is_realtime_absolute(clock_id, flags);
        let registry_candidate = realtime_absolute
            && (initial_ns != 0 || flags.contains(TimerFdSettimeFlags::TFD_TIMER_CANCEL_ON_SET));
        if registry_candidate && !registry_registered {
            self.registry_add();
        }

        let domain = if flags.contains(TimerFdSettimeFlags::TFD_TIMER_ABSTIME) {
            Self::absolute_domain(clock_id)
        } else {
            Self::relative_domain(clock_id)
        };
        let (now, observed_clock_set_seq) = if domain == DeadlineDomain::Realtime
            && flags.contains(TimerFdSettimeFlags::TFD_TIMER_ABSTIME)
        {
            let (now, seq) = realtime_now_with_clock_set_seq();
            (now.to_ktime_ns(), Some(seq))
        } else {
            (domain.now_ns(), None)
        };
        let deadline = if initial_ns == 0 {
            None
        } else if flags.contains(TimerFdSettimeFlags::TFD_TIMER_ABSTIME) {
            Some(initial_ns)
        } else {
            Some(now.saturating_add(initial_ns).min(KTIME_MAX_NS))
        };
        let immediate = deadline.is_some_and(|expiry| expiry <= now);
        let registry_member = registry_candidate
            && (!immediate || flags.contains(TimerFdSettimeFlags::TFD_TIMER_CANCEL_ON_SET));

        let (old, old_backend, generation, report_canceled) = {
            let mut state = self.state.lock_irqsave();
            let old = state.current_spec();
            let old_backend = state.timer.take();
            let old_cancel = state.cancel_pending;
            state.generation = state.generation.wrapping_add(1);
            let generation = state.generation;
            state.interval_ns = interval_ns;
            state.next_expiry_ns = deadline;
            state.deadline_domain = domain;
            state.ticks = 0;
            state.expired = false;
            state.settime_flags = flags;
            state.observed_clock_set_seq = observed_clock_set_seq;
            state.registry_registered = registry_member;
            state.cancel_pending = old_cancel
                && realtime_absolute
                && flags.contains(TimerFdSettimeFlags::TFD_TIMER_CANCEL_ON_SET);
            if immediate {
                state.expired = true;
                state.ticks = 1;
            }
            let report_canceled = initial_ns != 0 && state.cancel_pending;
            if report_canceled {
                state.cancel_pending = false;
            }
            (old, old_backend, generation, report_canceled)
        };

        if let Some(old_backend) = old_backend {
            old_backend.cancel();
        }
        if !registry_member && (registry_registered || registry_candidate) {
            self.registry_remove();
        }
        if let Some(deadline) = deadline.filter(|_| !immediate) {
            let backend = self.new_backend(deadline, domain, generation);
            if self.publish_backend(backend.clone(), generation) {
                backend.activate();
            }
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
        let (realtime_now, clock_set_seq) = realtime_now_with_clock_set_seq();
        let (old_backend, deadline, domain, generation, notify) = {
            let mut state = self.state.lock_irqsave();
            if !state.registry_registered {
                return;
            }
            if state.shutdown || !state.configured_realtime_absolute() {
                return;
            }
            if state.observed_clock_set_seq == Some(clock_set_seq) {
                return;
            }
            state.observed_clock_set_seq = Some(clock_set_seq);
            let notify = state.cancel_on_set();
            if notify {
                state.cancel_pending = true;
                state.ticks = state.ticks.wrapping_add(1);
            }
            let Some(deadline) = state.next_expiry_ns else {
                drop(state);
                if notify {
                    self.notify_readable();
                }
                return;
            };
            // A lazy periodic/one-shot expiry has already consumed its
            // backend. read/gettime owns any subsequent periodic forward and
            // rearm; installing another backend here would duplicate it.
            if state.expired {
                let unregister = state.registry_registered && !notify;
                if unregister {
                    state.registry_registered = false;
                }
                drop(state);
                if unregister {
                    self.registry_remove();
                }
                if notify {
                    self.notify_readable();
                }
                return;
            }
            state.generation = state.generation.wrapping_add(1);
            let generation = state.generation;
            let old_backend = state.timer.take();
            (
                old_backend,
                deadline,
                state.deadline_domain,
                generation,
                notify,
            )
        };

        if let Some(old_backend) = old_backend {
            old_backend.cancel();
        }
        let now = realtime_now.to_ktime_ns();
        if now >= deadline {
            let mut state = self.state.lock_irqsave();
            let unregister = if !state.shutdown && state.generation == generation {
                state.expired = true;
                state.ticks = state.ticks.wrapping_add(1);
                let unregister = state.registry_registered && !notify;
                if unregister {
                    state.registry_registered = false;
                }
                unregister
            } else {
                false
            };
            drop(state);
            if unregister {
                self.registry_remove();
            }
            self.notify_readable();
        } else {
            let backend = self.new_backend(deadline, domain, generation);
            if self.publish_backend(backend.clone(), generation) {
                backend.activate();
            }
            if notify {
                self.notify_readable();
            }
        }
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
        let deadline = state.next_expiry_ns.unwrap();
        let now = state.deadline_domain.now_ns();
        state.timer = None;
        if now < deadline {
            return Ok(());
        }
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
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        let _operation = self.operation.lock();
        let (backend, registry_registered) = {
            let mut state = self.state.lock_irqsave();
            state.shutdown = true;
            state.generation = state.generation.wrapping_add(1);
            state.next_expiry_ns = None;
            let registry_registered = state.registry_registered;
            state.registry_registered = false;
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
    ) -> Result<usize, SystemError> {
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
    }

    fn supports_read_user(&self) -> bool {
        true
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
    ) -> Result<usize, SystemError> {
        Err(SystemError::EINVAL)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(self.metadata.clone())
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        TimerFdFs::instance()
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
        Ok(String::from("timerfd"))
    }
}

/// Notify all realtime-absolute timerfds after a discontinuous wall-clock set.
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
