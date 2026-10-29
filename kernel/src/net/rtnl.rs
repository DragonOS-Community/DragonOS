//! Global serialization for the network control plane.
//!
//! RTNL is a sleeping lock. It may only be acquired from process-context
//! control paths; hardirq, NAPI, and packet data paths must never acquire it.
//!
//! Lock ordering for control operations starts with RTNL, followed by netns
//! topology, interface/control state, data-plane state, and finally netlink
//! notification queues. Notification helpers must not acquire RTNL
//! recursively.

use crate::libs::mutex::{Mutex, MutexGuard};

static RTNL_MUTEX: Mutex<()> = Mutex::new(());

/// RAII proof that the global network control-plane lock is held.
#[must_use = "the RTNL guard must be held for the complete control operation"]
pub(crate) struct RtnlGuard {
    _guard: MutexGuard<'static, ()>,
}

/// Acquires the global network control-plane lock.
pub(crate) fn lock() -> RtnlGuard {
    RtnlGuard {
        _guard: RTNL_MUTEX.lock(),
    }
}
