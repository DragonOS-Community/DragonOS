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
//! │  - prop_type: PropagationType                               │
//! │  - peer_group_id: PropagationGroupId                        │
//! │  - master/slaves relationships                              │
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

use crate::filesystem::vfs::{mount::MountFlags, MountFS};
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

/// Defines the propagation type for mount points, controlling how mount events are shared.
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
    /// The type of propagation behavior for this mount
    prop_type: PropagationType,
    /// Peer group ID for shared mounts (valid when prop_type is Shared or Slave)
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
                prop_type: PropagationType::Private,
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
                prop_type: PropagationType::Shared,
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
                prop_type: PropagationType::Shared,
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
                prop_type: PropagationType::Slave,
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
                prop_type: PropagationType::Unbindable,
                peer_group_id: PropagationGroupId::NONE,
                master: None,
                slaves: Vec::new(),
            }),
        })
    }

    /// Get the current propagation type
    pub fn prop_type(&self) -> PropagationType {
        self.inner.lock().prop_type
    }

    /// Get the peer group ID (0 if not in a shared group)
    pub fn peer_group_id(&self) -> PropagationGroupId {
        self.inner.lock().peer_group_id
    }

    /// Check if this mount is shared
    pub fn is_shared(&self) -> bool {
        self.inner.lock().prop_type == PropagationType::Shared
    }

    /// Check if this mount is private
    pub fn is_private(&self) -> bool {
        self.inner.lock().prop_type == PropagationType::Private
    }

    /// Check if this mount is a slave
    pub fn is_slave(&self) -> bool {
        self.inner.lock().prop_type == PropagationType::Slave
    }

    /// Check if this mount is unbindable
    pub fn is_unbindable(&self) -> bool {
        self.inner.lock().prop_type == PropagationType::Unbindable
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
        if inner.prop_type != PropagationType::Shared {
            // If transitioning from slave, disconnect from master
            if inner.prop_type == PropagationType::Slave {
                inner.master = None;
            }
            // Allocate new group ID if needed
            if !inner.peer_group_id.is_valid() {
                inner.peer_group_id = PropagationGroupId::alloc();
            }
            inner.prop_type = PropagationType::Shared;
        }
    }

    /// Set shared with a specific group ID (used for propagation)
    pub fn set_shared_with_group(&self, group_id: PropagationGroupId) {
        let mut inner = self.inner.lock();
        if inner.prop_type == PropagationType::Slave {
            inner.master = None;
        }
        inner.peer_group_id = group_id;
        inner.prop_type = PropagationType::Shared;
    }

    /// Change propagation type to private
    ///
    /// Disconnects from peer group and master relationships.
    pub fn set_private(&self) {
        let mut inner = self.inner.lock();
        if inner.prop_type == PropagationType::Slave {
            inner.master = None;
        }
        // Note: We keep peer_group_id for potential reuse, but mark as private
        inner.prop_type = PropagationType::Private;
    }

    /// Change propagation type to slave
    ///
    /// If currently shared, becomes a slave of the peer group.
    /// This is typically used when doing `mount --make-slave`.
    pub fn set_slave(&self, master: Option<Weak<MountFS>>) {
        let mut inner = self.inner.lock();
        inner.prop_type = PropagationType::Slave;
        inner.master = master;
        // Keep peer_group_id so slaves can still receive events from the group
    }

    /// Change propagation type to unbindable
    pub fn set_unbindable(&self) {
        let mut inner = self.inner.lock();
        if inner.prop_type == PropagationType::Slave {
            inner.master = None;
        }
        inner.prop_type = PropagationType::Unbindable;
        inner.peer_group_id = PropagationGroupId::NONE;
    }

    /// Add a slave mount
    pub fn add_slave(&self, slave: Weak<MountFS>) {
        let mut inner = self.inner.lock();
        inner.slaves.push(slave);
    }

    /// Remove a slave mount
    pub fn remove_slave(&self, slave: &Weak<MountFS>) {
        let mut inner = self.inner.lock();
        inner.slaves.retain(|s| !Weak::ptr_eq(s, slave));
    }

    /// Get all valid slave mounts
    pub fn slaves(&self) -> Vec<Arc<MountFS>> {
        let inner = self.inner.lock();
        inner.slaves.iter().filter_map(|s| s.upgrade()).collect()
    }

    /// Clean up stale (dropped) slave references
    pub fn cleanup_stale_slaves(&self) {
        let mut inner = self.inner.lock();
        inner.slaves.retain(|s| s.upgrade().is_some());
    }

    /// Clone the propagation state for a new mount copy.
    ///
    /// When copying a mount (e.g., for namespace cloning), the new mount
    /// should inherit the propagation type but may need different relationships.
    pub fn clone_for_copy(&self) -> Arc<Self> {
        let inner = self.inner.lock();
        Arc::new(Self {
            inner: SpinLock::new(MountPropagationInner {
                prop_type: inner.prop_type,
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
        use alloc::string::String;

        let inner = self.inner.lock();
        match inner.prop_type {
            PropagationType::Shared => {
                if inner.peer_group_id.is_valid() {
                    alloc::format!("shared:{}", inner.peer_group_id.0)
                } else {
                    String::new()
                }
            }
            PropagationType::Slave => {
                if inner.peer_group_id.is_valid() {
                    alloc::format!("master:{}", inner.peer_group_id.0)
                } else {
                    String::new()
                }
            }
            PropagationType::Private | PropagationType::Unbindable => String::new(),
        }
    }
}

impl Clone for MountPropagation {
    fn clone(&self) -> Self {
        let inner = self.inner.lock();
        Self {
            inner: SpinLock::new(MountPropagationInner {
                prop_type: inner.prop_type,
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
    let old_type = propagation.prop_type();
    let old_group_id = propagation.peer_group_id();

    // If transitioning FROM shared, unregister from the peer group first
    if old_type == PropagationType::Shared && prop_type != PropagationType::Shared {
        unregister_peer(old_group_id, mount);
    }

    match prop_type {
        PropagationType::Shared => {
            propagation.set_shared();
            // Register in peer group if newly shared
            if old_type != PropagationType::Shared {
                let new_group_id = propagation.peer_group_id();
                register_peer(new_group_id, mount);
            }
        }
        PropagationType::Private => {
            propagation.set_private();
        }
        PropagationType::Slave => {
            // When making a mount a slave, it should become a slave of its
            // current peer group (if any). For simplicity, we just set it as slave.
            propagation.set_slave(None);
        }
        PropagationType::Unbindable => {
            propagation.set_unbindable();
        }
    }

    Ok(())
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
///
/// # Returns
/// * `Ok(())` on success
/// * `Err(SystemError)` on failure (partial propagation may have occurred)
pub fn propagate_mount(
    source_mnt: &Arc<MountFS>,
    mountpoint_id: InodeId,
    new_child: &Arc<MountFS>,
) -> Result<(), SystemError> {
    let propagation = source_mnt.propagation();
    let group_id = propagation.peer_group_id();

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

    // Propagate to each peer
    for peer in peers {
        let peer_id: usize = peer.mount_id().into();
        if propagated.contains(&peer_id) {
            continue;
        }
        propagated.insert(peer_id);

        if let Err(e) = propagate_one(&peer, mountpoint_id, new_child) {
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

        if let Err(e) = propagate_one(&slave, mountpoint_id, new_child) {
            log::warn!("propagate_mount: failed to propagate to slave: {:?}", e);
        }

        // Also propagate to slaves of slaves (recursive)
        propagate_to_slaves(&slave, mountpoint_id, new_child, &mut propagated);
    }

    Ok(())
}

/// Propagate mount to a single target mount.
///
/// The cloned child mount joins the SAME peer group as the source child,
/// so that all propagated children can propagate events to each other.
fn propagate_one(
    target_mnt: &Arc<MountFS>,
    mountpoint_id: InodeId,
    source_child: &Arc<MountFS>,
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

    // The cloned child should join the SAME peer group as source_child,
    // NOT the target parent's group. This way, all propagated children
    // form a peer group and can propagate events to each other.
    let source_child_prop = source_child.propagation();
    if source_child_prop.is_shared() {
        let group_id = source_child_prop.peer_group_id();
        cloned_child.propagation().set_shared_with_group(group_id);
        register_peer(group_id, &cloned_child);
    }

    // Add the cloned mount to the target
    target_mnt.add_mount(mountpoint_id, cloned_child.clone())?;

    Ok(())
}

/// Recursively propagate to slaves.
fn propagate_to_slaves(
    mnt: &Arc<MountFS>,
    mountpoint_id: InodeId,
    source_child: &Arc<MountFS>,
    propagated: &mut HashSet<usize>,
) {
    let prop = mnt.propagation();
    for slave in prop.slaves() {
        let slave_id: usize = slave.mount_id().into();
        if propagated.contains(&slave_id) {
            continue;
        }
        propagated.insert(slave_id);

        if let Err(e) = propagate_one(&slave, mountpoint_id, source_child) {
            log::warn!("propagate_to_slaves: failed: {:?}", e);
        }

        // Recurse
        propagate_to_slaves(&slave, mountpoint_id, source_child, propagated);
    }
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
    if let Some(child) = peer_mnt.mountpoints().remove(&mountpoint_id) {
        // Unregister the child from its peer group if shared
        let child_prop = child.propagation();
        if child_prop.is_shared() {
            unregister_peer(child_prop.peer_group_id(), &child);
        }
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
}
