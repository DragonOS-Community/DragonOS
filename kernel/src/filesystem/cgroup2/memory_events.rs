//! Per-open-description memory event observation and deferred epoll delivery.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use system_error::SystemError;

use crate::{
    cgroup::memory::MemoryAccount,
    filesystem::epoll::{
        event_poll::{EPollItemList, EventPoll},
        EPollEventType, EPollItem,
    },
    libs::mutex::Mutex,
    mm::memory_notify::weak_registry::WeakRegistry,
};

static WATCHERS: WeakRegistry<MemoryEventOpen> = WeakRegistry::new();

#[derive(Debug)]
pub(crate) struct MemoryEventOpen {
    pub(super) account: Arc<MemoryAccount>,
    pub(super) local: bool,
    seen: AtomicU64,
    closed: AtomicBool,
    registration: AtomicUsize,
    epitems: EPollItemList,
    snapshot: Mutex<Option<alloc::vec::Vec<u8>>>,
}

impl MemoryEventOpen {
    pub(super) fn new(account: Arc<MemoryAccount>, local: bool) -> Result<Arc<Self>, SystemError> {
        // kernfs reports the initial notification until the first read.
        let seen = account.event_sequence(local).wrapping_sub(1);
        let state = Arc::try_new(Self {
            account,
            local,
            seen: AtomicU64::new(seen),
            closed: AtomicBool::new(false),
            registration: AtomicUsize::new(0),
            epitems: EPollItemList::default(),
            snapshot: Mutex::new(None),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        WATCHERS.register(&state, |id| state.registration.store(id, Ordering::Release))?;
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
            let current = cgroup
                .local_memory_generation()
                .ok_or(SystemError::ENODEV)?;
            if !Arc::ptr_eq(&current.account, &self.account) {
                return Err(SystemError::ENODEV);
            }
            (offset == 0 || stored.is_none()).then(|| self.account.snapshot())
        };
        if let Some(snapshot) = snapshot {
            let bytes = crate::cgroup::memory::stats::format_events(&snapshot, self.local)?;
            let sequence = if self.local {
                snapshot.local_event_sequence
            } else {
                snapshot.event_sequence
            };
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
            || !self.account.is_online()
            || self.seen.load(Ordering::Acquire) != self.account.event_sequence(self.local)
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
        WATCHERS.unregister(self.registration.swap(0, Ordering::AcqRel));
    }
}

impl Drop for MemoryEventOpen {
    fn drop(&mut self) {
        self.close();
    }
}

pub(crate) fn has_watchers() -> bool {
    WATCHERS.has_registrations()
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
