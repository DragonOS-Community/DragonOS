//! Peer-group identifiers, lifetime ownership, and the global peer discovery index.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use hashbrown::{HashMap, HashSet};
use ida::IdAllocator;
use system_error::SystemError;

use crate::filesystem::vfs::MountFS;
use crate::libs::{rwlock::RwLock, spinlock::SpinLock};

// ============================================================================
// PropagationGroupId
// ============================================================================

/// Linux exposes peer group IDs as positive signed integers in mountinfo.
const PROPAGATION_GROUP_ID_END: usize = i32::MAX as usize + 1;

pub(super) struct PropagationGroupIdAllocator {
    ida: IdAllocator,
    pub(super) next_fresh: usize,
    /// Smallest position that may contain a freed ID below `next_fresh`.
    /// Free only moves this cursor backwards and therefore never allocates.
    pub(super) lowest_free: usize,
    /// Number of holes below `next_fresh`. When the last hole is reused we
    /// jump directly back to the fresh frontier instead of scanning a dense
    /// allocated range under the global spin lock.
    pub(super) reusable_count: usize,
}

impl PropagationGroupIdAllocator {
    pub(super) const fn new() -> Self {
        Self {
            ida: IdAllocator::new(1, PROPAGATION_GROUP_ID_END).unwrap(),
            next_fresh: 1,
            lowest_free: 1,
            reusable_count: 0,
        }
    }

    pub(super) fn alloc(&mut self) -> Option<usize> {
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

    pub(super) fn free(&mut self, id: usize) {
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
    pub(super) fn alloc() -> Result<Arc<Self>, SystemError> {
        let id = PROPAGATION_GROUP_ID_ALLOCATOR
            .lock()
            .alloc()
            .ok_or(SystemError::ENOSPC)?;
        Ok(Arc::new(Self {
            id: PropagationGroupId(id),
        }))
    }

    pub(super) fn id(&self) -> PropagationGroupId {
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

pub(super) enum PreparedPeerGroupState {
    Remove(usize),
    Replace(usize, Vec<Weak<MountFS>>),
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
#[cfg_attr(not(test), allow(dead_code))]
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

pub(super) fn try_snapshot_peer_group<R>(
    group_id: PropagationGroupId,
    before_reserve: &mut R,
) -> Result<Vec<Arc<MountFS>>, SystemError>
where
    R: FnMut() -> Result<(), SystemError>,
{
    let registry = PEER_GROUP_REGISTRY.inner.read();
    let Some(registered) = registry.get(&group_id.data()) else {
        return Ok(Vec::new());
    };
    let mut members = Vec::new();
    if !registered.is_empty() {
        before_reserve()?;
        members
            .try_reserve(registered.len())
            .map_err(|_| SystemError::ENOMEM)?;
    }
    members.extend(
        registered
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|member| {
                let propagation = member.propagation();
                propagation.is_shared() && propagation.peer_group_id() == group_id
            }),
    );
    Ok(members)
}

/// Count group keys that require capacity during transaction commit.
///
/// The caller keeps `MOUNT_LIFECYCLE_LOCK` from this snapshot through
/// `apply_prepared_peer_groups`, so another topology mutation cannot consume
/// the capacity reserved below.
pub(super) fn count_new_peer_group_keys(groups: &[PreparedPeerGroupState]) -> usize {
    let registry = PEER_GROUP_REGISTRY.inner.read();
    groups
        .iter()
        .filter(|group| {
            matches!(group, PreparedPeerGroupState::Replace(group_id, _) if !registry.contains_key(group_id))
        })
        .count()
}

/// Reserve every new registry key before a propagation transaction commits.
pub(super) fn try_reserve_peer_group_keys<R>(
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
    PEER_GROUP_REGISTRY
        .inner
        .write()
        .try_reserve(additional)
        .map_err(|_| SystemError::ENOMEM)
}

/// Build complete peer-group replacements for mounts that will become
/// discoverable at an event commit. Every final member vector and every new
/// registry key is reserved before the first topology edge is published.
pub(super) fn prepare_peer_registrations(
    mounts: &[&Arc<MountFS>],
) -> Result<Vec<PreparedPeerGroupState>, SystemError> {
    let mut additions = Vec::new();
    additions
        .try_reserve(mounts.len())
        .map_err(|_| SystemError::ENOMEM)?;
    for mount in mounts {
        let propagation = mount.propagation();
        let group_id = propagation.peer_group_id();
        if propagation.is_shared() && group_id.is_valid() {
            additions.push((group_id, (*mount).clone()));
        }
    }
    additions.sort_unstable_by_key(|(group_id, _)| group_id.data());

    let mut prepared = Vec::new();
    prepared
        .try_reserve(additions.len())
        .map_err(|_| SystemError::ENOMEM)?;
    let mut index = 0;
    while index < additions.len() {
        let group_id = additions[index].0;
        let start = index;
        while index < additions.len() && additions[index].0 == group_id {
            index += 1;
        }

        let mut before_reserve = || Ok(());
        let current = try_snapshot_peer_group(group_id, &mut before_reserve)?;
        let mut members = Vec::new();
        let final_capacity = current
            .len()
            .checked_add(index - start)
            .ok_or(SystemError::ENOMEM)?;
        members
            .try_reserve(final_capacity)
            .map_err(|_| SystemError::ENOMEM)?;
        // A shared bind can add tens of thousands of peers in one
        // transaction. Deduplicating by scanning the growing vector makes
        // this preflight quadratic and can hold the mount lifecycle lock for
        // minutes. Mount IDs are the stable object identity within a boot, so
        // keep publication order while using an O(1) membership index.
        let mut member_ids = HashSet::new();
        member_ids
            .try_reserve(final_capacity)
            .map_err(|_| SystemError::ENOMEM)?;
        for member in current
            .iter()
            .chain(additions[start..index].iter().map(|(_, mount)| mount))
        {
            if !member_ids.insert(member.mount_id().data()) {
                continue;
            }
            members.push(Arc::downgrade(member));
        }
        prepared.push(PreparedPeerGroupState::Replace(group_id.data(), members));
    }

    let new_group_keys = count_new_peer_group_keys(&prepared);
    try_reserve_peer_group_keys(new_group_keys, &mut || Ok(()))?;
    Ok(prepared)
}

/// Publish prepared registry membership without fallible allocation.
///
/// The lifecycle lock is held by `PropagationChangeTransaction`; keep this
/// registry write lock disjoint from per-mount propagation spin locks.
pub(super) fn apply_prepared_peer_groups(groups: Vec<PreparedPeerGroupState>) {
    let mut registry = PEER_GROUP_REGISTRY.inner.write();
    for group in groups {
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
