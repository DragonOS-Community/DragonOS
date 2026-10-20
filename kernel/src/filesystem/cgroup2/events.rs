//! Per-open-description cgroup event observation and deferred epoll delivery.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use system_error::SystemError;

use crate::{
    filesystem::epoll::{
        event_poll::{EPollItemList, EventPoll},
        EPollEventType, EPollItem,
    },
    libs::mutex::Mutex,
    mm::memory_notify::weak_registry::WeakRegistry,
};

use super::event_notify::EventSource;

static WATCHERS: WeakRegistry<CgroupEventOpen> = WeakRegistry::new();
static MEMORY_WATCHERS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
pub(crate) struct CgroupEventOpen {
    source: EventSource,
    seen: AtomicU64,
    closed: AtomicBool,
    registration: AtomicUsize,
    memory_registered: AtomicBool,
    epitems: EPollItemList,
    snapshot: Mutex<super::read_snapshot::TextSnapshot>,
}

impl CgroupEventOpen {
    pub(super) fn new(source: EventSource) -> Result<Arc<Self>, SystemError> {
        // kernfs reports the initial notification until the first read.
        let seen = source.sequence().wrapping_sub(1);
        let state = Arc::try_new(Self {
            source,
            seen: AtomicU64::new(seen),
            closed: AtomicBool::new(false),
            registration: AtomicUsize::new(0),
            memory_registered: AtomicBool::new(false),
            epitems: EPollItemList::default(),
            snapshot: Mutex::new(super::read_snapshot::TextSnapshot::default()),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        WATCHERS.register(&state, |id| state.registration.store(id, Ordering::Release))?;
        if matches!(state.source, EventSource::Memory { .. }) {
            // Pair admission with request_oom_progress's full barrier before
            // its lockless empty check, like the registry's admission barrier.
            MEMORY_WATCHERS.fetch_add(1, Ordering::SeqCst);
            state.memory_registered.store(true, Ordering::Release);
        }
        Ok(state)
    }

    pub(super) fn read(
        &self,
        cgroup: &Arc<crate::cgroup::CgroupNode>,
        ty: super::files::CgroupCoreFile,
        generation: u64,
        offset: usize,
        len: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        let mut stored = self.snapshot.lock();
        let mut rendered_sequence = None;
        let count = stored.read(
            offset,
            len,
            buf,
            || {
                let (bytes, sequence) = self.render(cgroup, ty, generation)?;
                rendered_sequence = Some(sequence);
                Ok(bytes)
            },
            || {
                let _update = crate::cgroup::lock();
                self.check_source(cgroup, ty, generation)
            },
        )?;
        // Acknowledge only a successfully committed record, not a cached tail,
        // an empty request, continuous EOF, or failed rendering. Notifications racing with
        // formatting remain pending because this is the captured sequence.
        if let Some(sequence) = rendered_sequence {
            self.seen.store(sequence, Ordering::Release);
        }
        Ok(count)
    }

    /// Caller holds UPDATE; identity must stay bound to the original epoch.
    fn check_source(
        &self,
        cgroup: &Arc<crate::cgroup::CgroupNode>,
        ty: super::files::CgroupCoreFile,
        generation: u64,
    ) -> Result<(), SystemError> {
        super::files::check_live_file(cgroup, ty, generation)?;
        match &self.source {
            EventSource::Memory { account, .. } => {
                let current = cgroup
                    .local_memory_generation()
                    .ok_or(SystemError::ENODEV)?;
                if !Arc::ptr_eq(&current.account, account) {
                    return Err(SystemError::ENODEV);
                }
            }
            EventSource::Core(node) => {
                if !Arc::ptr_eq(cgroup, node) {
                    return Err(SystemError::ENODEV);
                }
            }
        }
        Ok(())
    }

    fn render(
        &self,
        cgroup: &Arc<crate::cgroup::CgroupNode>,
        ty: super::files::CgroupCoreFile,
        generation: u64,
    ) -> Result<(alloc::vec::Vec<u8>, u64), SystemError> {
        let snapshot = {
            let _update = crate::cgroup::lock();
            self.check_source(cgroup, ty, generation)?;
            match &self.source {
                EventSource::Memory { account, local } => (Some(account.snapshot()), None, *local),
                EventSource::Core(node) => (
                    None,
                    Some(crate::cgroup::freezer::snapshot_events(node)),
                    false,
                ),
            }
        };
        // Numeric capture and source identity are atomic under UPDATE; formatting
        // may allocate and must run after releasing it.
        if let Some(memory) = snapshot.0 {
            Ok((
                crate::cgroup::memory::stats::format_events(&memory, snapshot.2)?,
                if snapshot.2 {
                    memory.local_event_sequence
                } else {
                    memory.event_sequence
                },
            ))
        } else {
            let (populated, frozen, sequence) = snapshot.1.expect("core event snapshot");
            Ok((
                alloc::format!(
                    "populated {}\nfrozen {}\n",
                    u8::from(populated),
                    u8::from(frozen)
                )
                .into_bytes(),
                sequence,
            ))
        }
    }

    pub(super) fn poll(&self) -> EPollEventType {
        let mut flags = default_mask();
        if self.closed.load(Ordering::Acquire)
            || !self.source.is_online()
            || self.seen.load(Ordering::Acquire) != self.source.sequence()
        {
            flags |= EPollEventType::EPOLLPRI | EPollEventType::EPOLLERR;
        }
        flags
    }

    pub(super) fn add(&self, item: Arc<EPollItem>) {
        self.epitems.add(item);
    }

    pub(super) fn remove(&self, item: &Arc<EPollItem>) -> Result<(), SystemError> {
        self.epitems.remove(item)
    }

    pub(super) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        if self.memory_registered.swap(false, Ordering::AcqRel) {
            let old = MEMORY_WATCHERS.fetch_sub(1, Ordering::AcqRel);
            debug_assert_ne!(old, 0, "memory event observation underflow");
        }
        WATCHERS.unregister(self.registration.swap(0, Ordering::AcqRel));
    }
}

impl Drop for CgroupEventOpen {
    fn drop(&mut self) {
        self.close();
    }
}

pub(crate) fn has_watchers() -> bool {
    WATCHERS.has_registrations()
}

pub(crate) fn has_memory_watchers() -> bool {
    MEMORY_WATCHERS.load(Ordering::Acquire) != 0
}

pub(super) fn default_mask() -> EPollEventType {
    EPollEventType::EPOLLIN
        | EPollEventType::EPOLLOUT
        | EPollEventType::EPOLLRDNORM
        | EPollEventType::EPOLLWRNORM
}

/// Invoked only by the permanently pinned memory notifier in task context.
pub(crate) fn notify() {
    WATCHERS.for_each(
        |state| {
            if !state.closed.load(Ordering::Acquire) {
                let flags = state.poll();
                if flags.contains(EPollEventType::EPOLLPRI) {
                    let _ = EventPoll::wakeup_epoll(&state.epitems, flags);
                }
            }
        },
        |state| !state.closed.load(Ordering::Acquire),
    );
}
