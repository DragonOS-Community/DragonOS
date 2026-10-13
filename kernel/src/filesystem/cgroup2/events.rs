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
    snapshot: Mutex<Option<alloc::vec::Vec<u8>>>,
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
            snapshot: Mutex::new(None),
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
        // Preserve one generated record across partial reads and EOF. A new
        // read from offset zero (including after lseek) regenerates it, like
        // kernfs's single seq-file record; EOF must not consume a newer event.
        let snapshot = {
            let _update = crate::cgroup::lock();
            super::files::check_live_file(cgroup, ty, generation)?;
            match &self.source {
                EventSource::Memory { account, local } => {
                    let current = cgroup
                        .local_memory_generation()
                        .ok_or(SystemError::ENODEV)?;
                    if !Arc::ptr_eq(&current.account, account) {
                        return Err(SystemError::ENODEV);
                    }
                    if offset == 0 || stored.is_none() {
                        (Some(account.snapshot()), None, *local)
                    } else {
                        (None, None, *local)
                    }
                }
                EventSource::Core(node) => {
                    if !Arc::ptr_eq(cgroup, node) {
                        return Err(SystemError::ENODEV);
                    }
                    if offset == 0 || stored.is_none() {
                        let (populated, frozen, sequence) =
                            crate::cgroup::freezer::snapshot_events(node);
                        (None, Some((populated, frozen, sequence)), false)
                    } else {
                        (None, None, false)
                    }
                }
            }
        };
        // Rendering may allocate; keep it outside UPDATE and STATE just
        // as the original memory-events implementation did. No new heap
        // allocation is required to hold either kind of numeric snapshot.
        let record = if let Some(memory) = snapshot.0 {
            Some((
                crate::cgroup::memory::stats::format_events(&memory, snapshot.2)?,
                if snapshot.2 {
                    memory.local_event_sequence
                } else {
                    memory.event_sequence
                },
            ))
        } else {
            snapshot.1.map(|(populated, frozen, sequence)| {
                (
                    alloc::format!(
                        "populated {}\nfrozen {}\n",
                        u8::from(populated),
                        u8::from(frozen)
                    )
                    .into_bytes(),
                    sequence,
                )
            })
        };
        if let Some((bytes, sequence)) = record {
            *stored = Some(bytes);
            self.seen.store(sequence, Ordering::Release);
        }
        let bytes = stored.as_ref().expect("event snapshot was initialized");
        let start = offset.min(bytes.len());
        let count = len.min(buf.len()).min(bytes.len() - start);
        buf[..count].copy_from_slice(&bytes[start..start + count]);
        Ok(count)
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
