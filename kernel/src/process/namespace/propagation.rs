//! Linux-compatible mount propagation for mount namespaces.
//!
//! The implementation is split by responsibility: peer-group resources,
//! per-mount state, atomic propagation-type changes, and topology events.
//! Linux 6.6 `fs/pnode.c` and `fs/namespace.c` define the reference semantics.

mod change;
mod event;
mod group;
mod state;

#[cfg(test)]
mod tests;

pub use change::{
    change_mnt_propagation_recursive, flags_to_propagation_type, is_propagation_change,
};
#[allow(unused_imports)]
pub(crate) use event::{
    abort_mount_propagation, commit_mount_propagation_locked, ensure_subtree_shared,
    prepare_mount_propagation_locked, propagate_moved_tree_locked, PreparedPropagation,
};
pub use event::{propagate_umount, propagation_umount_busy};
#[allow(unused_imports)]
pub use group::{register_peer, PropagationGroupId};
pub(crate) use state::detach_mount_propagation;
#[allow(unused_imports)]
pub use state::{
    inherit_bind_mount_propagation, register_slave_with_master, MountPropagation, PropagationType,
};
