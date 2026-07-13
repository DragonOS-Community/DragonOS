//! Mount propagation management for mount namespace.
//!
//! This module implements mount propagation semantics similar to Linux,
//! supporting shared, private, slave, and unbindable propagation types.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    PeerGroupRegistry                        │
//! │  ┌─────────────────────────────────────────────────────┐    │
//! │  │ Group 1: [Mount A, Mount B, Mount C]                │    │
//! │  │ Group 2: [Mount D, Mount E]                         │    │
//! │  │ ...                                                 │    │
//! │  └─────────────────────────────────────────────────────┘    │
//! └─────────────────────────────────────────────────────────────┘
//!                           │
//!                           ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │                   MountPropagation                          │
//! │  - flags: shared/unbindable flags                           │
//! │  - peer_group_id: PropagationGroupId                        │
//! │  - master/slaves relationships (slave state)                 │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! Reference: https://www.kernel.org/doc/Documentation/filesystems/sharedsubtree.txt

use alloc::collections::{btree_map::Entry, BTreeMap, BTreeSet};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use hashbrown::HashSet;
use ida::IdAllocator;
use system_error::SystemError;

use crate::filesystem::vfs::{
    mount::{MountFlags, MountPath, MOUNT_LIFECYCLE_LOCK},
    MountFS,
};
use crate::libs::{mutex::MutexGuard, rwlock::RwLock, spinlock::SpinLock};

// ============================================================================
// PropagationGroupId
// ============================================================================

/// Linux exposes peer group IDs as positive signed integers in mountinfo.
const PROPAGATION_GROUP_ID_END: usize = i32::MAX as usize + 1;

struct PropagationGroupIdAllocator {
    ida: IdAllocator,
    next_fresh: usize,
    /// `IdAllocator` normally continues searching from its cursor. Keep freed
    /// holes explicitly so peer group IDs have Linux IDA-style reuse behavior.
    reusable: BTreeSet<usize>,
}

impl PropagationGroupIdAllocator {
    const fn new() -> Self {
        Self {
            ida: IdAllocator::new(1, PROPAGATION_GROUP_ID_END).unwrap(),
            next_fresh: 1,
            reusable: BTreeSet::new(),
        }
    }

    fn alloc(&mut self) -> Option<usize> {
        if let Some(id) = self.reusable.iter().next().copied() {
            self.reusable.remove(&id);
            return self.ida.alloc_specific(id);
        }
        if self.next_fresh >= PROPAGATION_GROUP_ID_END {
            return None;
        }
        let id = self.next_fresh;
        let allocated = self.ida.alloc_specific(id);
        if allocated.is_some() {
            self.next_fresh += 1;
        }
        allocated
    }

    fn free(&mut self, id: usize) {
        self.ida.free(id);
        self.reusable.insert(id);
    }
}

static PROPAGATION_GROUP_ID_ALLOCATOR: SpinLock<PropagationGroupIdAllocator> =
    SpinLock::new(PropagationGroupIdAllocator::new());

int_like!(PropagationGroupId, usize);

impl PropagationGroupId {
    /// Invalid/unset group ID
    pub const NONE: Self = PropagationGroupId(0);

    /// Check if this is a valid (non-zero) group ID
    pub fn is_valid(&self) -> bool {
        self.0 != 0
    }
}

/// Ref-counted ownership of one allocated peer group ID.
///
/// Every live shared mount and every pending propagation transaction keeps an
/// `Arc` to this object. The ID is reusable only after the final owner drops.
#[derive(Debug)]
pub(crate) struct PropagationGroup {
    id: PropagationGroupId,
}

impl PropagationGroup {
    fn alloc() -> Result<Arc<Self>, SystemError> {
        let id = PROPAGATION_GROUP_ID_ALLOCATOR
            .lock()
            .alloc()
            .ok_or(SystemError::ENOSPC)?;
        Ok(Arc::new(Self {
            id: PropagationGroupId(id),
        }))
    }

    #[inline]
    fn id(&self) -> PropagationGroupId {
        self.id
    }
}

impl Drop for PropagationGroup {
    fn drop(&mut self) {
        PROPAGATION_GROUP_ID_ALLOCATOR.lock().free(self.id.0);
    }
}

// ============================================================================
// PeerGroupRegistry - Centralized Peer Group Management
// ============================================================================

/// Global peer group registry instance.
static PEER_GROUP_REGISTRY: PeerGroupRegistry = PeerGroupRegistry::new();

/// Manages peer group relationships for mount propagation.
///
/// This registry maintains a mapping from group IDs to the set of mounts
/// that belong to each peer group. When a mount event occurs on a shared
/// mount, this registry is used to find all peers that should receive
/// the propagated event.
///
/// # Thread Safety
///
/// The registry uses `RwLock` to allow concurrent reads while ensuring
/// exclusive access for writes. Weak references are used to avoid preventing
/// mount cleanup.
///
/// # Example
///
/// ```text
/// Peer Group 42:
///   ┌─────────┐     ┌─────────┐     ┌─────────┐
///   │ Mount A │ ◄──►│ Mount B │ ◄──►│ Mount C │
///   │ NS: 1   │     │ NS: 2   │     │ NS: 1   │
///   └─────────┘     └─────────┘     └─────────┘
///        │               │               │
///        └───────────────┴───────────────┘
///                   Peer Group
/// ```
pub struct PeerGroupRegistry {
    /// Maps group ID to weak references of mounts in that group.
    /// Using Weak<MountFS> to avoid preventing mount cleanup.
    inner: RwLock<BTreeMap<usize, Vec<Weak<MountFS>>>>,
}

impl PeerGroupRegistry {
    #[inline]
    fn is_current_member(mount: &Arc<MountFS>, group_id: PropagationGroupId) -> bool {
        let propagation = mount.propagation();
        propagation.is_shared() && propagation.peer_group_id() == group_id
    }

    /// Create a new empty registry.
    const fn new() -> Self {
        Self {
            inner: RwLock::new(BTreeMap::new()),
        }
    }

    /// Register a mount in a peer group.
    ///
    /// This adds the mount to the registry so it can receive propagated events.
    /// If the mount is already registered, this is a no-op.
    ///
    /// # Arguments
    /// * `group_id` - The peer group to join
    /// * `mount` - The mount to register
    pub fn register(&self, group_id: PropagationGroupId, mount: &Arc<MountFS>) {
        if !group_id.is_valid() {
            return;
        }

        let mut registry = self.inner.write();
        let peers = registry.entry(group_id.0).or_default();

        // Clean up stale references and check if already registered
        peers.retain(|w| {
            w.upgrade()
                .is_some_and(|m| !Arc::ptr_eq(&m, mount) && Self::is_current_member(&m, group_id))
        });

        // Add new peer
        peers.push(Arc::downgrade(mount));

        // log::debug!(
        //     "PeerGroupRegistry::register: mount added to group {}, total peers: {}",
        //     group_id.0,
        //     peers.len()
        // );
    }

    /// Unregister a mount from its peer group.
    ///
    /// This removes the mount from the registry. If the group becomes empty,
    /// the group entry is removed entirely.
    ///
    /// # Arguments
    /// * `group_id` - The peer group to leave
    /// * `mount` - The mount to unregister
    pub fn unregister(&self, group_id: PropagationGroupId, mount: &Arc<MountFS>) {
        if !group_id.is_valid() {
            return;
        }

        let mut registry = self.inner.write();
        if let Some(peers) = registry.get_mut(&group_id.0) {
            peers.retain(|w| {
                w.upgrade().is_some_and(|m| {
                    !Arc::ptr_eq(&m, mount) && Self::is_current_member(&m, group_id)
                })
            });

            // Remove empty groups to save memory
            if peers.is_empty() {
                registry.remove(&group_id.0);
                // log::debug!(
                //     "PeerGroupRegistry::unregister: group {} removed (empty)",
                //     group_id.0
                // );
            }
        }
    }

