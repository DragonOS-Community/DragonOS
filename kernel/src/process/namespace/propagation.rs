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
use core::sync::atomic::{AtomicUsize, Ordering};
use hashbrown::HashSet;
use system_error::SystemError;

use crate::filesystem::vfs::{mount::MountFlags, mount::MountPath, MountFS};
use crate::libs::rwlock::RwLock;

// ============================================================================
// PropagationGroupId
// ============================================================================

/// Global propagation group ID counter.
/// Group IDs start from 1 (0 means no group/invalid).
static NEXT_GROUP_ID: AtomicUsize = AtomicUsize::new(1);

int_like!(PropagationGroupId, usize);

impl PropagationGroupId {
    /// Invalid/unset group ID
    pub const NONE: Self = PropagationGroupId(0);

    /// Allocate a new unique group ID (monotonically increasing)
    pub fn alloc() -> Self {
        let id = NEXT_GROUP_ID.fetch_add(1, Ordering::Relaxed);
        PropagationGroupId(id)
    }

    /// Check if this is a valid (non-zero) group ID
    pub fn is_valid(&self) -> bool {
        self.0 != 0
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
            if let Some(m) = w.upgrade() {
                !Arc::ptr_eq(&m, mount)
            } else {
                false
            }
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
                if let Some(m) = w.upgrade() {
                    !Arc::ptr_eq(&m, mount)
                } else {
                    false
                }
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
                .filter(|m| !Arc::ptr_eq(m, exclude))
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
            peers.iter().filter_map(|w| w.upgrade()).collect()
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
            peers.iter().filter(|w| w.upgrade().is_some()).count()
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
            peers.retain(|w| w.upgrade().is_some());
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
    /// Peer group ID for shared mounts.
    peer_group_id: PropagationGroupId,
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
                peer_group_id: PropagationGroupId::NONE,
                master: None,
                slaves: Vec::new(),
            }),
        })
    }

    /// Create a new shared propagation with a newly allocated group ID
    pub fn new_shared() -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(MountPropagationInner {
                flags: PropagationFlags::SHARED,
                peer_group_id: PropagationGroupId::alloc(),
                master: None,
                slaves: Vec::new(),
            }),
        })
    }

    /// Create a new shared propagation with a specific group ID
    pub fn new_shared_with_group(group_id: PropagationGroupId) -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(MountPropagationInner {
                flags: PropagationFlags::SHARED,
                peer_group_id: group_id,
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
                peer_group_id: PropagationGroupId::NONE,
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
                peer_group_id: PropagationGroupId::NONE,
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
        self.inner.lock().peer_group_id
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
    pub fn set_shared(&self) {
        let mut inner = self.inner.lock();
        inner.flags.remove(PropagationFlags::UNBINDABLE);
        if !inner.peer_group_id.is_valid() {
            inner.peer_group_id = PropagationGroupId::alloc();
        }
        inner.flags.insert(PropagationFlags::SHARED);
    }

    /// Set shared with a specific group ID (used for propagation)
    pub fn set_shared_with_group(&self, group_id: PropagationGroupId) {
        let mut inner = self.inner.lock();
        inner.flags.remove(PropagationFlags::UNBINDABLE);
        inner.peer_group_id = group_id;
        inner.flags.insert(PropagationFlags::SHARED);
    }

    /// Clear the shared flag without changing slave/master relationships.
    pub fn clear_shared(&self) {
        self.inner.lock().flags.remove(PropagationFlags::SHARED);
    }

    /// Clear the peer group ID.
    pub fn clear_group_id(&self) {
        self.inner.lock().peer_group_id = PropagationGroupId::NONE;
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
        inner.peer_group_id = PropagationGroupId::NONE;
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
        inner.peer_group_id = PropagationGroupId::NONE;
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
                peer_group_id: inner.peer_group_id,
                master: inner.master.clone(),
                slaves: Vec::new(), // New copy starts with no slaves
            }),
        })
    }

    /// Get propagation info string for /proc/self/mountinfo format
    ///
    /// Returns a string like "shared:1" or "master:2" or empty for private.
    pub fn info_string(&self) -> alloc::string::String {
        let inner = self.inner.lock();
        let mut parts = Vec::new();
        if inner.flags.contains(PropagationFlags::SHARED) && inner.peer_group_id.is_valid() {
            parts.push(alloc::format!("shared:{}", inner.peer_group_id.0));
        }
        if let Some(master) = inner.master.as_ref().and_then(|w| w.upgrade()) {
            let master_group = master.propagation().peer_group_id();
            if master_group.is_valid() {
                parts.push(alloc::format!("master:{}", master_group.0));
            }
        }
        parts.join(" ")
    }
}

