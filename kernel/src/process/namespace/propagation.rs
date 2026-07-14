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

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use hashbrown::{HashMap, HashSet};
use system_error::SystemError;

use crate::filesystem::vfs::{
    mount::{MountFSInode, MountFlags, MountId, MOUNT_LIFECYCLE_LOCK},
    MountFS,
};
use crate::libs::{mutex::MutexGuard, rwlock::RwLock, spinlock::SpinLock};
use ida::IdAllocator;

// ============================================================================
// PropagationGroupId
// ============================================================================

/// Linux exposes peer group IDs as positive signed integers in mountinfo.
const PROPAGATION_GROUP_ID_END: usize = i32::MAX as usize + 1;

struct PropagationGroupIdAllocator {
    ida: IdAllocator,
    next_fresh: usize,
    /// Smallest position that may contain a freed ID below `next_fresh`.
    /// Free only moves this cursor backwards and therefore never allocates.
    lowest_free: usize,
    /// Number of holes below `next_fresh`. When the last hole is reused we
    /// jump directly back to the fresh frontier instead of scanning a dense
    /// allocated range under the global spin lock.
    reusable_count: usize,
}

impl PropagationGroupIdAllocator {
    const fn new() -> Self {
        Self {
            ida: IdAllocator::new(1, PROPAGATION_GROUP_ID_END).unwrap(),
            next_fresh: 1,
            lowest_free: 1,
            reusable_count: 0,
        }
    }

    fn alloc(&mut self) -> Option<usize> {
        while self.lowest_free < self.next_fresh {
            let id = self.lowest_free;
            self.lowest_free += 1;
            if !self.ida.exists(id) {
                let allocated = self.ida.alloc_specific(id)?;
                self.reusable_count -= 1;
                if self.reusable_count == 0 {
                    self.lowest_free = self.next_fresh;
                }
                return Some(allocated);
            }
        }
        debug_assert_eq!(self.reusable_count, 0);
        if self.next_fresh >= PROPAGATION_GROUP_ID_END {
            return None;
        }
        let id = self.next_fresh;
        let allocated = self.ida.alloc_specific(id);
        if allocated.is_some() {
            self.next_fresh += 1;
            self.lowest_free = self.next_fresh;
        }
        allocated
    }

    fn free(&mut self, id: usize) {
        if !self.ida.exists(id) {
            return;
        }
        self.ida.free(id);
        self.reusable_count += 1;
        if id < self.lowest_free {
            self.lowest_free = id;
        }
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

/// Ref-counted ownership of one allocated peer group ID. Live shared mounts
/// and detached propagation transactions keep the ID reserved until their
/// final owner is gone.
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

lazy_static! {
    /// Global peer group registry instance.
    static ref PEER_GROUP_REGISTRY: PeerGroupRegistry = PeerGroupRegistry::new();
}

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
    inner: RwLock<HashMap<usize, Vec<Weak<MountFS>>>>,
}

impl PeerGroupRegistry {
    #[inline]
    fn is_current_member(mount: &Arc<MountFS>, group_id: PropagationGroupId) -> bool {
        let propagation = mount.propagation();
        propagation.is_shared() && propagation.peer_group_id() == group_id
    }