    /// Get all peers in a group, excluding the specified mount.
    ///
    /// This is typically used when propagating events - we want to send
    /// to all peers except the source of the event.
    ///
    /// # Arguments
    /// * `group_id` - The peer group to query
    /// * `exclude` - The mount to exclude from results
    ///
    /// # Returns
    /// A vector of all active mounts in the group, excluding `exclude`.
    pub fn get_peers_excluding(
        &self,
        group_id: PropagationGroupId,
        exclude: &Arc<MountFS>,
    ) -> Vec<Arc<MountFS>> {
        if !group_id.is_valid() {
            return Vec::new();
        }

        let registry = self.inner.read();
        if let Some(peers) = registry.get(&group_id.0) {
            peers
                .iter()
                .filter_map(|w| w.upgrade())
                .filter(|m| !Arc::ptr_eq(m, exclude) && Self::is_current_member(m, group_id))
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Get all peers in a group.
    ///
    /// # Arguments
    /// * `group_id` - The peer group to query
    ///
    /// # Returns
    /// A vector of all active mounts in the group.
    pub fn get_all_peers(&self, group_id: PropagationGroupId) -> Vec<Arc<MountFS>> {
        if !group_id.is_valid() {
            return Vec::new();
        }

        let registry = self.inner.read();
        if let Some(peers) = registry.get(&group_id.0) {
            peers
                .iter()
                .filter_map(|w| w.upgrade())
                .filter(|mount| Self::is_current_member(mount, group_id))
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Get the number of active peers in a group.
    ///
    /// # Arguments
    /// * `group_id` - The peer group to query
    ///
    /// # Returns
    /// The number of active (non-stale) mounts in the group.
    #[allow(dead_code)]
    pub fn peer_count(&self, group_id: PropagationGroupId) -> usize {
        if !group_id.is_valid() {
            return 0;
        }

        let registry = self.inner.read();
        if let Some(peers) = registry.get(&group_id.0) {
            peers
                .iter()
                .filter_map(|w| w.upgrade())
                .filter(|mount| Self::is_current_member(mount, group_id))
                .count()
        } else {
            0
        }
    }

    /// Check if a group exists and has active members.
    ///
    /// # Arguments
    /// * `group_id` - The peer group to check
    ///
    /// # Returns
    /// `true` if the group exists and has at least one active member.
    #[allow(dead_code)]
    pub fn group_exists(&self, group_id: PropagationGroupId) -> bool {
        self.peer_count(group_id) > 0
    }

    /// Clean up stale references in a group.
    ///
    /// This removes any weak references that can no longer be upgraded.
    /// Called automatically during register/unregister, but can be called
    /// explicitly for maintenance.
    ///
    /// # Arguments
    /// * `group_id` - The peer group to clean up
    #[allow(dead_code)]
    pub fn cleanup_stale(&self, group_id: PropagationGroupId) {
        if !group_id.is_valid() {
            return;
        }

        let mut registry = self.inner.write();
        if let Some(peers) = registry.get_mut(&group_id.0) {
            let before = peers.len();
            peers.retain(|w| {
                w.upgrade()
                    .is_some_and(|mount| Self::is_current_member(&mount, group_id))
            });
            let after = peers.len();

            if before != after {
                log::debug!(
                    "PeerGroupRegistry::cleanup_stale: group {} cleaned {} stale refs",
                    group_id.0,
                    before - after
                );
            }

            if peers.is_empty() {
                registry.remove(&group_id.0);
            }
        }
    }
}

// ============================================================================
// Public API - Convenience functions that delegate to the global registry
// ============================================================================

/// Register a mount in a peer group.
///
/// This is a convenience function that delegates to the global `PeerGroupRegistry`.
#[inline]
pub fn register_peer(group_id: PropagationGroupId, mount: &Arc<MountFS>) {
    PEER_GROUP_REGISTRY.register(group_id, mount);
}

/// Unregister a mount from its peer group.
///
/// This is a convenience function that delegates to the global `PeerGroupRegistry`.
#[inline]
pub fn unregister_peer(group_id: PropagationGroupId, mount: &Arc<MountFS>) {
    PEER_GROUP_REGISTRY.unregister(group_id, mount);
}

/// Remove a mount from its live peer group before releasing the group owner.
/// Callers that mutate live topology must hold `MOUNT_LIFECYCLE_LOCK`.
pub(crate) fn leave_peer_group(mount: &Arc<MountFS>) {
    let propagation = mount.propagation();
    let group_id = propagation.peer_group_id();
    if !propagation.is_shared() || !group_id.is_valid() {
        return;
    }
    unregister_peer(group_id, mount);
    propagation.clear_shared();
    propagation.clear_group_id();
}

/// Detach a mount from the complete propagation graph before it leaves live
/// topology. This mirrors Linux `change_mnt_propagation(mnt, MS_PRIVATE)` in
/// `umount_tree()`: slaves are reparented or orphaned by `do_make_slave()`, the
/// mount leaves its master, and its peer-group owner is released.
pub(crate) fn detach_mount_propagation(mount: &Arc<MountFS>) {
    do_make_slave(mount);
    detach_from_master(mount);
    mount.propagation().set_private();
}

/// Get all peers in a group (excluding the given mount).
///
/// This is a convenience function that delegates to the global `PeerGroupRegistry`.
#[inline]
pub fn get_peers(group_id: PropagationGroupId, exclude: &Arc<MountFS>) -> Vec<Arc<MountFS>> {
    PEER_GROUP_REGISTRY.get_peers_excluding(group_id, exclude)
}

/// Get all peers in a group (including all mounts).
///
/// This is a convenience function that delegates to the global `PeerGroupRegistry`.
#[inline]
pub fn get_all_peers(group_id: PropagationGroupId) -> Vec<Arc<MountFS>> {
    PEER_GROUP_REGISTRY.get_all_peers(group_id)
}

bitflags! {
    /// Mount propagation flags.
    ///
    /// Linux treats shared and slave as orthogonal state: shared is a flag,
    /// while slave is represented by the presence of a master mount.  Keep the
    /// same model here so a mount can be both shared and slave.
    pub struct PropagationFlags: u32 {
        /// Mount events propagate bidirectionally with peers.
        const SHARED = 1 << 0;
        /// Mount cannot be bind mounted.
        const UNBINDABLE = 1 << 1;
    }
}

/// Defines requested propagation type for mount point change operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PropagationType {
    /// Mount events do not propagate to or from this mount (default)
    #[default]
    Private,
    /// Mount events propagate bidirectionally with other mounts in the same peer group
    Shared,
    /// Mount events propagate from the master mount to this slave mount (one-way)
    Slave,
    /// Mount cannot be bind mounted and events do not propagate
    Unbindable,
}

/// Manages mount propagation state and relationships for a single mount point.
///
/// This struct tracks how mount events (mount, unmount, remount) propagate between
/// mount points according to their propagation types.
pub struct MountPropagation {
    inner: SpinLock<MountPropagationInner>,
}

/// Inner state protected by lock
struct MountPropagationInner {
    /// Propagation flags. Slave state is represented by `master`.
    flags: PropagationFlags,
    /// Ref-counted peer group ownership for shared mounts.
    peer_group: Option<Arc<PropagationGroup>>,
    /// Reference to the master mount for slave mounts
    master: Option<Weak<MountFS>>,
    /// List of slave mounts that receive events from this mount
    slaves: Vec<Weak<MountFS>>,
}

impl MountPropagation {
    /// Create a new private propagation (default)
    pub fn new_private() -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(MountPropagationInner {
                flags: PropagationFlags::empty(),
                peer_group: None,
                master: None,
                slaves: Vec::new(),
            }),
        })
    }

    /// Create a new shared propagation with a newly allocated group ID
    #[cfg(test)]
    pub fn new_shared() -> Result<Arc<Self>, SystemError> {
        Ok(Arc::new(Self {
            inner: SpinLock::new(MountPropagationInner {
                flags: PropagationFlags::SHARED,
                peer_group: Some(PropagationGroup::alloc()?),
                master: None,
                slaves: Vec::new(),
            }),
        }))
    }