impl Clone for MountPropagation {
    fn clone(&self) -> Self {
        let inner = self.inner.lock();
        Self {
            inner: SpinLock::new(MountPropagationInner {
                flags: inner.flags,
                peer_group_id: inner.peer_group_id,
                master: inner.master.clone(),
                slaves: inner.slaves.clone(),
            }),
        }
    }
}

/// Convert mount flags to propagation type
///
/// Returns the propagation type indicated by the flags, or None if
/// no propagation flags are set.
pub fn flags_to_propagation_type(flags: MountFlags) -> Option<PropagationType> {
    if flags.contains(MountFlags::SHARED) {
        Some(PropagationType::Shared)
    } else if flags.contains(MountFlags::SLAVE) {
        Some(PropagationType::Slave)
    } else if flags.contains(MountFlags::PRIVATE) {
        Some(PropagationType::Private)
    } else if flags.contains(MountFlags::UNBINDABLE) {
        Some(PropagationType::Unbindable)
    } else {
        None
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
pub fn change_mnt_propagation(
    mount: &Arc<MountFS>,
    prop_type: PropagationType,
) -> Result<(), SystemError> {
    let propagation = mount.propagation();

    match prop_type {
        PropagationType::Shared => {
            let was_shared = propagation.is_shared();
            propagation.set_shared();
            if !was_shared {
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

    Ok(())
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
        unregister_peer(old_group_id, mount);
        propagation.clear_shared();
        propagation.clear_group_id();
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
    // Change the root mount
    change_mnt_propagation(mount, prop_type)?;

    if recursive {
        // Change all child mounts
        let mountpoints = mount.mountpoints();
        for child_mount in mountpoints.values() {
            change_mnt_propagation_recursive(child_mount, prop_type, true)?;
        }
    }

    Ok(())
}

// ============================================================================
// Mount Propagation Functions
// ============================================================================

use crate::filesystem::vfs::InodeId;
use crate::libs::spinlock::SpinLock;

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
pub fn propagate_mount(
    source_mnt: &Arc<MountFS>,
    mountpoint_id: InodeId,
    new_child: &Arc<MountFS>,
    mount_path: &Arc<MountPath>,
) -> Result<(), SystemError> {
    let propagation = source_mnt.propagation();
    let group_id = propagation.peer_group_id();
    let source_parent_path = source_mnt
        .namespace()
        .and_then(|ns| ns.mount_list().get_mount_path_by_mountfs(source_mnt));

    // Only propagate for shared mounts
    if !propagation.is_shared() {
        return Ok(());
    }

    // log::debug!(
    //     "propagate_mount: propagating from group {} to peers",
    //     group_id.0
    // );

    // Get all peers (excluding source)
    let peers = get_peers(group_id, source_mnt);

    // Track which mounts we've already propagated to (to avoid duplicates)
    let mut propagated: HashSet<usize> = HashSet::new();
    propagated.insert(source_mnt.mount_id().into());
    let mut slave_child_groups = BTreeMap::new();

    // Propagate to each peer
    for peer in peers {
        let peer_id: usize = peer.mount_id().into();
        if propagated.contains(&peer_id) {
            continue;
        }
        propagated.insert(peer_id);

        if let Err(e) = propagate_one(
            &peer,
            group_id,
            mountpoint_id,
            new_child,
            mount_path,
            &source_parent_path,
            &mut slave_child_groups,
        ) {
            log::warn!("propagate_mount: failed to propagate to peer: {:?}", e);
            // Continue with other peers even if one fails
        }
    }

    // Propagate to slaves
    for slave in propagation.slaves() {
        let slave_id: usize = slave.mount_id().into();
        if propagated.contains(&slave_id) {
            continue;
        }
        propagated.insert(slave_id);

        if let Err(e) = propagate_one(
            &slave,
            group_id,
            mountpoint_id,
            new_child,
            mount_path,
            &source_parent_path,
            &mut slave_child_groups,
        ) {
            log::warn!("propagate_mount: failed to propagate to slave: {:?}", e);
        }

        // Also propagate to slaves of slaves (recursive)
        propagate_to_slaves(
            &slave,
            mountpoint_id,
            new_child,
            mount_path,
            &source_parent_path,
            &mut propagated,
            &mut slave_child_groups,
        );
    }

    Ok(())
}

/// Propagate mount to a single target mount.
///
/// The cloned child mount joins the SAME peer group as the source child,
/// so that all propagated children can propagate events to each other.
fn propagate_one(
    target_mnt: &Arc<MountFS>,
    source_group_id: PropagationGroupId,
    mountpoint_id: InodeId,
    source_child: &Arc<MountFS>,
    mount_path: &Arc<MountPath>,
    source_parent_path: &Option<Arc<MountPath>>,
    slave_child_groups: &mut BTreeMap<usize, PropagationGroupId>,
) -> Result<(), SystemError> {
    // Check if the target has the same mountpoint
    let target_mountpoints = target_mnt.mountpoints();
    if target_mountpoints.contains_key(&mountpoint_id) {
        // Already has something mounted here
        return Ok(());
    }
    drop(target_mountpoints);

    // Clone the child mount for this target
    let cloned_child = source_child.deepcopy(None);

    // Peer targets receive a peer of the propagated child. Slave targets
    // receive a slave of the propagated child; joining the master's peer group
    // would incorrectly allow reverse propagation back to the master side.
    let source_child_prop = source_child.propagation();
    let target_prop = target_mnt.propagation();
    let target_is_source_peer =
        target_prop.is_shared() && target_prop.peer_group_id() == source_group_id;
    if target_is_source_peer && source_child_prop.is_shared() {
        let group_id = source_child_prop.peer_group_id();
        cloned_child.propagation().set_shared_with_group(group_id);
        register_peer(group_id, &cloned_child);
    } else {
        let cloned_prop = cloned_child.propagation();
        if cloned_prop.is_shared() {
            unregister_peer(cloned_prop.peer_group_id(), &cloned_child);
        }
        cloned_prop.set_private();
        cloned_prop.set_slave(Some(Arc::downgrade(source_child)));
        source_child_prop.add_slave(Arc::downgrade(&cloned_child));

        if target_prop.is_shared() {
            let target_group_id = target_prop.peer_group_id();
            let child_group_id = *slave_child_groups
                .entry(target_group_id.data())
                .or_insert_with(PropagationGroupId::alloc);
            cloned_prop.set_shared_with_group(child_group_id);
            register_peer(child_group_id, &cloned_child);
        }
    }

    // Add the cloned mount to the target's mountpoints
    if let Some(source_mountpoint) = source_child.self_mountpoint() {
        cloned_child.set_self_mountpoint(Some(
            source_mountpoint.clone_with_new_mount_fs(target_mnt.clone()),
        ));
    }
    target_mnt.add_mount(mountpoint_id, cloned_child.clone())?;

    // 传播子挂载必须继承目标挂载点的 namespace，
    // 否则后续 is_belongs_to_mntns() 检查会因 namespace 为 None 而误判 EINVAL。
    if let Some(ns) = target_mnt.namespace() {
        cloned_child.set_namespace(Arc::downgrade(&ns));
        let target_mount_path = propagated_mount_path(source_parent_path, mount_path, target_mnt);
        ns.add_mount(Some(mountpoint_id), target_mount_path, cloned_child)
            .inspect_err(|_e| {
                // 回滚 mountpoints 中已插入的克隆挂载。
                target_mnt.mountpoints().remove(&mountpoint_id);
            })?;
    }

    Ok(())
}

/// Recursively propagate to slaves.
fn propagate_to_slaves(
    mnt: &Arc<MountFS>,
    mountpoint_id: InodeId,
    source_child: &Arc<MountFS>,
    mount_path: &Arc<MountPath>,
    source_parent_path: &Option<Arc<MountPath>>,
    propagated: &mut HashSet<usize>,
    slave_child_groups: &mut BTreeMap<usize, PropagationGroupId>,
) {
    let prop = mnt.propagation();
    for slave in prop.slaves() {
        let slave_id: usize = slave.mount_id().into();
        if propagated.contains(&slave_id) {
            continue;
        }
        propagated.insert(slave_id);

        let source_group_id = source_child.propagation().peer_group_id();
        if let Err(e) = propagate_one(
            &slave,
            source_group_id,
            mountpoint_id,
            source_child,
            mount_path,
            source_parent_path,
            slave_child_groups,
        ) {
            log::warn!("propagate_to_slaves: failed: {:?}", e);
        }

        // Recurse
        propagate_to_slaves(
            &slave,
            mountpoint_id,
            source_child,
            mount_path,
            source_parent_path,
            propagated,
            slave_child_groups,
        );
    }
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

    // Get all peers in the group (including this mount, for completeness)
    let all_peers = get_all_peers(group_id);

    // Track which mounts we've processed
    let mut processed: HashSet<usize> = HashSet::new();
    processed.insert(parent_mnt.mount_id().into());

    // Propagate to each peer
    for peer in all_peers {
        let peer_id: usize = peer.mount_id().into();
        if processed.contains(&peer_id) {
            continue;
        }
        processed.insert(peer_id);

        // Try to umount at the peer
        if let Err(e) = umount_at_peer(&peer, mountpoint_id) {
            log::debug!("propagate_umount: peer umount skipped: {:?}", e);
            // Continue with other peers even if one fails
        }
    }

    // Propagate to slaves
    for slave in propagation.slaves() {
        let slave_id: usize = slave.mount_id().into();
        if processed.contains(&slave_id) {
            continue;
        }
        processed.insert(slave_id);

        if let Err(e) = umount_at_peer(&slave, mountpoint_id) {
            log::debug!("propagate_umount: slave umount skipped: {:?}", e);
        }

        // Recurse to slaves of slaves
        propagate_umount_to_slaves(&slave, mountpoint_id, &mut processed);
    }

    Ok(())
}

/// Umount at a specific peer mount.
fn umount_at_peer(peer_mnt: &Arc<MountFS>, mountpoint_id: InodeId) -> Result<(), SystemError> {
    if peer_mnt.mountpoints().contains_key(&mountpoint_id) {
        let Some(child) = peer_mnt.mountpoints().remove(&mountpoint_id) else {
            return Ok(());
        };
        // Unregister the child from its peer group if shared
        let child_prop = child.propagation();
        if child_prop.is_shared() {
            unregister_peer(child_prop.peer_group_id(), &child);
        }
        if let Some(master) = child_prop.master() {
            master.propagation().remove_slave(&Arc::downgrade(&child));
            child_prop.set_master(None);
        }

        child.set_self_mountpoint(None);

        // 先从 mount_list 移除，再清 namespace，避免 "namespace=None 但 mount_list 仍有记录" 的 TOCTOU 中间态。
        if let Some(ns) = child.namespace() {
            if let Some(mp) = ns.mount_list().get_mount_path_by_mountfs(&child) {
                ns.remove_mount(mp.as_str());
            }
        }
        child.clear_namespace();
        // log::debug!("umount_at_peer: removed mount at {:?}", mountpoint_id);
    }
    Ok(())
}

/// Recursively propagate umount to slaves.
fn propagate_umount_to_slaves(
    mnt: &Arc<MountFS>,
    mountpoint_id: InodeId,
    processed: &mut HashSet<usize>,
) {
    let prop = mnt.propagation();
    for slave in prop.slaves() {
        let slave_id: usize = slave.mount_id().into();
        if processed.contains(&slave_id) {
            continue;
        }
        processed.insert(slave_id);

        if let Err(e) = umount_at_peer(&slave, mountpoint_id) {
            log::debug!("propagate_umount_to_slaves: failed: {:?}", e);
        }

        // Recurse
        propagate_umount_to_slaves(&slave, mountpoint_id, processed);
    }
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
        prop.set_shared();
        assert!(prop.is_shared());
        assert!(prop.peer_group_id().is_valid());

        prop.set_private();
        assert!(prop.is_private());

        prop.set_unbindable();
        assert!(prop.is_unbindable());
    }

    fn new_test_mount(propagation: Arc<MountPropagation>) -> Arc<MountFS> {
        MountFS::new(
            RamFS::new(),
            None,
            None,
            propagation,
            None,
            MountFlags::empty(),
            None,
        )
    }

    #[test]
    fn test_make_slave_selects_peer_as_master() {
        let prop_a = MountPropagation::new_shared();
        let group_id = prop_a.peer_group_id();
        let prop_b = MountPropagation::new_shared_with_group(group_id);
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
        let master = new_test_mount(MountPropagation::new_shared());
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
        let master = new_test_mount(MountPropagation::new_shared());
        let source_child = new_test_mount(MountPropagation::new_shared());
        let source_child_group = source_child.propagation().peer_group_id();

        let slave_group = PropagationGroupId::alloc();
        let slave_a_prop = MountPropagation::new_slave(Arc::downgrade(&master));
        slave_a_prop.set_shared_with_group(slave_group);
        let slave_b_prop = MountPropagation::new_slave(Arc::downgrade(&master));
        slave_b_prop.set_shared_with_group(slave_group);
        let slave_a = new_test_mount(slave_a_prop);
        let slave_b = new_test_mount(slave_b_prop);

        register_peer(slave_group, &slave_a);
        register_peer(slave_group, &slave_b);
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
}