    /// Create a new empty registry.
    fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
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
            let active: Vec<_> = peers
                .iter()
                .filter_map(|w| w.upgrade())
                .filter(|m| Self::is_current_member(m, group_id))
                .collect();
            let Some(exclude_index) = active
                .iter()
                .position(|member| Arc::ptr_eq(member, exclude))
            else {
                return active;
            };
            active[exclude_index + 1..]
                .iter()
                .chain(active[..exclude_index].iter())
                .cloned()
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

fn leave_peer_group(mount: &Arc<MountFS>) {
    let propagation = mount.propagation();
    let group_id = propagation.peer_group_id();
    if !propagation.is_shared() || !group_id.is_valid() {
        return;
    }
    unregister_peer(group_id, mount);
    propagation.clear_shared();
    propagation.clear_group_id();
}

/// Remove one mount from every propagation relationship before its lifecycle
/// leaves live topology. Callers serialize this with `MOUNT_LIFECYCLE_LOCK`.
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
            if let Some(group) = &inner.peer_group {
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
    let mount_root = mount.root_dentry();
    let fallback = peers.first().cloned();
    peers
        .into_iter()
        .find(|peer| Arc::ptr_eq(&peer.root_dentry(), &mount_root))
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

type GraphMountId = MountId;

struct PropagationGraphNode {
    mount: Arc<MountFS>,
    flags: PropagationFlags,
    peer_group: Option<Arc<PropagationGroup>>,
    master: Option<GraphMountId>,
    slaves: Vec<GraphMountId>,
}

struct PropagationGraphGroup {
    members: Vec<GraphMountId>,
}

struct PropagationGraph {
    nodes: HashMap<GraphMountId, PropagationGraphNode>,
    order: Vec<GraphMountId>,
    groups: HashMap<usize, PropagationGraphGroup>,
}

struct PreparedMountPropagationState {
    mount: Arc<MountFS>,
    inner: MountPropagationInner,
}

enum PreparedPeerGroupState {
    Remove(usize),
    Replace(usize, Vec<Weak<MountFS>>),
}

struct PropagationChangeTransaction {
    _topology_guard: MutexGuard<'static, ()>,
    mount_states: HashMap<GraphMountId, PreparedMountPropagationState>,
    peer_groups: Vec<PreparedPeerGroupState>,
}

fn reserve_vec<T, R>(
    vec: &mut Vec<T>,
    additional: usize,
    before_reserve: &mut R,
) -> Result<(), SystemError>
where
    R: FnMut() -> Result<(), SystemError>,
{
    if additional == 0 {
        return Ok(());
    }
    before_reserve()?;
    vec.try_reserve(additional).map_err(|_| SystemError::ENOMEM)
}

fn reserve_map<K, V, R>(
    map: &mut HashMap<K, V>,
    additional: usize,
    before_reserve: &mut R,
) -> Result<(), SystemError>
where
    K: core::hash::Hash + Eq,
    R: FnMut() -> Result<(), SystemError>,
{
    if additional == 0 {
        return Ok(());
    }
    before_reserve()?;
    map.try_reserve(additional).map_err(|_| SystemError::ENOMEM)
}

impl PropagationGraph {
    fn new<R>(target_count: usize, before_reserve: &mut R) -> Result<Self, SystemError>
    where
        R: FnMut() -> Result<(), SystemError>,
    {
        let mut nodes = HashMap::new();
        reserve_map(&mut nodes, target_count, before_reserve)?;
        let mut order = Vec::new();
        reserve_vec(&mut order, target_count, before_reserve)?;
        let mut groups = HashMap::new();
        reserve_map(&mut groups, target_count, before_reserve)?;
        Ok(Self {
            nodes,
            order,
            groups,
        })
    }

    /// Capture the complete master/slave component reachable from `seed`.
    /// Rebuilding reverse slave vectors is safe only when every existing edge
    /// incident on a replaced master is represented by its forward master edge.
    fn capture_component<R>(
        &mut self,
        seed: Arc<MountFS>,
        before_reserve: &mut R,
    ) -> Result<(), SystemError>
    where
        R: FnMut() -> Result<(), SystemError>,
    {
        let mut pending = Vec::new();
        reserve_vec(&mut pending, 1, before_reserve)?;
        pending.push(seed);
        let mut index = 0;
        while index < pending.len() {
            let mount = pending[index].clone();
            index += 1;
            let id = mount.mount_id();
            if self.nodes.contains_key(&id) {
                continue;
            }

            let propagation = mount.propagation();
            let inner = propagation.inner.lock();
            let master = inner.master.as_ref().and_then(Weak::upgrade);
            let mut candidate_slaves = Vec::new();
            reserve_vec(&mut candidate_slaves, inner.slaves.len(), before_reserve)?;
            candidate_slaves.extend(inner.slaves.iter().filter_map(Weak::upgrade));
            let flags = inner.flags;
            let peer_group = inner.peer_group.clone();
            drop(inner);

            // A reverse entry is authoritative only when the slave's forward
            // edge still names this mount. Check after dropping this mount's
            // spin lock so malformed legacy state cannot create nested-lock
            // deadlocks while the transaction normalizes stale weak entries.
            let mut slaves = Vec::new();
            reserve_vec(&mut slaves, candidate_slaves.len(), before_reserve)?;
            for slave in candidate_slaves {
                if slave
                    .propagation()
                    .master()
                    .is_some_and(|slave_master| Arc::ptr_eq(&slave_master, &mount))
                {
                    slaves.push(slave);
                }
            }
            let mut slave_ids = Vec::new();
            reserve_vec(&mut slave_ids, slaves.len(), before_reserve)?;
            slave_ids.extend(slaves.iter().map(|slave| slave.mount_id()));
            let node = PropagationGraphNode {
                mount: mount.clone(),
                flags,
                peer_group,
                master: master.as_ref().map(|master| master.mount_id()),
                slaves: slave_ids,
            };

            reserve_map(&mut self.nodes, 1, before_reserve)?;
            reserve_vec(&mut self.order, 1, before_reserve)?;
            self.nodes.insert(id, node);
            self.order.push(id);

            let additional = usize::from(master.is_some()) + slaves.len();
            reserve_vec(&mut pending, additional, before_reserve)?;
            if let Some(master) = master {
                pending.push(master);
            }
            pending.extend(slaves);
        }
        Ok(())
    }

    /// Snapshot one touched peer group in registry order. Every peer's full
    /// master/slave component is captured because a peer may become the new
    /// master and its unrelated existing slaves must survive final-state replace.
    fn capture_group<R>(
        &mut self,
        group_id: PropagationGroupId,
        required_member: &Arc<MountFS>,
        before_reserve: &mut R,
    ) -> Result<(), SystemError>
    where
        R: FnMut() -> Result<(), SystemError>,
    {
        if self.groups.contains_key(&group_id.0) {
            return Ok(());
        }

        let mut members = Vec::new();
        {
            let registry = PEER_GROUP_REGISTRY.inner.read();
            if let Some(registered) = registry.get(&group_id.0) {
                reserve_vec(&mut members, registered.len(), before_reserve)?;
                for weak in registered {
                    if let Some(member) = weak.upgrade() {
                        let propagation = member.propagation();
                        if propagation.is_shared() && propagation.peer_group_id() == group_id {
                            members.push(member);
                        }
                    }
                }
            }
        }
        if !members
            .iter()
            .any(|member| Arc::ptr_eq(member, required_member))
        {
            reserve_vec(&mut members, 1, before_reserve)?;
            members.push(required_member.clone());
        }

        let mut member_ids = Vec::new();
        reserve_vec(&mut member_ids, members.len(), before_reserve)?;
        for member in members {
            self.capture_component(member.clone(), before_reserve)?;
            member_ids.push(member.mount_id());
        }
        reserve_map(&mut self.groups, 1, before_reserve)?;
        self.groups.insert(
            group_id.0,
            PropagationGraphGroup {
                members: member_ids,
            },
        );
        Ok(())
    }

    fn simulate_make_slave<R>(
        &mut self,
        target: GraphMountId,
        before_reserve: &mut R,
    ) -> Result<(), SystemError>
    where
        R: FnMut() -> Result<(), SystemError>,
    {
        let (was_shared, old_group_id, old_master, target_root) = {
            let node = self.nodes.get(&target).unwrap();
            (
                node.flags.contains(PropagationFlags::SHARED),
                node.peer_group.as_ref().map(|group| group.id().0),
                node.master,
                node.mount.root_dentry(),
            )
        };

        let master = if was_shared {
            let group_id = old_group_id.expect("shared graph node has a group");
            let members = &self.groups.get(&group_id).unwrap().members;
            let target_index = members
                .iter()
                .position(|member| *member == target)
                .expect("shared graph group contains its target");
            let mut fallback = None;
            let mut exact = None;
            for offset in 1..members.len() {
                let member = members[(target_index + offset) % members.len()];
                fallback.get_or_insert(member);
                if Arc::ptr_eq(
                    &self.nodes.get(&member).unwrap().mount.root_dentry(),
                    &target_root,
                ) {
                    exact = Some(member);
                    break;
                }
            }
            let master = exact.or(fallback).or(old_master);
            self.groups
                .get_mut(&group_id)
                .unwrap()
                .members
                .retain(|member| *member != target);
            let node = self.nodes.get_mut(&target).unwrap();
            node.flags.remove(PropagationFlags::SHARED);
            node.peer_group = None;
            master
        } else {
            old_master
        };

        let migrating_slaves = core::mem::take(&mut self.nodes.get_mut(&target).unwrap().slaves);
        if let Some(old_master) = old_master {
            self.nodes
                .get_mut(&old_master)
                .unwrap()
                .slaves
                .retain(|slave| *slave != target);
        }
        if let Some(master) = master {
            let master_slaves = &mut self.nodes.get_mut(&master).unwrap().slaves;
            let additional = migrating_slaves
                .len()
                .checked_add(1)
                .ok_or(SystemError::ENOMEM)?;
            reserve_vec(master_slaves, additional, before_reserve)?;
            // Linux list_move() puts the converted mount at the head of the
            // new master's slave list, then appends its migrating slaves.
            master_slaves.insert(0, target);
            master_slaves.extend(migrating_slaves.iter().copied());
            for slave in migrating_slaves {
                self.nodes.get_mut(&slave).unwrap().master = Some(master);
            }
        } else {
            for slave in migrating_slaves {
                self.nodes.get_mut(&slave).unwrap().master = None;
            }
        }
        self.nodes.get_mut(&target).unwrap().master = master;
        Ok(())
    }

    fn detach_graph_master(&mut self, target: GraphMountId) {
        if let Some(master) = self.nodes.get(&target).unwrap().master {
            self.nodes
                .get_mut(&master)
                .unwrap()
                .slaves
                .retain(|slave| *slave != target);
        }
        self.nodes.get_mut(&target).unwrap().master = None;
    }

    fn simulate_change<A, R>(
        &mut self,
        targets: &[GraphMountId],
        prop_type: PropagationType,
        alloc_group: &mut A,
        before_reserve: &mut R,
    ) -> Result<(), SystemError>
    where
        A: FnMut() -> Result<Arc<PropagationGroup>, SystemError>,
        R: FnMut() -> Result<(), SystemError>,
    {
        for target in targets {
            match prop_type {
                PropagationType::Shared => {
                    if !self
                        .nodes
                        .get(target)
                        .unwrap()
                        .flags
                        .contains(PropagationFlags::SHARED)
                    {
                        let group = alloc_group()?;
                        let group_id = group.id().0;
                        let node = self.nodes.get_mut(target).unwrap();
                        node.flags.remove(PropagationFlags::UNBINDABLE);
                        node.flags.insert(PropagationFlags::SHARED);
                        node.peer_group = Some(group);
                        let mut members = Vec::new();
                        reserve_vec(&mut members, 1, before_reserve)?;
                        members.push(*target);
                        reserve_map(&mut self.groups, 1, before_reserve)?;
                        self.groups
                            .insert(group_id, PropagationGraphGroup { members });
                    }
                }
                PropagationType::Slave => {
                    self.simulate_make_slave(*target, before_reserve)?;
                }
                PropagationType::Private => {
                    self.simulate_make_slave(*target, before_reserve)?;
                    self.detach_graph_master(*target);
                    let node = self.nodes.get_mut(target).unwrap();
                    node.flags
                        .remove(PropagationFlags::SHARED | PropagationFlags::UNBINDABLE);
                    node.peer_group = None;
                }
                PropagationType::Unbindable => {
                    self.simulate_make_slave(*target, before_reserve)?;
                    self.detach_graph_master(*target);
                    let node = self.nodes.get_mut(target).unwrap();
                    node.flags.remove(PropagationFlags::SHARED);
                    node.flags.insert(PropagationFlags::UNBINDABLE);
                    node.peer_group = None;
                }
            }
        }
        Ok(())
    }
}

fn collect_change_targets<R>(
    root: &Arc<MountFS>,
    recursive: bool,
    before_reserve: &mut R,
) -> Result<Vec<Arc<MountFS>>, SystemError>
where
    R: FnMut() -> Result<(), SystemError>,
{
    let mut targets = Vec::new();
    let mut stack = Vec::new();
    reserve_vec(&mut stack, 1, before_reserve)?;
    stack.push(root.clone());
    while let Some(current) = stack.pop() {
        reserve_vec(&mut targets, 1, before_reserve)?;
        targets.push(current.clone());
        if !recursive {
            continue;
        }

        let mountpoints = current.mountpoints();
        let child_count = mountpoints.values().map(Vec::len).sum();
        let mut children = Vec::new();
        reserve_vec(&mut children, child_count, before_reserve)?;
        for shadow_stack in mountpoints.values() {
            children.extend(shadow_stack.iter().cloned());
        }
        drop(mountpoints);
        reserve_vec(&mut stack, children.len(), before_reserve)?;
        stack.extend(children.into_iter().rev());
    }
    Ok(targets)
}

impl PropagationChangeTransaction {
    fn prepare<A, R>(
        root: &Arc<MountFS>,
        prop_type: PropagationType,
        recursive: bool,
        mut alloc_group: A,
        mut before_reserve: R,
    ) -> Result<Self, SystemError>
    where
        A: FnMut() -> Result<Arc<PropagationGroup>, SystemError>,
        R: FnMut() -> Result<(), SystemError>,
    {
        let topology_guard = MOUNT_LIFECYCLE_LOCK.lock();
        let targets = collect_change_targets(root, recursive, &mut before_reserve)?;
        for target in &targets {
            if !target.is_live() {
                return Err(SystemError::EINVAL);
            }
        }

        let mut graph = PropagationGraph::new(targets.len(), &mut before_reserve)?;
        for target in &targets {
            graph.capture_component(target.clone(), &mut before_reserve)?;
        }

        // Snapshot every initially touched group before simulation changes any
        // graph node; registry filtering must observe the real pre-transaction state.
        for target in &targets {
            let propagation = target.propagation();
            if propagation.is_shared() {
                graph.capture_group(propagation.peer_group_id(), target, &mut before_reserve)?;
            }
        }

        let mut target_ids = Vec::new();
        reserve_vec(&mut target_ids, targets.len(), &mut before_reserve)?;
        target_ids.extend(targets.iter().map(|target| target.mount_id()));
        graph.simulate_change(
            &target_ids,
            prop_type,
            &mut alloc_group,
            &mut before_reserve,
        )?;

        let mut mount_states = HashMap::new();
        reserve_map(&mut mount_states, graph.nodes.len(), &mut before_reserve)?;
        for id in &graph.order {
            let node = graph.nodes.get(id).unwrap();
            let mut slaves = Vec::new();
            reserve_vec(&mut slaves, node.slaves.len(), &mut before_reserve)?;
            slaves.extend(
                node.slaves
                    .iter()
                    .map(|slave| Arc::downgrade(&graph.nodes.get(slave).unwrap().mount)),
            );
            let master = node
                .master
                .map(|master| Arc::downgrade(&graph.nodes.get(&master).unwrap().mount));
            mount_states.insert(
                *id,
                PreparedMountPropagationState {
                    mount: node.mount.clone(),
                    inner: MountPropagationInner {
                        flags: node.flags,
                        peer_group: node.peer_group.clone(),
                        master,
                        slaves,
                    },
                },
            );
        }
        let mut peer_groups = Vec::new();
        reserve_vec(&mut peer_groups, graph.groups.len(), &mut before_reserve)?;
        let mut new_group_keys = 0;
        {
            let registry = PEER_GROUP_REGISTRY.inner.read();
            for (group_id, group) in graph.groups {
                if group.members.is_empty() {
                    peer_groups.push(PreparedPeerGroupState::Remove(group_id));
                    continue;
                }
                let mut members = Vec::new();
                reserve_vec(&mut members, group.members.len(), &mut before_reserve)?;
                members.extend(
                    group
                        .members
                        .iter()
                        .map(|member| Arc::downgrade(&graph.nodes.get(member).unwrap().mount)),
                );
                if !registry.contains_key(&group_id) {
                    new_group_keys += 1;
                }
                peer_groups.push(PreparedPeerGroupState::Replace(group_id, members));
            }
        }
        if new_group_keys != 0 {
            before_reserve()?;
            PEER_GROUP_REGISTRY
                .inner
                .write()
                .try_reserve(new_group_keys)
                .map_err(|_| SystemError::ENOMEM)?;
        }

        // Drop every snapshot-only owner while the topology guard is still
        // held. The returned transaction owns only final state and resources.
        drop(graph.nodes);
        drop(graph.order);
        drop(target_ids);
        drop(targets);

        Ok(Self {
            _topology_guard: topology_guard,
            mount_states,
            peer_groups,
        })
    }

    fn commit(self) {
        let Self {
            _topology_guard,
            mount_states,
            peer_groups,
        } = self;
        {
            let mut registry = PEER_GROUP_REGISTRY.inner.write();
            for group in peer_groups {
                match group {
                    PreparedPeerGroupState::Remove(group_id) => {
                        registry.remove(&group_id);
                    }
                    PreparedPeerGroupState::Replace(group_id, members) => {
                        registry.insert(group_id, members);
                    }
                }
            }
        }
        for (_, state) in mount_states {
            let old_inner = {
                let propagation = state.mount.propagation();
                let mut inner = propagation.inner.lock();
                core::mem::replace(&mut *inner, state.inner)
            };
            // Release a last group owner after the per-mount spin lock. IDA
            // removal and the lowest-free cursor update never allocate.
            drop(old_inner);
        }
        drop(_topology_guard);
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
    let transaction = PropagationChangeTransaction::prepare(
        mount,
        prop_type,
        recursive,
        PropagationGroup::alloc,
        || Ok(()),
    )?;
    transaction.commit();
    Ok(())
}

// ============================================================================
// Mount Propagation Functions
// ============================================================================

#[derive(Clone, Copy)]
enum PropagationTargetKind {
    Peer,
    Slave,
}

struct PropagationTarget {
    mount: Arc<MountFS>,
    kind: PropagationTargetKind,
    master_parent: Option<Arc<MountFS>>,
}

struct PreparedMount {
    target_parent: Arc<MountFS>,
    mountpoint: Arc<MountFSInode>,
    expected_top: Option<Arc<MountFS>>,
    clone: Arc<MountFS>,
}

pub(crate) struct PreparedPropagation {
    source_mnt: Arc<MountFS>,
    mountpoint: Arc<MountFSInode>,
    new_child: Arc<MountFS>,
    mounts: Vec<PreparedMount>,
}

/// Return every peer/slave destination exactly once.  The registry is only a
/// discovery index; the mount/dentry objects below remain the correctness
/// identity throughout prepare and commit.
fn propagation_targets(source: &Arc<MountFS>) -> Vec<PropagationTarget> {
    let source_group = source.propagation().peer_group_id();
    let peers = get_peers(source_group, source);
    let mut result = Vec::new();
    let mut visited = HashSet::new();
    visited.insert(source.mount_id().data());

    for peer in peers {
        if visited.insert(peer.mount_id().data()) {
            result.push(PropagationTarget {
                mount: peer.clone(),
                kind: PropagationTargetKind::Peer,
                master_parent: None,
            });
        }
    }

    let mut pending: Vec<(Arc<MountFS>, Arc<MountFS>)> = source
        .propagation()
        .slaves()
        .into_iter()
        .map(|slave| (slave, source.clone()))
        .collect();
    for peer in result.iter().map(|target| &target.mount) {
        pending.extend(
            peer.propagation()
                .slaves()
                .into_iter()
                .map(|slave| (slave, peer.clone())),
        );
    }
    while let Some((slave, master_parent)) = pending.pop() {
        if !visited.insert(slave.mount_id().data()) {
            continue;
        }
        pending.extend(
            slave
                .propagation()
                .slaves()
                .into_iter()
                .map(|child| (child, slave.clone())),
        );
        result.push(PropagationTarget {
            mount: slave,
            kind: PropagationTargetKind::Slave,
            master_parent: Some(master_parent),
        });
    }
    result
}

fn configure_clone_propagation(
    source: &Arc<MountFS>,
    clone: &Arc<MountFS>,
    target_parent: &Arc<MountFS>,
    kind: PropagationTargetKind,
    master_source: Option<&Arc<MountFS>>,
    slave_groups: &mut BTreeMap<(usize, usize), Arc<PropagationGroup>>,
) -> Result<(), SystemError> {
    if matches!(kind, PropagationTargetKind::Peer) {
        return Ok(());
    }

    let clone_prop = clone.propagation();
    clone_prop.set_private();
    let master_source = master_source.expect("slave propagation requires the previous layer");
    clone_prop.set_slave(Some(Arc::downgrade(master_source)));

    // A shared slave parent needs one corresponding child peer group across
    // all of its peers.  Keying by object identities avoids pathname aliases.
    let target_prop = target_parent.propagation();
    if target_prop.is_shared() {
        let key = (target_prop.peer_group_id().data(), source.mount_id().data());
        let group = match slave_groups.entry(key) {
            alloc::collections::btree_map::Entry::Occupied(entry) => entry.get().clone(),
            alloc::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(PropagationGroup::alloc()?).clone()
            }
        };
        clone_prop.set_shared_with_group(group);
    }
    Ok(())
}

/// Build a complete detached copy.  No namespace, lifecycle, peer registry or
/// master/slave list is published during this phase.
fn prepare_subtree_copy(
    source: &Arc<MountFS>,
    target_parent: &Arc<MountFS>,
    mountpoint: Arc<MountFSInode>,
    kind: PropagationTargetKind,
    master_root: Option<&Arc<MountFS>>,
    slave_groups: &mut BTreeMap<(usize, usize), Arc<PropagationGroup>>,
) -> Result<Arc<MountFS>, SystemError> {
    let root_clone = source.deepcopy(Some(mountpoint))?;
    configure_clone_propagation(
        source,
        &root_clone,
        target_parent,
        kind,
        master_root,
        slave_groups,
    )?;

    let build_result = (|| {
        // An explicit queue prevents adversarially deep mount trees from
        // exhausting the kernel stack. Replaying each stack bottom-to-top
        // preserves shadow order in the detached clone.
        let mut queue = vec![(source.clone(), root_clone.clone(), master_root.cloned())];
        let mut index = 0;
        while index < queue.len() {
            let (source_parent, clone_parent, master_parent) = queue[index].clone();
            index += 1;
            let child_stacks: Vec<Vec<Arc<MountFS>>> =
                source_parent.mountpoints().values().cloned().collect();
            for stack in child_stacks {
                for (stack_index, source_child) in stack.into_iter().enumerate() {
                    let source_mp = source_child.self_mountpoint().ok_or(SystemError::EINVAL)?;
                    let clone_mp = clone_parent.wrapper_for_dentry(source_mp.shared_dentry())?;
                    let child_clone = source_child.deepcopy(Some(clone_mp.clone()))?;
                    let master_child = master_parent.as_ref().and_then(|master_parent| {
                        let master_mp =
                            master_parent.wrapper_for_existing_edge(source_mp.shared_dentry());
                        master_parent
                            .children_at(&master_mp)
                            .get(stack_index)
                            .cloned()
                    });
                    if matches!(kind, PropagationTargetKind::Slave) && master_child.is_none() {
                        return Err(SystemError::EBUSY);
                    }
                    configure_clone_propagation(
                        &source_child,
                        &child_clone,
                        &clone_parent,
                        kind,
                        master_child.as_ref(),
                        slave_groups,
                    )?;
                    if let Err(error) = clone_parent.attach_top(&clone_mp, child_clone.clone()) {
                        MountFS::deactivate_disconnected_subtree(&child_clone);
                        return Err(error);
                    }
                    queue.push((source_child, child_clone, master_child));
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = build_result {
        // Detached mount trees contain strong parent/child cycles. Break every
        // edge on prepare failure even though no lifecycle counters were
        // published yet.
        MountFS::deactivate_disconnected_subtree(&root_clone);
        return Err(error);
    }
    Ok(root_clone)
}

fn abandon_prepared(prepared: &[PreparedMount]) {
    for item in prepared {
        MountFS::deactivate_disconnected_subtree(&item.clone);
    }
}

pub(crate) fn abort_mount_propagation(prepared: Option<PreparedPropagation>) {
    if let Some(prepared) = prepared {
        abandon_prepared(&prepared.mounts);
    }
}

fn collect_subtree(root: &Arc<MountFS>) -> Vec<Arc<MountFS>> {
    let mut result = Vec::new();
    let mut pending = vec![root.clone()];
    while let Some(mount) = pending.pop() {
        pending.extend(mount.mount_children());
        result.push(mount);
    }
    result
}

/// Reserve every peer-group ID before changing a detached subtree. This is the
/// object-topology equivalent of Linux `invent_group_ids(..., true)` and keeps
/// recursive publication all-or-nothing on allocation failure.
pub(crate) fn ensure_subtree_shared(root: &Arc<MountFS>) -> Result<(), SystemError> {
    let subtree = collect_subtree(root);
    let mut groups = Vec::new();
    for mount in &subtree {
        if !mount.propagation().is_shared() {
            groups.push((mount.clone(), PropagationGroup::alloc()?));
        }
    }
    for (mount, group) in groups {
        mount.propagation().set_shared_with_group(group);
    }
    Ok(())
}

fn activate_subtree(
    root: &Arc<MountFS>,
    namespace: Option<&Arc<super::mnt::MntNamespace>>,
) -> Result<(), SystemError> {
    for mount in collect_subtree(root) {
        if let Some(namespace) = namespace {
            mount.set_namespace(Arc::downgrade(namespace));
        }
        mount.activate()?;
    }
    Ok(())
}

fn register_subtree(root: &Arc<MountFS>) {
    for mount in collect_subtree(root) {
        let prop = mount.propagation();
        if prop.is_shared() {
            register_peer(prop.peer_group_id(), &mount);
        }
        register_slave_with_master(&mount);
    }
}

pub(crate) fn prepare_mount_propagation_locked(
    source_mnt: &Arc<MountFS>,
    mountpoint: &Arc<MountFSInode>,
    new_child: &Arc<MountFS>,
) -> Result<Option<PreparedPropagation>, SystemError> {
    let source_prop = source_mnt.propagation();
    if !source_prop.is_shared() {
        return Ok(None);
    }
    let canonical_mountpoint = source_mnt.wrapper_for_dentry(mountpoint.shared_dentry())?;
    if canonical_mountpoint.dentry_id() != mountpoint.dentry_id() {
        return Err(SystemError::EINVAL);
    }

    let source_dentry = mountpoint.shared_dentry();
    let mut slave_groups = BTreeMap::new();
    let mut mounts = Vec::new();
    let mut propagated_sources = BTreeMap::new();
    propagated_sources.insert(source_mnt.mount_id().data(), new_child.clone());
    for target in propagation_targets(source_mnt) {
        let PropagationTarget {
            mount: target_parent,
            kind,
            master_parent,
        } = target;
        let master_source = master_parent
            .as_ref()
            .and_then(|master| propagated_sources.get(&master.mount_id().data()).cloned());
        if matches!(kind, PropagationTargetKind::Slave) && master_source.is_none() {
            continue;
        }
        let target_mp = match target_parent.wrapper_for_dentry(source_dentry.clone()) {
            Ok(mountpoint) => mountpoint,
            // Equivalent to Linux propagate_mnt skipping a peer whose bind
            // root does not cover the source mountpoint.
            Err(SystemError::EXDEV) => continue,
            Err(error) => {
                abandon_prepared(&mounts);
                return Err(error);
            }
        };
        let expected_top = target_parent.lookup_top(&target_mp);
        let clone = match prepare_subtree_copy(
            new_child,
            &target_parent,
            target_mp.clone(),
            kind,
            master_source.as_ref(),
            &mut slave_groups,
        ) {
            Ok(clone) => clone,
            Err(error) => {
                abandon_prepared(&mounts);
                return Err(error);
            }
        };
        propagated_sources.insert(target_parent.mount_id().data(), clone.clone());
        mounts.push(PreparedMount {
            target_parent,
            mountpoint: target_mp,
            expected_top,
            clone,
        });
    }
    Ok(Some(PreparedPropagation {
        source_mnt: source_mnt.clone(),
        mountpoint: canonical_mountpoint,
        new_child: new_child.clone(),
        mounts,
    }))
}

pub(crate) fn commit_mount_propagation_locked(
    prepared: Option<PreparedPropagation>,
) -> Result<(), SystemError> {
    let Some(prepared) = prepared else {
        return Ok(());
    };
    if !prepared.source_mnt.is_live()
        || !prepared
            .source_mnt
            .children_at(&prepared.mountpoint)
            .iter()
            .any(|child| Arc::ptr_eq(child, &prepared.new_child))
    {
        abandon_prepared(&prepared.mounts);
        return Err(SystemError::EBUSY);
    }
    for item in &prepared.mounts {
        if !item.target_parent.is_live() {
            abandon_prepared(&prepared.mounts);
            return Err(SystemError::EBUSY);
        }
        // A rename may have moved the shared dentry outside a bind root after
        // prepare. Re-project it while serialized instead of trusting the old
        // wrapper's pathname ancestry.
        let current_mountpoint = match item
            .target_parent
            .wrapper_for_dentry(item.mountpoint.shared_dentry())
        {
            Ok(mountpoint) => mountpoint,
            Err(_) => {
                abandon_prepared(&prepared.mounts);
                return Err(SystemError::EBUSY);
            }
        };
        if current_mountpoint.dentry_id() != item.mountpoint.dentry_id() {
            abandon_prepared(&prepared.mounts);
            return Err(SystemError::EBUSY);
        }
        let current = item.target_parent.lookup_top(&item.mountpoint);
        let unchanged = match (&current, &item.expected_top) {
            (None, None) => true,
            (Some(current), Some(expected)) => Arc::ptr_eq(current, expected),
            _ => false,
        };
        if !unchanged {
            abandon_prepared(&prepared.mounts);
            return Err(SystemError::EBUSY);
        }
    }

    // Namespace/lifecycle are initialized before any edge becomes reachable.
    let source_namespace = prepared.new_child.namespace();
    for item in &prepared.mounts {
        let namespace = item.target_parent.namespace();
        if let (Some(source_namespace), Some(target_namespace)) =
            (source_namespace.as_ref(), namespace.as_ref())
        {
            if !Arc::ptr_eq(source_namespace.user_ns(), target_namespace.user_ns()) {
                // Linux lock_mnt_tree() protects mounts propagated across a
                // user-namespace boundary, while leaving the propagated root
                // itself movable as the new visible boundary.
                for mount in collect_subtree(&item.clone) {
                    mount.lock_mount();
                }
                item.clone.unlock_mount();
            }
        }
        if let Err(error) = activate_subtree(&item.clone, namespace.as_ref()) {
            abandon_prepared(&prepared.mounts);
            return Err(error);
        }
    }

    let mut attached: Vec<&PreparedMount> = Vec::new();
    for item in &prepared.mounts {
        let result = if item.expected_top.is_some() {
            item.target_parent
                .attach_beneath(&item.mountpoint, item.clone.clone())
        } else {
            item.target_parent
                .attach_top(&item.mountpoint, item.clone.clone())
        };
        if let Err(error) = result {
            for committed in attached.iter().rev() {
                let _: Result<Arc<MountFS>, _> = committed
                    .target_parent
                    .detach_exact_restoring_cover(&committed.clone);
            }
            abandon_prepared(&prepared.mounts);
            return Err(error);
        }
        attached.push(item);
    }

    for item in &prepared.mounts {
        register_subtree(&item.clone);
    }
    Ok(())
}

/// Linux makes every mount in a moved tree shared before propagating the tree
/// into the destination parent's peers.  The complete tree is copied once per
/// destination instead of the former path/BFS reconstruction.
pub(crate) fn propagate_moved_tree_locked(
    target_parent: &Arc<MountFS>,
    moved_root: &Arc<MountFS>,
    moved_root_mountpoint: &Arc<MountFSInode>,
) -> Result<(), SystemError> {
    let subtree = collect_subtree(moved_root);
    let mut invented = Vec::new();
    for mount in &subtree {
        let prop = mount.propagation();
        if !prop.is_shared() {
            invented.push((mount.clone(), prop.prop_type(), PropagationGroup::alloc()?));
        }
    }
    for (mount, _, group) in &invented {
        let prop = mount.propagation();
        if !prop.is_shared() {
            prop.set_shared_with_group(group.clone());
            register_peer(prop.peer_group_id(), mount);
        }
    }
    let propagation =
        prepare_mount_propagation_locked(target_parent, moved_root_mountpoint, moved_root);
    if let Err(error) = propagation.and_then(commit_mount_propagation_locked) {
        // Linux discards invented group IDs when propagation preparation
        // fails. Restore the pre-move state rather than leaking a semantic
        // change from a failed move.
        for (mount, old_type, _) in invented.into_iter().rev() {
            let prop = mount.propagation();
            unregister_peer(prop.peer_group_id(), &mount);
            match old_type {
                PropagationType::Private => prop.set_private(),
                PropagationType::Slave => {
                    prop.clear_shared();
                    prop.clear_group_id();
                }
                PropagationType::Unbindable => prop.set_unbindable(),
                PropagationType::Shared => unreachable!(),
            }
        }
        return Err(error);
    }
    Ok(())
}

/// Propagate an umount event to all peers and slaves.
///
/// When a mount is unmounted from a shared mount point, this function
/// propagates the umount to all peers in the same group and all slaves.
///
/// # Arguments
/// * `parent_mnt` - The parent mount where the umount occurred
/// * `mountpoint` - The exact shared dentry where the event occurred
///
/// # Returns
/// * `Ok(())` on success
/// * `Err(SystemError)` on failure
pub fn propagate_umount(
    parent_mnt: &Arc<MountFS>,
    mountpoint: &Arc<MountFSInode>,
    source_child: &Arc<MountFS>,
    lazy: bool,
) -> Result<(), SystemError> {
    let propagation = parent_mnt.propagation();

    // Only propagate for shared mounts
    if !propagation.is_shared() {
        return Ok(());
    }

    // log::debug!(
    //     "propagate_umount: propagating umount from group {} to peers",
    //     group_id.0
    // );

    let prepared = propagated_umount_targets(parent_mnt, mountpoint, source_child)?;

    // The caller serializes detach through MOUNT_LIFECYCLE_LOCK. Validate the
    // whole set before the first mutation, then every detach below is an
    // invariant-preserving exact-object operation.
    for (target, target_mountpoint, child) in &prepared {
        if !target.is_live()
            || !target
                .children_at(target_mountpoint)
                .iter()
                .any(|candidate| Arc::ptr_eq(candidate, child))
        {
            return Err(SystemError::EBUSY);
        }
    }
    // Linux propagate_mount_unlock() clears MNT_LOCKED only from the exact
    // corresponding propagation roots before gathering them for umount.  The
    // protected descendants remain locked and, for lazy detach, connected to
    // their detached component.
    for (_, _, child) in &prepared {
        child.unlock_mount();
    }
    for (target, _, child) in &prepared {
        target.detach_exact_restoring_cover(child)?;
    }
    for (_, _, child) in prepared {
        child.set_self_mountpoint(None);
        MountFS::finish_disconnected_umount(&child, lazy)?;
    }
    Ok(())
}

type PropagatedUmountTarget = (Arc<MountFS>, Arc<MountFSInode>, Arc<MountFS>);

fn propagated_umount_targets(
    parent_mnt: &Arc<MountFS>,
    mountpoint: &Arc<MountFSInode>,
    source_child: &Arc<MountFS>,
) -> Result<Vec<PropagatedUmountTarget>, SystemError> {
    // A deep slave's child is mastered by the corresponding child in the
    // immediately preceding layer, not by the original source child. Keep the
    // same parent->child projection that mount propagation uses.
    let mut corresponding = BTreeMap::new();
    corresponding.insert(parent_mnt.mount_id().data(), source_child.clone());
    let mut result = Vec::new();
    for target in propagation_targets(parent_mnt) {
        let reference_child = match target.kind {
            PropagationTargetKind::Peer => source_child.clone(),
            PropagationTargetKind::Slave => {
                let Some(master_parent) = target.master_parent.as_ref() else {
                    continue;
                };
                let Some(child) = corresponding.get(&master_parent.mount_id().data()) else {
                    continue;
                };
                child.clone()
            }
        };
        let target_mountpoint = match target.mount.wrapper_for_dentry(mountpoint.shared_dentry()) {
            Ok(mountpoint) => mountpoint,
            Err(SystemError::EXDEV) => continue,
            Err(error) => return Err(error),
        };
        let Some(child) = propagated_child_at(&target.mount, &target_mountpoint, &reference_child)
        else {
            continue;
        };
        corresponding.insert(target.mount.mount_id().data(), child.clone());
        result.push((target.mount, target_mountpoint, child));
    }
    Ok(result)
}

fn propagated_child_at(
    parent: &Arc<MountFS>,
    mountpoint: &Arc<MountFSInode>,
    source_child: &Arc<MountFS>,
) -> Option<Arc<MountFS>> {
    let source_prop = source_child.propagation();
    parent
        .children_at(mountpoint)
        .into_iter()
        .rev()
        .find(|candidate| {
            let candidate_prop = candidate.propagation();
            (source_prop.is_shared()
                && candidate_prop.is_shared()
                && candidate_prop.peer_group_id() == source_prop.peer_group_id())
                || candidate_prop
                    .master()
                    .is_some_and(|master| Arc::ptr_eq(&master, source_child))
        })
}

/// Preflight the propagation set before ordinary umount mutates topology.
pub fn propagation_umount_busy(parent_mnt: &Arc<MountFS>, mountpoint: &Arc<MountFSInode>) -> bool {
    let propagation = parent_mnt.propagation();
    if !propagation.is_shared() {
        return false;
    }
    let Some(source_child) = parent_mnt.lookup_top(mountpoint) else {
        return true;
    };
    propagated_umount_targets(parent_mnt, mountpoint, &source_child)
        .map(|targets| {
            targets.into_iter().any(|(_, _, child)| {
                !child.mount_children().is_empty() || child.subtree_has_external_pins()
            })
        })
        .unwrap_or(true)
}

/// Umount at a specific peer mount.
///
/// Does NOT call `sync_filesystem()` here: all propagation clones share the same
/// `super_block_state` (including `umount_lock`) via `deepcopy()`. The top-level
/// `umount()` already holds the write lock while running the sync body; syncing
/// again here would be redundant and cause a RwSem self-deadlock.
#[cfg(test)]
fn umount_at_peer(
    peer_mnt: &Arc<MountFS>,
    source_mountpoint: &Arc<MountFSInode>,
    source_child: &Arc<MountFS>,
) -> Result<(), SystemError> {
    let peer_mountpoint = match peer_mnt.wrapper_for_dentry(source_mountpoint.shared_dentry()) {
        Ok(mountpoint) => mountpoint,
        Err(SystemError::EXDEV) => return Ok(()),
        Err(error) => return Err(error),
    };
    let Some(child) = propagated_child_at(peer_mnt, &peer_mountpoint, source_child) else {
        return Ok(());
    };
    peer_mnt.detach_exact(&child)?;
    MountFS::deactivate_disconnected_subtree(&child);

    Ok(())
}

/// Recursively propagate umount to slaves.
#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::{Cell, RefCell};

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

    #[test]
    fn test_propagation_group_allocator_reuses_freed_holes() {
        let mut allocator = PropagationGroupIdAllocator::new();
        let first = allocator.alloc().unwrap();
        let second = allocator.alloc().unwrap();
        assert_eq!(second, first + 1);

        allocator.free(first);
        assert_eq!(allocator.alloc(), Some(first));
        assert_eq!(allocator.reusable_count, 0);
        assert_eq!(allocator.lowest_free, allocator.next_fresh);
        assert_eq!(allocator.alloc(), Some(second + 1));
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
        mount.activate().unwrap();
        mount
    }

    fn attach_test_child(parent: &Arc<MountFS>, child: &Arc<MountFS>) {
        let mountpoint = parent.mountpoint_root_inode();
        child.set_self_mountpoint(Some(mountpoint.clone()));
        parent.attach_top(&mountpoint, child.clone()).unwrap();
    }

    fn shared_copy(source: &Arc<MountFS>) -> Arc<MountFS> {
        let copy = source.deepcopy(None).unwrap();
        copy.activate().unwrap();
        copy
    }

    #[test]
    fn test_recursive_target_collection_is_dfs_preorder() {
        let root = new_test_mount(MountPropagation::new_private());
        let child_a = new_test_mount(MountPropagation::new_private());
        let child_b = new_test_mount(MountPropagation::new_private());
        let grandchild_a = new_test_mount(MountPropagation::new_private());
        let grandchild_b = new_test_mount(MountPropagation::new_private());
        attach_test_child(&root, &child_a);
        attach_test_child(&root, &child_b);
        attach_test_child(&child_a, &grandchild_a);
        attach_test_child(&child_b, &grandchild_b);

        let targets = collect_change_targets(&root, true, &mut || Ok(())).unwrap();
        let position = |mount: &Arc<MountFS>| {
            targets
                .iter()
                .position(|target| Arc::ptr_eq(target, mount))
                .unwrap()
        };
        assert_eq!(position(&root), 0);
        assert_eq!(position(&grandchild_a), position(&child_a) + 1);
        assert_eq!(position(&grandchild_b), position(&child_b) + 1);
    }

    #[test]
    fn test_peer_ring_scan_starts_after_middle_target() {
        let peer_a = new_test_mount(MountPropagation::new_shared().unwrap());
        let group = peer_a.propagation().peer_group().unwrap();
        let target_b = shared_copy(&peer_a);
        let peer_c = shared_copy(&peer_a);
        let fallback_d = new_test_mount(MountPropagation::new_shared_with_group(group));
        let group_id = peer_a.propagation().peer_group_id();
        for mount in [&peer_a, &target_b, &peer_c, &fallback_d] {
            register_peer(group_id, mount);
        }

        let ring = get_peers(group_id, &target_b);
        assert!(Arc::ptr_eq(&ring[0], &peer_c));
        change_mnt_propagation(&target_b, PropagationType::Slave).unwrap();
        assert!(target_b
            .propagation()
            .master()
            .is_some_and(|master| Arc::ptr_eq(&master, &peer_c)));
    }

    #[test]
    fn test_recursive_prepare_group_failure_changes_nothing() {
        let root = new_test_mount(MountPropagation::new_private());
        let child = new_test_mount(MountPropagation::new_private());
        attach_test_child(&root, &child);

        let calls = Cell::new(0);
        let first_group = RefCell::new(None);
        let result = PropagationChangeTransaction::prepare(
            &root,
            PropagationType::Shared,
            true,
            || {
                let call = calls.get();
                calls.set(call + 1);
                if call == 1 {
                    return Err(SystemError::ENOSPC);
                }
                let group = PropagationGroup::alloc()?;
                *first_group.borrow_mut() = Some(Arc::downgrade(&group));
                Ok(group)
            },
            || Ok(()),
        );

        assert!(matches!(result, Err(SystemError::ENOSPC)));
        assert!(root.propagation().is_private());
        assert!(child.propagation().is_private());
        assert!(first_group
            .borrow()
            .as_ref()
            .is_some_and(|group| group.upgrade().is_none()));
    }

    #[test]
    fn test_recursive_prepare_capacity_failure_changes_nothing() {
        let root = new_test_mount(MountPropagation::new_private());
        let group_weak = RefCell::new(None);
        let fail_next_reserve = Cell::new(false);
        let result = PropagationChangeTransaction::prepare(
            &root,
            PropagationType::Shared,
            false,
            || {
                let group = PropagationGroup::alloc()?;
                *group_weak.borrow_mut() = Some(Arc::downgrade(&group));
                fail_next_reserve.set(true);
                Ok(group)
            },
            || {
                if fail_next_reserve.replace(false) {
                    Err(SystemError::ENOMEM)
                } else {
                    Ok(())
                }
            },
        );

        assert!(matches!(result, Err(SystemError::ENOMEM)));
        assert!(root.propagation().is_private());
        assert!(group_weak
            .borrow()
            .as_ref()
            .is_some_and(|group| group.upgrade().is_none()));
    }

    #[test]
    fn test_group_allocator_reuses_multiple_holes_in_minimum_order_without_free_storage() {
        let mut allocator = PropagationGroupIdAllocator::new();
        let ids: Vec<_> = (0..6).map(|_| allocator.alloc().unwrap()).collect();
        allocator.free(ids[4]);
        allocator.free(0);
        assert_eq!(allocator.lowest_free, ids[4]);
        allocator.free(ids[1]);
        allocator.free(ids[3]);

        assert_eq!(allocator.lowest_free, ids[1]);
        assert_eq!(allocator.alloc(), Some(ids[1]));
        assert_eq!(allocator.alloc(), Some(ids[3]));
        assert_eq!(allocator.alloc(), Some(ids[4]));
        assert_eq!(allocator.reusable_count, 0);
        assert_eq!(allocator.lowest_free, allocator.next_fresh);
        assert_eq!(allocator.alloc(), Some(ids[5] + 1));
    }

    #[test]
    fn test_recursive_slave_chain_materializes_each_final_edge_once() {
        let mount_a = new_test_mount(MountPropagation::new_shared().unwrap());
        let group_id = mount_a.propagation().peer_group_id();
        let mount_b = shared_copy(&mount_a);
        let mount_c = shared_copy(&mount_a);
        let external = shared_copy(&mount_a);
        attach_test_child(&mount_a, &mount_b);
        attach_test_child(&mount_b, &mount_c);
        for mount in [&mount_a, &mount_b, &mount_c, &external] {
            register_peer(group_id, mount);
        }

        change_mnt_propagation_recursive(&mount_a, PropagationType::Slave, true).unwrap();

        let external_slaves = external.propagation().slaves();
        assert_eq!(external_slaves.len(), 3);
        for mount in [&mount_a, &mount_b, &mount_c] {
            assert!(!mount.propagation().is_shared());
            assert!(mount
                .propagation()
                .master()
                .is_some_and(|master| Arc::ptr_eq(&master, &external)));
            assert_eq!(
                external_slaves
                    .iter()
                    .filter(|slave| Arc::ptr_eq(slave, mount))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn test_make_slave_preserves_new_masters_unrelated_slave() {
        let target = new_test_mount(MountPropagation::new_shared().unwrap());
        let group_id = target.propagation().peer_group_id();
        let peer = shared_copy(&target);
        let unrelated = new_test_mount(MountPropagation::new_slave(Arc::downgrade(&peer)));
        peer.propagation().add_slave(Arc::downgrade(&unrelated));
        register_peer(group_id, &target);
        register_peer(group_id, &peer);

        change_mnt_propagation(&target, PropagationType::Slave).unwrap();

        let slaves = peer.propagation().slaves();
        assert_eq!(slaves.len(), 2);
        assert!(Arc::ptr_eq(&slaves[0], &target));
        assert!(Arc::ptr_eq(&slaves[1], &unrelated));
        assert!(unrelated
            .propagation()
            .master()
            .is_some_and(|master| Arc::ptr_eq(&master, &peer)));
    }

    #[test]
    fn test_nonshared_change_reparents_existing_slave_subtree_like_linux() {
        let master = new_test_mount(MountPropagation::new_private());
        let target = new_test_mount(MountPropagation::new_slave(Arc::downgrade(&master)));
        let child = new_test_mount(MountPropagation::new_slave(Arc::downgrade(&target)));
        master.propagation().add_slave(Arc::downgrade(&target));
        target.propagation().add_slave(Arc::downgrade(&child));

        change_mnt_propagation(&target, PropagationType::Slave).unwrap();
        assert!(target
            .propagation()
            .master()
            .is_some_and(|current| Arc::ptr_eq(&current, &master)));
        assert!(child
            .propagation()
            .master()
            .is_some_and(|current| Arc::ptr_eq(&current, &master)));
        assert!(target.propagation().slaves().is_empty());

        change_mnt_propagation(&target, PropagationType::Private).unwrap();
        assert!(target.propagation().master().is_none());
        assert!(child
            .propagation()
            .master()
            .is_some_and(|current| Arc::ptr_eq(&current, &master)));
        assert!(target.propagation().slaves().is_empty());
    }

    #[test]
    fn test_make_slave_prefers_exact_root_dentry_peer() {
        let target = new_test_mount(MountPropagation::new_shared().unwrap());
        let group = target.propagation().peer_group().unwrap();
        let fallback = new_test_mount(MountPropagation::new_shared_with_group(group));
        let exact = shared_copy(&target);
        let group_id = target.propagation().peer_group_id();
        register_peer(group_id, &target);
        register_peer(group_id, &fallback);
        register_peer(group_id, &exact);

        assert!(!Arc::ptr_eq(&target.root_dentry(), &fallback.root_dentry()));
        assert!(Arc::ptr_eq(&target.root_dentry(), &exact.root_dentry()));
        change_mnt_propagation(&target, PropagationType::Slave).unwrap();

        assert!(target
            .propagation()
            .master()
            .is_some_and(|master| Arc::ptr_eq(&master, &exact)));
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
        let source_child = new_test_mount(MountPropagation::new_private());
        let peer = new_test_mount(MountPropagation::new_private());
        let child = new_test_mount(MountPropagation::new_slave(Arc::downgrade(&source_child)));
        let mountpoint = peer.mountpoint_root_inode();

        source_child.propagation().add_slave(Arc::downgrade(&child));
        child.set_self_mountpoint(Some(mountpoint.clone()));
        peer.attach_top(&mountpoint, child.clone()).unwrap();

        umount_at_peer(&peer, &mountpoint, &source_child).unwrap();

        assert!(peer.lookup_top(&mountpoint).is_none());
        assert!(!child.propagation().is_slave());
        assert!(source_child.propagation().slaves().is_empty());
    }

    #[test]
    fn test_propagate_to_shared_slave_keeps_child_shared_slave() {
        let master = new_test_mount(MountPropagation::new_shared().unwrap());
        register_peer(master.propagation().peer_group_id(), &master);
        let source_child = new_test_mount(MountPropagation::new_shared().unwrap());
        let source_child_group = source_child.propagation().peer_group_id();

        let slave_group = PropagationGroup::alloc().unwrap();
        let slave_a = master.deepcopy(None).unwrap();
        slave_a.propagation().set_private();
        slave_a
            .propagation()
            .set_slave(Some(Arc::downgrade(&master)));
        slave_a
            .propagation()
            .set_shared_with_group(slave_group.clone());
        slave_a.activate().unwrap();
        let slave_b = master.deepcopy(None).unwrap();
        slave_b.propagation().set_private();
        slave_b
            .propagation()
            .set_slave(Some(Arc::downgrade(&master)));
        slave_b
            .propagation()
            .set_shared_with_group(slave_group.clone());
        slave_b.activate().unwrap();

        register_peer(slave_group.id(), &slave_a);
        register_peer(slave_group.id(), &slave_b);
        master.propagation().add_slave(Arc::downgrade(&slave_a));
        master.propagation().add_slave(Arc::downgrade(&slave_b));

        let mountpoint = master.mountpoint_root_inode();
        source_child.set_self_mountpoint(Some(mountpoint.clone()));
        propagate_mount(&master, &mountpoint, &source_child).unwrap();

        let child_a = slave_a
            .lookup_top(
                &slave_a
                    .wrapper_for_dentry(mountpoint.shared_dentry())
                    .unwrap(),
            )
            .expect("slave_a should receive propagated child");
        let child_b = slave_b
            .lookup_top(
                &slave_b
                    .wrapper_for_dentry(mountpoint.shared_dentry())
                    .unwrap(),
            )
            .expect("slave_b should receive propagated child");
        let child_a_prop = child_a.propagation();
        let child_b_prop = child_b.propagation();

        assert!(child_a_prop.is_slave());
        assert!(child_a_prop.is_shared());
        assert!(child_b_prop.is_slave());
        assert!(child_b_prop.is_shared());
        assert_eq!(child_a_prop.peer_group_id(), child_b_prop.peer_group_id());
        assert_ne!(child_a_prop.peer_group_id(), source_child_group);
    }
}