    /// Create a new shared propagation with an existing group owner.
    #[cfg(test)]
    pub(crate) fn new_shared_with_group(group: Arc<PropagationGroup>) -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(MountPropagationInner {
                flags: PropagationFlags::SHARED,
                peer_group: Some(group),
                master: None,
                slaves: Vec::new(),
            }),
        })
    }

    /// Create a new slave propagation
    pub fn new_slave(master: Weak<MountFS>) -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(MountPropagationInner {
                flags: PropagationFlags::empty(),
                peer_group: None,
                master: Some(master),
                slaves: Vec::new(),
            }),
        })
    }

    /// Create a new unbindable propagation
    pub fn new_unbindable() -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(MountPropagationInner {
                flags: PropagationFlags::UNBINDABLE,
                peer_group: None,
                master: None,
                slaves: Vec::new(),
            }),
        })
    }

    /// Get the current propagation type
    pub fn prop_type(&self) -> PropagationType {
        let inner = self.inner.lock();
        if inner.flags.contains(PropagationFlags::UNBINDABLE) {
            PropagationType::Unbindable
        } else if inner.flags.contains(PropagationFlags::SHARED) {
            PropagationType::Shared
        } else if inner.master.is_some() {
            PropagationType::Slave
        } else {
            PropagationType::Private
        }
    }

    /// Get the peer group ID (0 if not in a shared group)
    pub fn peer_group_id(&self) -> PropagationGroupId {
        self.inner
            .lock()
            .peer_group
            .as_ref()
            .map_or(PropagationGroupId::NONE, |group| group.id())
    }

    /// Clone the current group owner for propagation transactions and copies.
    pub(crate) fn peer_group(&self) -> Option<Arc<PropagationGroup>> {
        self.inner.lock().peer_group.clone()
    }

    /// Check if this mount is shared
    pub fn is_shared(&self) -> bool {
        self.inner.lock().flags.contains(PropagationFlags::SHARED)
    }

    /// Check if this mount is private
    pub fn is_private(&self) -> bool {
        let inner = self.inner.lock();
        !inner.flags.contains(PropagationFlags::SHARED)
            && !inner.flags.contains(PropagationFlags::UNBINDABLE)
            && inner.master.is_none()
    }

    /// Check if this mount is a slave
    pub fn is_slave(&self) -> bool {
        self.inner.lock().master.is_some()
    }

    /// Check if this mount is unbindable
    pub fn is_unbindable(&self) -> bool {
        self.inner
            .lock()
            .flags
            .contains(PropagationFlags::UNBINDABLE)
    }

    /// Get the master mount reference (for slave mounts)
    pub fn master(&self) -> Option<Arc<MountFS>> {
        self.inner.lock().master.as_ref().and_then(|w| w.upgrade())
    }

    /// Change propagation type to shared
    ///
    /// Allocates a new peer group ID if not already shared.
    pub fn set_shared(&self) -> Result<(), SystemError> {
        let mut inner = self.inner.lock();
        let new_group = if inner.peer_group.is_none() {
            Some(PropagationGroup::alloc()?)
        } else {
            None
        };
        inner.flags.remove(PropagationFlags::UNBINDABLE);
        if let Some(group) = new_group {
            inner.peer_group = Some(group);
        }
        inner.flags.insert(PropagationFlags::SHARED);
        Ok(())
    }

    /// Set shared with an existing group owner (used for propagation).
    pub(crate) fn set_shared_with_group(&self, group: Arc<PropagationGroup>) {
        let mut inner = self.inner.lock();
        inner.flags.remove(PropagationFlags::UNBINDABLE);
        inner.peer_group = Some(group);
        inner.flags.insert(PropagationFlags::SHARED);
    }

    /// Clear the shared flag without changing slave/master relationships.
    pub fn clear_shared(&self) {
        self.inner.lock().flags.remove(PropagationFlags::SHARED);
    }

    /// Clear the peer group ID.
    pub fn clear_group_id(&self) {
        self.inner.lock().peer_group = None;
    }

    /// Set the master mount. `Some` makes this mount a slave.
    pub fn set_master(&self, master: Option<Weak<MountFS>>) {
        self.inner.lock().master = master;
    }

    /// Change propagation type to private
    ///
    /// Disconnects from peer group and master relationships.
    pub fn set_private(&self) {
        let mut inner = self.inner.lock();
        inner
            .flags
            .remove(PropagationFlags::SHARED | PropagationFlags::UNBINDABLE);
        inner.peer_group = None;
        inner.master = None;
    }

    /// Change propagation type to slave
    ///
    /// If currently shared, becomes a slave of the peer group.
    /// This is typically used when doing `mount --make-slave`.
    pub fn set_slave(&self, master: Option<Weak<MountFS>>) {
        self.inner.lock().master = master;
    }

    /// Change propagation type to unbindable
    pub fn set_unbindable(&self) {
        let mut inner = self.inner.lock();
        inner.flags.remove(PropagationFlags::SHARED);
        inner.flags.insert(PropagationFlags::UNBINDABLE);
        inner.peer_group = None;
        inner.master = None;
    }

    /// Add a slave mount
    pub fn add_slave(&self, slave: Weak<MountFS>) {
        let mut inner = self.inner.lock();
        inner.slaves.retain(|s| {
            if s.upgrade().is_some() {
                !Weak::ptr_eq(s, &slave)
            } else {
                false
            }
        });
        inner.slaves.push(slave);
    }

    /// Remove a slave mount
    pub fn remove_slave(&self, slave: &Weak<MountFS>) {
        let mut inner = self.inner.lock();
        inner
            .slaves
            .retain(|s| s.upgrade().is_some() && !Weak::ptr_eq(s, slave));
    }

    /// Get all valid slave mounts
    pub fn slaves(&self) -> Vec<Arc<MountFS>> {
        let mut inner = self.inner.lock();
        inner.slaves.retain(|s| s.upgrade().is_some());
        inner.slaves.iter().filter_map(|s| s.upgrade()).collect()
    }

    /// Clean up stale (dropped) slave references
    pub fn cleanup_stale_slaves(&self) {
        let mut inner = self.inner.lock();
        inner.slaves.retain(|s| s.upgrade().is_some());
    }

    /// Take all currently registered slaves, leaving the slave list empty.
    pub fn take_slaves(&self) -> Vec<Arc<MountFS>> {
        let mut inner = self.inner.lock();
        let slaves = inner.slaves.iter().filter_map(|s| s.upgrade()).collect();
        inner.slaves.clear();
        slaves
    }

    /// Clone the propagation state for a new mount copy.
    ///
    /// When copying a mount (e.g., for namespace cloning), the new mount
    /// should inherit the propagation type but may need different relationships.
    pub fn clone_for_copy(&self) -> Arc<Self> {
        let inner = self.inner.lock();
        Arc::new(Self {
            inner: SpinLock::new(MountPropagationInner {
                flags: inner.flags,
                peer_group: inner.peer_group.clone(),
                master: inner.master.clone(),
                slaves: Vec::new(), // New copy starts with no slaves
            }),
        })
    }

    /// Get propagation info string for /proc/self/mountinfo format
    ///
    /// Returns a string like "shared:1" or "master:2" or empty for private.
    pub fn info_string(&self) -> alloc::string::String {
        self.proc_mountinfo_tags()
    }

    /// Optional mountinfo fields: `shared:N`, `master:N`, `propagate_from:N`, `unbindable`.
    pub fn proc_mountinfo_tags(&self) -> alloc::string::String {
        let inner = self.inner.lock();
        let mut parts = Vec::new();
        if inner.flags.contains(PropagationFlags::SHARED) {
            if let Some(group) = inner.peer_group.as_ref() {
                parts.push(alloc::format!("shared:{}", group.id().0));
            }
        }
        if let Some(master) = inner.master.as_ref().and_then(|w| w.upgrade()) {
            let master_group = master.propagation().peer_group_id();
            if master_group.is_valid() {
                parts.push(alloc::format!("master:{}", master_group.0));
                if let Some(dom) = dominating_peer_group_id(&master) {
                    if dom != master_group.0 {
                        parts.push(alloc::format!("propagate_from:{dom}"));
                    }
                }
            }
        }
        if inner.flags.contains(PropagationFlags::UNBINDABLE) {
            parts.push("unbindable".into());
        }
        parts.join(" ")
    }
}

fn dominating_peer_group_id(immediate_master: &Arc<MountFS>) -> Option<usize> {
    let mut dominating = None;
    let mut current = immediate_master.propagation().master();
    while let Some(master) = current {
        let group = master.propagation().peer_group_id();
        if group.is_valid() {
            dominating = Some(group.0);
        }
        current = master.propagation().master();
    }
    dominating
}

