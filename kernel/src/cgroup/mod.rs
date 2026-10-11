pub mod controllers;
pub mod core;
pub(crate) mod cpu;
pub(crate) mod cpuset;
pub(crate) mod threaded;

use crate::libs::mutex::{Mutex, MutexGuard};

static UPDATE_LOCK: Mutex<()> = Mutex::new(());

/// Serialize hierarchy, controller configuration and membership publication.
/// Take before hierarchy/accounting/pi/rq locks, never from interrupt context.
pub(crate) fn lock() -> MutexGuard<'static, ()> {
    UPDATE_LOCK.lock()
}

#[allow(unused_imports)]
pub use controllers::{CgroupCpuState, CgroupFreezerState, CgroupMemoryState};
#[allow(unused_imports)]
pub use core::{
    cgroup_accounting_lock, cgroup_can_fork_in, cgroup_common_ancestor, cgroup_migrate_vet_dst,
    cgroup_path_from_view, cgroup_root, cgroup_root_node, find_node_by_abs_path,
    find_or_create_node_by_abs_path, CgroupNode, CgroupRoot, TaskCgroupRef,
};