impl Clone for MountPropagation {
    fn clone(&self) -> Self {
        let inner = self.inner.lock();
        Self {
            inner: SpinLock::new(MountPropagationInner {
                flags: inner.flags,
                peer_group: inner.peer_group.clone(),
                master: inner.master.clone(),
                slaves: inner.slaves.clone(),
            }),
        }
    }
}

/// Convert mount flags to propagation type
///
/// Linux removes only `MS_REC | MS_SILENT`, rejects every remaining
/// non-propagation bit, and requires exactly one propagation type bit.
pub fn flags_to_propagation_type(flags: MountFlags) -> Result<PropagationType, SystemError> {
    let propagation_mask =
        MountFlags::SHARED | MountFlags::SLAVE | MountFlags::PRIVATE | MountFlags::UNBINDABLE;
    let allowed = propagation_mask | MountFlags::REC | MountFlags::SILENT;
    if !(flags & !allowed).is_empty() {
        return Err(SystemError::EINVAL);
    }

    let type_flags = flags & propagation_mask;
    if type_flags.bits().count_ones() != 1 {
        return Err(SystemError::EINVAL);
    }

    if type_flags == MountFlags::SHARED {
        Ok(PropagationType::Shared)
    } else if type_flags == MountFlags::SLAVE {
        Ok(PropagationType::Slave)
    } else if type_flags == MountFlags::PRIVATE {
        Ok(PropagationType::Private)
    } else {
        Ok(PropagationType::Unbindable)
    }
}

/// Check if mount flags indicate a propagation type change request
pub fn is_propagation_change(flags: MountFlags) -> bool {
    flags.intersects(
        MountFlags::SHARED | MountFlags::PRIVATE | MountFlags::SLAVE | MountFlags::UNBINDABLE,
    )
}

/// Change the propagation type of a mount.
///
/// This implements the core logic for `mount --make-{shared,private,slave,unbindable}`.
///
/// # Arguments
/// * `mount` - The mount to change
/// * `prop_type` - The new propagation type
///
/// # Returns
/// * `Ok(())` on success
/// * `Err(SystemError)` on failure
#[cfg(test)]
pub fn change_mnt_propagation(
    mount: &Arc<MountFS>,
    prop_type: PropagationType,
) -> Result<(), SystemError> {
    change_mnt_propagation_recursive(mount, prop_type, false)
}

fn change_mnt_propagation_locked(
    mount: &Arc<MountFS>,
    prop_type: PropagationType,
    reserved_group: Option<Arc<PropagationGroup>>,
) {
    let propagation = mount.propagation();

    match prop_type {
        PropagationType::Shared => {
            let was_shared = propagation.is_shared();
            if !was_shared {
                propagation.set_shared_with_group(
                    reserved_group.expect("new shared mount must have a reserved group"),
                );
                register_peer(propagation.peer_group_id(), mount);
            }
        }
        PropagationType::Private => {
            do_make_slave(mount);
            detach_from_master(mount);
            propagation.set_private();
        }
        PropagationType::Slave => {
            do_make_slave(mount);
        }
        PropagationType::Unbindable => {
            do_make_slave(mount);
            detach_from_master(mount);
            propagation.set_unbindable();
        }
    }
}

/// Convert a mount to Linux slave semantics.
///
/// This mirrors Linux `do_make_slave()`:
/// - if the mount has peers, remove it from the peer group and choose one peer
///   as the new master;
/// - if it has no peers but already has a master, keep that master;
/// - if it has neither peers nor master, orphan its existing slaves and leave
///   it private;
/// - otherwise reparent its existing slaves to the new master and make the
///   mount itself a slave of that master.
fn do_make_slave(mount: &Arc<MountFS>) {
    let propagation = mount.propagation();
    let old_group_id = propagation.peer_group_id();
    let was_shared = propagation.is_shared();
    let peers = if was_shared {
        get_peers(old_group_id, mount)
    } else {
        Vec::new()
    };

    if was_shared {
        leave_peer_group(mount);
    }

    let master = if peers.is_empty() {
        propagation.master()
    } else {
        choose_slave_master(mount, peers)
    };

    let Some(master) = master else {
        for slave in propagation.take_slaves() {
            slave.propagation().set_master(None);
        }
        propagation.set_master(None);
        return;
    };

    let mount_weak = Arc::downgrade(mount);
    if let Some(old_master) = propagation.master() {
        if !Arc::ptr_eq(&old_master, &master) {
            old_master.propagation().remove_slave(&mount_weak);
        }
    }

    for slave in propagation.take_slaves() {
        slave
            .propagation()
            .set_master(Some(Arc::downgrade(&master)));
        master.propagation().add_slave(Arc::downgrade(&slave));
    }

    propagation.set_master(Some(Arc::downgrade(&master)));
    master.propagation().add_slave(mount_weak);
}

fn choose_slave_master(mount: &Arc<MountFS>, peers: Vec<Arc<MountFS>>) -> Option<Arc<MountFS>> {
    let mount_root = mount.root_inner_inode();
    let fallback = peers.first().cloned();
    peers
        .into_iter()
        .find(|peer| Arc::ptr_eq(&peer.root_inner_inode(), &mount_root))
        .or(fallback)
}

fn detach_from_master(mount: &Arc<MountFS>) {
    let propagation = mount.propagation();
    if let Some(master) = propagation.master() {
        master.propagation().remove_slave(&Arc::downgrade(mount));
    }
    propagation.set_master(None);
}

/// Register a copied mount in its master's slave list when it already carries
/// a master pointer cloned from another mount.
pub fn register_slave_with_master(mount: &Arc<MountFS>) {
    if let Some(master) = mount.propagation().master() {
        master.propagation().add_slave(Arc::downgrade(mount));
    }
}

/// Apply Linux bind-clone propagation inheritance before the clone is
/// propagated from the destination parent.
///
/// This only prepares the clone's propagation flags and master pointer. Peer
/// and slave registration is intentionally deferred until after destination
/// parent propagation, otherwise the new child could be observed as a peer of
/// its own parent when both happen to use the same peer group.
pub fn inherit_bind_mount_propagation(source: &Arc<MountFS>, clone: &Arc<MountFS>) {
    let source_prop = source.propagation();
    if !source_prop.is_shared() && !source_prop.is_slave() {
        return;
    }

    let clone_prop = clone.propagation();
    if source_prop.is_shared() {
        if clone_prop.is_shared() {
            unregister_peer(clone_prop.peer_group_id(), clone);
        }
        let group = source_prop
            .peer_group()
            .expect("shared mount must own a propagation group");
        clone_prop.set_shared_with_group(group);
    }

    if source_prop.is_slave() {
        clone_prop.set_slave(source_prop.master().map(|master| Arc::downgrade(&master)));
    }
}

/// Change the propagation type of a mount tree (recursive).
///
/// This implements `mount --make-r{shared,private,slave,unbindable}`.
///
/// # Arguments
/// * `mount` - The root mount of the subtree
/// * `prop_type` - The new propagation type
/// * `recursive` - Whether to apply recursively
///
/// # Returns
/// * `Ok(())` on success
/// * `Err(SystemError)` on failure
pub fn change_mnt_propagation_recursive(
    mount: &Arc<MountFS>,
    prop_type: PropagationType,
    recursive: bool,
) -> Result<(), SystemError> {
    let _topology = MOUNT_LIFECYCLE_LOCK.lock();
    let mut mounts = Vec::new();
    mounts.push(mount.clone());
    if recursive {
        let mut index = 0;
        while index < mounts.len() {
            let current = mounts[index].clone();
            index += 1;
            mounts.extend(current.mountpoints().values().cloned());
        }
    }

    // Linux invent_group_ids() reserves every required ID before changing the
    // first mount. Mirror that all-or-nothing behavior for recursive changes.
    let mut reserved_groups = Vec::with_capacity(mounts.len());
    for current in &mounts {
        if !current.is_live() {
            return Err(SystemError::EINVAL);
        }
        if prop_type == PropagationType::Shared && !current.propagation().is_shared() {
            reserved_groups.push(Some(PropagationGroup::alloc()?));
        } else {
            reserved_groups.push(None);
        }
    }

    for (current, group) in mounts.iter().zip(reserved_groups) {
        change_mnt_propagation_locked(current, prop_type, group);
    }
    Ok(())
}

// ============================================================================
// Mount Propagation Functions
// ============================================================================

use crate::filesystem::vfs::InodeId;

/// Propagate a mount event to all peers and slaves.
///
/// When a new mount is created on a shared mount point, this function
/// propagates the mount to all peers in the same group and all slaves.
///
/// # Arguments
/// * `source_mnt` - The mount where the new mount was created
/// * `mountpoint_id` - The inode ID of the mountpoint
/// * `new_child` - The newly created MountFS
/// * `mount_path` - The mount path to register in peer namespaces' mount_list
///
/// # Returns
/// * `Ok(())` on success
/// * `Err(SystemError)` on failure (partial propagation may have occurred)
#[cfg(test)]
pub fn propagate_mount(
    source_mnt: &Arc<MountFS>,
    mountpoint_id: InodeId,
    new_child: &Arc<MountFS>,
    mount_path: &Arc<MountPath>,
) -> Result<(), SystemError> {
    let topology = MOUNT_LIFECYCLE_LOCK.lock();
    let plan = prepare_mount_propagation_locked(source_mnt, mountpoint_id, &topology)?;
    commit_mount_propagation_locked(plan, mountpoint_id, new_child, mount_path, &topology);
    Ok(())
}

struct PropagationTarget {
    mount: Arc<MountFS>,
    /// Mount ID of the preceding propagation destination. Its new child is
    /// this destination's master; `None` means the original source child.
    master_target_id: Option<usize>,
}

pub(crate) struct MountPropagationPlan {
    source_group: Option<Arc<PropagationGroup>>,
    source_parent_path: Option<Arc<MountPath>>,
    targets: Vec<PropagationTarget>,
    slave_child_groups: BTreeMap<usize, Arc<PropagationGroup>>,
}

/// Snapshot propagation destinations and reserve every group before the caller
/// publishes the source mount.
pub(crate) fn prepare_mount_propagation_locked(
    source_mnt: &Arc<MountFS>,
    mountpoint_id: InodeId,
    _topology: &MutexGuard<'_, ()>,
) -> Result<MountPropagationPlan, SystemError> {
    let propagation = source_mnt.propagation();
    let group_id = propagation.peer_group_id();
    let source_parent_path = source_mnt
        .namespace()
        .and_then(|ns| ns.mount_list().get_mount_path_by_mountfs(source_mnt));

    if !propagation.is_shared() {
        return Ok(MountPropagationPlan {
            source_group: None,
            source_parent_path,
            targets: Vec::new(),
            slave_child_groups: BTreeMap::new(),
        });
    }
    let source_group = propagation
        .peer_group()
        .expect("shared mount must own a propagation group");

    // log::debug!(
    //     "propagate_mount: propagating from group {} to peers",
    //     group_id.0
    // );

    let candidates = collect_propagation_targets(source_mnt, group_id);
    let mut blocked = HashSet::new();
    let mut targets = Vec::new();
    for target in candidates {
        let master_blocked = target
            .master_target_id
            .is_some_and(|master_id| blocked.contains(&master_id));
        if master_blocked || target.mount.mountpoints().contains_key(&mountpoint_id) {
            blocked.insert(target.mount.mount_id().data());
            continue;
        }
        targets.push(target);
    }

    // Reserve one group for every distinct shared slave peer group before any
    // topology is published. The Arc owners keep these IDs unavailable even if
    // a later preparation step drops a temporary clone.
    let mut slave_child_groups = BTreeMap::new();
    for target in &targets {
        if !target.mount.is_live() {
            return Err(SystemError::EBUSY);
        }
        let target_prop = target.mount.propagation();
        if target_prop.is_shared() && target_prop.peer_group_id() != group_id {
            let target_group_id = target_prop.peer_group_id().data();
            if let Entry::Vacant(entry) = slave_child_groups.entry(target_group_id) {
                entry.insert(PropagationGroup::alloc()?);
            }
        }
    }

    Ok(MountPropagationPlan {
        source_group: Some(source_group),
        source_parent_path,
        targets,
        slave_child_groups,
    })
}

/// Consume a preallocated plan without performing any group-ID allocation.
pub(crate) fn commit_mount_propagation_locked(
    plan: MountPropagationPlan,
    mountpoint_id: InodeId,
    new_child: &Arc<MountFS>,
    mount_path: &Arc<MountPath>,
    _topology: &MutexGuard<'_, ()>,
) {
    let MountPropagationPlan {
        source_group,
        source_parent_path,
        targets,
        slave_child_groups,
    } = plan;
    if targets.is_empty() {
        return;
    }
    let context = MountPropagationCommitContext {
        source_group: source_group
            .as_ref()
            .expect("non-empty propagation plan must retain source group"),
        mountpoint_id,
        source_child: new_child,
        mount_path,
        source_parent_path: &source_parent_path,
        slave_child_groups: &slave_child_groups,
    };
    let mut propagated_children = BTreeMap::new();
    for target in targets {
        let master_child = target
            .master_target_id
            .and_then(|id| propagated_children.get(&id).cloned())
            .unwrap_or_else(|| new_child.clone());
        let cloned = propagate_one(&target.mount, &master_child, &context);
        propagated_children.insert(target.mount.mount_id().data(), cloned);
    }
}

/// Snapshot every propagation destination. A shared slave contributes its peer
/// group and the walk then continues through slaves of every peer, matching the
/// group traversal performed by Linux `next_group()`.
fn collect_propagation_targets(
    source_mnt: &Arc<MountFS>,
    source_group_id: PropagationGroupId,
) -> Vec<PropagationTarget> {
    struct PendingTarget {
        mount: Arc<MountFS>,
        master_target_id: Option<usize>,
    }

    let mut pending: Vec<PendingTarget> = Vec::new();
    let mut queued = HashSet::new();
    for peer in get_all_peers(source_group_id) {
        if queued.insert(peer.mount_id().data()) {
            pending.push(PendingTarget {
                mount: peer,
                master_target_id: None,
            });
        }
    }
    if queued.insert(source_mnt.mount_id().data()) {
        pending.push(PendingTarget {
            mount: source_mnt.clone(),
            master_target_id: None,
        });
    }

    let mut targets = Vec::new();
    let mut index = 0;
    while index < pending.len() {
        let current = pending[index].mount.clone();
        let master_target_id = pending[index].master_target_id;
        index += 1;

        if !Arc::ptr_eq(&current, source_mnt) {
            targets.push(PropagationTarget {
                mount: current.clone(),
                master_target_id,
            });
        }

        let child_master_target_id = if Arc::ptr_eq(&current, source_mnt) {
            None
        } else {
            Some(current.mount_id().data())
        };
        let current_prop = current.propagation();
        for slave in current_prop.slaves() {
            let slave_prop = slave.propagation();
            if queued.insert(slave.mount_id().data()) {
                pending.push(PendingTarget {
                    mount: slave,
                    master_target_id: child_master_target_id,
                });
            }
            if slave_prop.is_shared() {
                for peer in get_all_peers(slave_prop.peer_group_id()) {
                    if queued.insert(peer.mount_id().data()) {
                        // A shared-slave peer follows the corresponding member
                        // of the master's peer group, not necessarily the
                        // first slave through which this group was reached.
                        // Linux propagate_one() derives last_source from each
                        // destination's own mnt_master in the same way.
                        let peer_master_target_id =
                            peer.propagation().master().and_then(|master| {
                                (!Arc::ptr_eq(&master, source_mnt))
                                    .then(|| master.mount_id().data())
                            });
                        pending.push(PendingTarget {
                            mount: peer,
                            master_target_id: peer_master_target_id,
                        });
                    }
                }
            }
        }
    }
    targets
}

struct MoveSourceNode {
    mount: Arc<MountFS>,
    parent_index: Option<usize>,
    mountpoint_id: InodeId,
}

struct MoveDestination {
    parent: Arc<MountFS>,
    group: Option<Arc<PropagationGroup>>,
    is_source_peer: bool,
    master_destination_index: Option<usize>,
}

/// Immutable destination/source snapshot plus every group needed by a
/// move-to-shared operation. It is built before the topology move, equivalent
/// to Linux reserving group IDs before `attach_recursive_mnt()` commits.
pub(crate) struct MoveMountPropagationPlan {
    source_nodes: Vec<MoveSourceNode>,
    destinations: Vec<MoveDestination>,
    source_groups: BTreeMap<usize, Arc<PropagationGroup>>,
    slave_groups: BTreeMap<(usize, usize), Arc<PropagationGroup>>,
    target_parent_path: Option<Arc<MountPath>>,
}

pub(crate) fn prepare_move_mount_propagation_locked(
    target_parent: &Arc<MountFS>,
    moved_root: &Arc<MountFS>,
    moved_root_mp_id: InodeId,
    _topology: &MutexGuard<'_, ()>,
) -> Result<MoveMountPropagationPlan, SystemError> {
    let target_prop = target_parent.propagation();
    let target_group = target_prop.peer_group().ok_or(SystemError::EINVAL)?;
    let target_group_id = target_group.id();

    let mut destinations = Vec::new();
    let mut destination_indices = BTreeMap::new();
    for target in collect_propagation_targets(target_parent, target_group_id) {
        let parent = target.mount;
        let master_destination_index = match target.master_target_id {
            Some(master_id) => match destination_indices.get(&master_id).copied() {
                Some(index) => Some(index),
                None => {
                    continue;
                }
            },
            None => None,
        };
        if !parent.is_live() {
            return Err(SystemError::EBUSY);
        }
        if parent.mountpoints().contains_key(&moved_root_mp_id) {
            continue;
        }
        let group = parent.propagation().peer_group();
        let is_source_peer = group
            .as_ref()
            .is_some_and(|group| Arc::ptr_eq(group, &target_group));
        let destination_index = destinations.len();
        destination_indices.insert(parent.mount_id().data(), destination_index);
        destinations.push(MoveDestination {
            parent,
            group,
            is_source_peer,
            master_destination_index,
        });
    }

    let mut source_nodes = Vec::new();
    source_nodes.push(MoveSourceNode {
        mount: moved_root.clone(),
        parent_index: None,
        mountpoint_id: moved_root_mp_id,
    });
    let mut index = 0;
    while index < source_nodes.len() {
        let current = source_nodes[index].mount.clone();
        for (mountpoint_id, child) in current.mountpoints().iter() {
            source_nodes.push(MoveSourceNode {
                mount: child.clone(),
                parent_index: Some(index),
                mountpoint_id: *mountpoint_id,
            });
        }
        index += 1;
    }

    let mut source_groups = BTreeMap::new();
    for node in &source_nodes {
        if !node.mount.propagation().is_shared() {
            source_groups.insert(node.mount.mount_id().data(), PropagationGroup::alloc()?);
        }
    }

    let mut slave_groups = BTreeMap::new();
    for node in &source_nodes {
        for destination in &destinations {
            if destination.is_source_peer {
                continue;
            }
            if let Some(group) = destination.group.as_ref() {
                let key = (node.mount.mount_id().data(), group.id().data());
                if let Entry::Vacant(entry) = slave_groups.entry(key) {
                    entry.insert(PropagationGroup::alloc()?);
                }
            }
        }
    }

    let target_parent_path = target_parent
        .namespace()
        .and_then(|ns| ns.mount_list().get_mount_path_by_mountfs(target_parent));
    Ok(MoveMountPropagationPlan {
        source_nodes,
        destinations,
        source_groups,
        slave_groups,
        target_parent_path,
    })
}

pub(crate) fn commit_move_mount_propagation_locked(
    plan: MoveMountPropagationPlan,
    moved_root_path: &Arc<MountPath>,
    _topology: &MutexGuard<'_, ()>,
) {
    // Install every source group first. This phase cannot allocate or fail.
    for node in &plan.source_nodes {
        let propagation = node.mount.propagation();
        if !propagation.is_shared() {
            let group = plan
                .source_groups
                .get(&node.mount.mount_id().data())
                .expect("move source group must be reserved")
                .clone();
            propagation.set_shared_with_group(group);
            register_peer(propagation.peer_group_id(), &node.mount);
        }
    }

    // Clone the original subtree once per destination captured before the move.
    // Never query the newly-created peer topology while committing.
    let mut destination_clones: Vec<Vec<Arc<MountFS>>> = Vec::new();
    for destination in plan.destinations {
        let destination_root_path = propagated_mount_path(
            &plan.target_parent_path,
            moved_root_path,
            &destination.parent,
        );
        let mut clones: Vec<Arc<MountFS>> = Vec::with_capacity(plan.source_nodes.len());

        for (node_index, node) in plan.source_nodes.iter().enumerate() {
            let target_parent = node
                .parent_index
                .map_or_else(|| destination.parent.clone(), |index| clones[index].clone());
            let cloned = node.mount.deepcopy(None);
            let cloned_prop = cloned.propagation();

            if destination.is_source_peer {
                let group = node
                    .mount
                    .propagation()
                    .peer_group()
                    .expect("move source must be shared before replication");
                cloned_prop.set_shared_with_group(group);
            } else {
                cloned_prop.set_private();
                let master_child = destination
                    .master_destination_index
                    .map(|index| destination_clones[index][node_index].clone())
                    .unwrap_or_else(|| node.mount.clone());
                cloned_prop.set_slave(Some(Arc::downgrade(&master_child)));
                if let Some(target_group) = destination.group.as_ref() {
                    let key = (node.mount.mount_id().data(), target_group.id().data());
                    let group = plan
                        .slave_groups
                        .get(&key)
                        .expect("move slave group must be reserved")
                        .clone();
                    cloned_prop.set_shared_with_group(group);
                }
            }

            if let Some(source_mountpoint) = node.mount.self_mountpoint() {
                cloned.set_self_mountpoint(Some(
                    source_mountpoint.clone_with_new_mount_fs(target_parent.clone()),
                ));
            }

            let clone_path = if node.parent_index.is_none() {
                destination_root_path.clone()
            } else {
                node.mount
                    .namespace()
                    .and_then(|ns| ns.mount_list().get_mount_path_by_mountfs(&node.mount))
                    .and_then(|path| {
                        mount_path_suffix(moved_root_path.as_str(), path.as_str()).map(|suffix| {
                            Arc::new(MountPath::from(join_mount_path(
                                destination_root_path.as_str(),
                                suffix,
                            )))
                        })
                    })
                    .unwrap_or_else(|| destination_root_path.clone())
            };

            target_parent
                .add_mount(node.mountpoint_id, cloned.clone())
                .expect("move propagation target was preflighted under topology lock");
            if let Some(ns) = target_parent.namespace() {
                cloned.set_namespace(Arc::downgrade(&ns));
                ns.add_mount(Some(node.mountpoint_id), clone_path, cloned.clone())
                    .expect("mount namespace insertion is infallible after topology preflight");
            }
            cloned.activate();

            if cloned_prop.is_shared() {
                register_peer(cloned_prop.peer_group_id(), &cloned);
            }
            if destination.is_source_peer {
                if cloned_prop.is_slave() {
                    register_slave_with_master(&cloned);
                }
            } else {
                let master_child = destination
                    .master_destination_index
                    .map(|index| destination_clones[index][node_index].clone())
                    .unwrap_or_else(|| node.mount.clone());
                master_child
                    .propagation()
                    .add_slave(Arc::downgrade(&cloned));
            }
            clones.push(cloned);
        }
        destination_clones.push(clones);
    }
}

/// Propagate mount to a single target mount.
///
/// The cloned child mount joins the SAME peer group as the source child,
/// so that all propagated children can propagate events to each other.
struct MountPropagationCommitContext<'a> {
    source_group: &'a Arc<PropagationGroup>,
    mountpoint_id: InodeId,
    source_child: &'a Arc<MountFS>,
    mount_path: &'a Arc<MountPath>,
    source_parent_path: &'a Option<Arc<MountPath>>,
    slave_child_groups: &'a BTreeMap<usize, Arc<PropagationGroup>>,
}

fn propagate_one(
    target_mnt: &Arc<MountFS>,
    master_child: &Arc<MountFS>,
    context: &MountPropagationCommitContext<'_>,
) -> Arc<MountFS> {
    // Clone the child mount for this target
    let cloned_child = context.source_child.deepcopy(None);

    // Peer targets receive a peer of the propagated child. Slave targets
    // receive a slave of the propagated child; joining the master's peer group
    // would incorrectly allow reverse propagation back to the master side.
    let source_child_prop = context.source_child.propagation();
    let target_prop = target_mnt.propagation();
    let target_is_source_peer = target_prop
        .peer_group()
        .is_some_and(|group| Arc::ptr_eq(&group, context.source_group));
    if target_is_source_peer && source_child_prop.is_shared() {
        let group = source_child_prop
            .peer_group()
            .expect("shared mount must own a propagation group");
        let group_id = group.id();
        cloned_child.propagation().set_shared_with_group(group);
        register_peer(group_id, &cloned_child);
        if source_child_prop.is_slave() {
            register_slave_with_master(&cloned_child);
        }
    } else {
        let cloned_prop = cloned_child.propagation();
        if cloned_prop.is_shared() {
            unregister_peer(cloned_prop.peer_group_id(), &cloned_child);
        }
        cloned_prop.set_private();
        cloned_prop.set_slave(Some(Arc::downgrade(master_child)));
        master_child
            .propagation()
            .add_slave(Arc::downgrade(&cloned_child));

        if target_prop.is_shared() {
            let target_group_id = target_prop.peer_group_id();
            let child_group = context
                .slave_child_groups
                .get(&target_group_id.data())
                .expect("shared slave group must be reserved before propagation")
                .clone();
            let child_group_id = child_group.id();
            cloned_prop.set_shared_with_group(child_group);
            register_peer(child_group_id, &cloned_child);
        }
    }

    // Add the cloned mount to the target's mountpoints
    if let Some(source_mountpoint) = context.source_child.self_mountpoint() {
        cloned_child.set_self_mountpoint(Some(
            source_mountpoint.clone_with_new_mount_fs(target_mnt.clone()),
        ));
    }
    debug_assert!(target_mnt.is_live());
    target_mnt
        .add_mount(context.mountpoint_id, cloned_child.clone())
        .expect("propagation target was preflighted under topology lock");

    // Propagated child mounts must inherit the target mount's namespace,
    // otherwise subsequent is_belongs_to_mntns() checks would incorrectly return EINVAL.
    if let Some(ns) = target_mnt.namespace() {
        cloned_child.set_namespace(Arc::downgrade(&ns));
        let target_mount_path =
            propagated_mount_path(context.source_parent_path, context.mount_path, target_mnt);
        ns.add_mount(
            Some(context.mountpoint_id),
            target_mount_path,
            cloned_child.clone(),
        )
        .expect("mount namespace insertion is infallible after topology preflight");
    }
    cloned_child.activate();

    cloned_child
}

fn propagated_mount_path(
    source_parent_path: &Option<Arc<MountPath>>,
    source_child_path: &Arc<MountPath>,
    target_mnt: &Arc<MountFS>,
) -> Arc<MountPath> {
    let Some(target_parent_path) = target_mnt
        .namespace()
        .and_then(|ns| ns.mount_list().get_mount_path_by_mountfs(target_mnt))
    else {
        return source_child_path.clone();
    };
    let Some(suffix) = source_parent_path
        .as_ref()
        .and_then(|parent| mount_path_suffix(parent.as_str(), source_child_path.as_str()))
    else {
        return source_child_path.clone();
    };

    Arc::new(MountPath::from(join_mount_path(
        target_parent_path.as_str(),
        suffix,
    )))
}

fn mount_path_suffix<'a>(parent: &str, child: &'a str) -> Option<&'a str> {
    if parent == "/" {
        return child
            .strip_prefix('/')
            .map(|suffix| if suffix.is_empty() { "/" } else { child });
    }

    child
        .strip_prefix(parent)
        .filter(|suffix| suffix.starts_with('/'))
}

fn join_mount_path(parent: &str, suffix: &str) -> alloc::string::String {
    if suffix == "/" || suffix.is_empty() {
        return parent.into();
    }
    if parent == "/" {
        return suffix.into();
    }

    alloc::format!("{}{}", parent.trim_end_matches('/'), suffix)
}

/// Propagate an umount event to all peers and slaves.
///
/// When a mount is unmounted from a shared mount point, this function
/// propagates the umount to all peers in the same group and all slaves.
///
/// # Arguments
/// * `parent_mnt` - The parent mount where the umount occurred
/// * `mountpoint_id` - The inode ID of the mountpoint being unmounted
///
/// # Returns
/// * `Ok(())` on success
/// * `Err(SystemError)` on failure
pub fn propagate_umount(
    parent_mnt: &Arc<MountFS>,
    mountpoint_id: InodeId,
) -> Result<(), SystemError> {
    let propagation = parent_mnt.propagation();
    let group_id = propagation.peer_group_id();

    // Only propagate for shared mounts
    if !propagation.is_shared() {
        return Ok(());
    }

    // log::debug!(
    //     "propagate_umount: propagating umount from group {} to peers",
    //     group_id.0
    // );

    // Use the same complete peer/slave traversal as mount propagation. In
    // particular, shared slave groups must expand all of their peers before
    // descending further, matching Linux propagation_next().
    for target in collect_propagation_targets(parent_mnt, group_id) {
        if let Err(e) = umount_at_peer(&target.mount, mountpoint_id) {
            log::debug!("propagate_umount: target umount skipped: {:?}", e);
        }
    }

    Ok(())
}

/// Preflight the propagation set before ordinary umount mutates topology.
pub fn propagation_umount_busy(parent_mnt: &Arc<MountFS>, mountpoint_id: InodeId) -> bool {
    let propagation = parent_mnt.propagation();
    if !propagation.is_shared() {
        return false;
    }
    for target in collect_propagation_targets(parent_mnt, propagation.peer_group_id()) {
        let parent = target.mount;
        let child = parent.mountpoints().get(&mountpoint_id).cloned();
        if let Some(child) = child {
            if child.subtree_has_external_pins() {
                return true;
            }
        }
    }
    false
}

/// Umount at a specific peer mount.
///
/// Does NOT call `sync_filesystem()` here: all propagation clones share the same
/// `super_block_state` (including `umount_lock`) via `deepcopy()`. The top-level
/// `umount()` already holds the write lock while running the sync body; syncing
/// again here would be redundant and cause a RwSem self-deadlock.
fn umount_at_peer(peer_mnt: &Arc<MountFS>, mountpoint_id: InodeId) -> Result<(), SystemError> {
    let Some(child) = peer_mnt.mountpoints().remove(&mountpoint_id) else {
        return Ok(());
    };

    MountFS::deactivate_disconnected_subtree(&child);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::ramfs::RamFS;

    #[test]
    fn test_propagation_type_default() {
        let prop = MountPropagation::new_private();
        assert!(prop.is_private());
        assert!(!prop.is_shared());
    }

    #[test]
    fn test_propagation_type_change() {
        let prop = MountPropagation::new_private();
        prop.set_shared().unwrap();
        assert!(prop.is_shared());
        assert!(prop.peer_group_id().is_valid());

        prop.set_private();
        assert!(prop.is_private());

        prop.set_unbindable();
        assert!(prop.is_unbindable());
    }

    #[test]
    fn test_propagation_type_flags_match_linux_validation() {
        assert_eq!(
            flags_to_propagation_type(MountFlags::SHARED | MountFlags::REC | MountFlags::SILENT),
            Ok(PropagationType::Shared)
        );
        assert_eq!(
            flags_to_propagation_type(MountFlags::SHARED | MountFlags::PRIVATE),
            Err(SystemError::EINVAL)
        );
        assert_eq!(
            flags_to_propagation_type(MountFlags::SHARED | MountFlags::NODEV),
            Err(SystemError::EINVAL)
        );
        assert_eq!(
            flags_to_propagation_type(MountFlags::REC | MountFlags::SILENT),
            Err(SystemError::EINVAL)
        );
    }

    fn new_test_mount(propagation: Arc<MountPropagation>) -> Arc<MountFS> {
        let mount = MountFS::new(
            RamFS::new(),
            None,
            None,
            propagation,
            None,
            MountFlags::empty(),
            None,
        );
        // Test fixtures represent mounts that have already been published.
        // Production constructors intentionally remain in Constructing until
        // their topology/namespace insertion commits.
        mount.activate();
        mount
    }

    #[test]
    fn test_make_slave_selects_peer_as_master() {
        let prop_a = MountPropagation::new_shared().unwrap();
        let group_id = prop_a.peer_group_id();
        let prop_b = MountPropagation::new_shared_with_group(prop_a.peer_group().unwrap());
        let mount_a = new_test_mount(prop_a);
        let mount_b = new_test_mount(prop_b);

        register_peer(group_id, &mount_a);
        register_peer(group_id, &mount_b);

        change_mnt_propagation(&mount_a, PropagationType::Slave).unwrap();

        let prop_a = mount_a.propagation();
        assert!(prop_a.is_slave());
        assert!(!prop_a.is_shared());
        assert!(prop_a
            .master()
            .is_some_and(|master| Arc::ptr_eq(&master, &mount_b)));
        assert!(mount_b
            .propagation()
            .slaves()
            .iter()
            .any(|slave| Arc::ptr_eq(slave, &mount_a)));
    }

    #[test]
    fn test_make_shared_preserves_slave_master() {
        let master = new_test_mount(MountPropagation::new_shared().unwrap());
        let slave = new_test_mount(MountPropagation::new_slave(Arc::downgrade(&master)));
        master.propagation().add_slave(Arc::downgrade(&slave));

        change_mnt_propagation(&slave, PropagationType::Shared).unwrap();

        let slave_prop = slave.propagation();
        assert!(slave_prop.is_shared());
        assert!(slave_prop.is_slave());
        assert!(slave_prop.info_string().contains("shared:"));
        assert!(slave_prop.info_string().contains("master:"));
    }

    #[test]
    fn test_make_slave_without_master_orphans_existing_slaves() {
        let parent = new_test_mount(MountPropagation::new_private());
        let child = new_test_mount(MountPropagation::new_slave(Arc::downgrade(&parent)));
        parent.propagation().add_slave(Arc::downgrade(&child));

        change_mnt_propagation(&parent, PropagationType::Slave).unwrap();

        assert!(parent.propagation().is_private());
        assert!(!child.propagation().is_slave());
        assert!(parent.propagation().slaves().is_empty());
    }

    #[test]
    fn test_slaves_prunes_stale_entries() {
        let master = new_test_mount(MountPropagation::new_private());
        {
            let child = new_test_mount(MountPropagation::new_slave(Arc::downgrade(&master)));
            master.propagation().add_slave(Arc::downgrade(&child));
            assert_eq!(master.propagation().slaves().len(), 1);
        }

        assert!(master.propagation().slaves().is_empty());
    }

    #[test]
    fn test_umount_at_peer_detaches_slave_from_master() {
        let master = new_test_mount(MountPropagation::new_private());
        let peer = new_test_mount(MountPropagation::new_private());
        let child = new_test_mount(MountPropagation::new_slave(Arc::downgrade(&master)));
        let mountpoint_id = InodeId::new(7);

        master.propagation().add_slave(Arc::downgrade(&child));
        peer.add_mount(mountpoint_id, child.clone()).unwrap();

        umount_at_peer(&peer, mountpoint_id).unwrap();

        assert!(peer.mountpoints().get(&mountpoint_id).is_none());
        assert!(!child.propagation().is_slave());
        assert!(master.propagation().slaves().is_empty());
    }

    #[test]
    fn test_propagate_to_shared_slave_keeps_child_shared_slave() {
        let master = new_test_mount(MountPropagation::new_shared().unwrap());
        let source_child = new_test_mount(MountPropagation::new_shared().unwrap());
        let source_child_group = source_child.propagation().peer_group_id();

        let slave_group = PropagationGroup::alloc().unwrap();
        let slave_a_prop = MountPropagation::new_slave(Arc::downgrade(&master));
        slave_a_prop.set_shared_with_group(slave_group.clone());
        let slave_b_prop = MountPropagation::new_slave(Arc::downgrade(&master));
        slave_b_prop.set_shared_with_group(slave_group.clone());
        let slave_a = new_test_mount(slave_a_prop);
        let slave_b = new_test_mount(slave_b_prop);

        register_peer(slave_group.id(), &slave_a);
        register_peer(slave_group.id(), &slave_b);
        master.propagation().add_slave(Arc::downgrade(&slave_a));
        master.propagation().add_slave(Arc::downgrade(&slave_b));

        let mountpoint_id = InodeId::new(42);
        let mount_path = Arc::new(MountPath::from("/propagated"));
        propagate_mount(&master, mountpoint_id, &source_child, &mount_path).unwrap();

        let child_a = slave_a
            .mountpoints()
            .get(&mountpoint_id)
            .expect("slave_a should receive propagated child")
            .clone();
        let child_b = slave_b
            .mountpoints()
            .get(&mountpoint_id)
            .expect("slave_b should receive propagated child")
            .clone();
        let child_a_prop = child_a.propagation();
        let child_b_prop = child_b.propagation();

        assert!(child_a_prop.is_slave());
        assert!(child_a_prop.is_shared());
        assert!(child_b_prop.is_slave());
        assert!(child_b_prop.is_shared());
        assert_eq!(child_a_prop.peer_group_id(), child_b_prop.peer_group_id());
        assert_ne!(child_a_prop.peer_group_id(), source_child_group);
    }

    #[test]
    fn test_shared_slave_peers_follow_corresponding_master_peers() {
        let master_a_prop = MountPropagation::new_shared().unwrap();
        let master_group = master_a_prop.peer_group().unwrap();
        let master_b_prop = MountPropagation::new_shared_with_group(master_group.clone());
        let master_a = new_test_mount(master_a_prop);
        let master_b = new_test_mount(master_b_prop);
        register_peer(master_group.id(), &master_a);
        register_peer(master_group.id(), &master_b);

        let slave_group = PropagationGroup::alloc().unwrap();
        let slave_a_prop = MountPropagation::new_slave(Arc::downgrade(&master_a));
        slave_a_prop.set_shared_with_group(slave_group.clone());
        let slave_b_prop = MountPropagation::new_slave(Arc::downgrade(&master_b));
        slave_b_prop.set_shared_with_group(slave_group.clone());
        let slave_a = new_test_mount(slave_a_prop);
        let slave_b = new_test_mount(slave_b_prop);
        register_peer(slave_group.id(), &slave_a);
        register_peer(slave_group.id(), &slave_b);
        master_a.propagation().add_slave(Arc::downgrade(&slave_a));
        master_b.propagation().add_slave(Arc::downgrade(&slave_b));

        let targets = collect_propagation_targets(&master_a, master_group.id());
        let slave_a_target = targets
            .iter()
            .find(|target| Arc::ptr_eq(&target.mount, &slave_a))
            .expect("source-side slave must be visited");
        let slave_b_target = targets
            .iter()
            .find(|target| Arc::ptr_eq(&target.mount, &slave_b))
            .expect("peer-side slave must be visited");

        assert_eq!(slave_a_target.master_target_id, None);
        assert_eq!(
            slave_b_target.master_target_id,
            Some(master_b.mount_id().data())
        );
    }

    #[test]
    fn test_umount_expands_shared_slave_peer_group() {
        let master_a_prop = MountPropagation::new_shared().unwrap();
        let master_group = master_a_prop.peer_group().unwrap();
        let master_b_prop = MountPropagation::new_shared_with_group(master_group.clone());
        let master_a = new_test_mount(master_a_prop);
        let master_b = new_test_mount(master_b_prop);
        register_peer(master_group.id(), &master_a);
        register_peer(master_group.id(), &master_b);

        let slave_group = PropagationGroup::alloc().unwrap();
        let slave_a_prop = MountPropagation::new_slave(Arc::downgrade(&master_a));
        slave_a_prop.set_shared_with_group(slave_group.clone());
        let slave_b_prop = MountPropagation::new_slave(Arc::downgrade(&master_b));
        slave_b_prop.set_shared_with_group(slave_group.clone());
        let slave_a = new_test_mount(slave_a_prop);
        let slave_b = new_test_mount(slave_b_prop);
        register_peer(slave_group.id(), &slave_a);
        register_peer(slave_group.id(), &slave_b);
        master_a.propagation().add_slave(Arc::downgrade(&slave_a));
        master_b.propagation().add_slave(Arc::downgrade(&slave_b));

        let mountpoint_id = InodeId::new(73);
        slave_b
            .add_mount(
                mountpoint_id,
                new_test_mount(MountPropagation::new_private()),
            )
            .unwrap();

        propagate_umount(&master_a, mountpoint_id).unwrap();

        assert!(slave_b.mountpoints().get(&mountpoint_id).is_none());
    }
}
