//! Per-mount propagation state and master/slave relationship maintenance.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use system_error::SystemError;

use crate::filesystem::vfs::MountFS;
use crate::libs::spinlock::SpinLock;

use super::group::{get_peers, unregister_peer, PropagationGroup, PropagationGroupId};

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

bitflags! {
    /// Mount propagation flags.
    ///
    /// Linux treats shared and slave as orthogonal state: shared is a flag,
    /// while slave is represented by the presence of a master mount.  Keep the
    /// same model here so a mount can be both shared and slave.
    pub(super) struct PropagationFlags: u32 {
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

/// Fallible graph-capture snapshot. Strong references are acquired while the
/// source state is locked so stale weak entries and object lifetimes match the
/// pre-refactor capture semantics.
pub(super) struct CapturedMountPropagationState {
    pub(super) flags: PropagationFlags,
    pub(super) peer_group: Option<Arc<PropagationGroup>>,
    pub(super) master: Option<Arc<MountFS>>,
    pub(super) candidate_slaves: Vec<Arc<MountFS>>,
}

/// Fully allocated final state published by a propagation change transaction.
pub(super) struct PreparedMountPropagationState {
    pub(super) flags: PropagationFlags,
    pub(super) peer_group: Option<Arc<PropagationGroup>>,
    pub(super) master: Option<Weak<MountFS>>,
    pub(super) slaves: Vec<Weak<MountFS>>,
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

    pub(super) fn try_snapshot_for_graph<R>(
        &self,
        before_reserve: &mut R,
    ) -> Result<CapturedMountPropagationState, SystemError>
    where
        R: FnMut() -> Result<(), SystemError>,
    {
        let inner = self.inner.lock();
        let master = inner.master.as_ref().and_then(Weak::upgrade);
        let mut candidate_slaves = Vec::new();
        if !inner.slaves.is_empty() {
            before_reserve()?;
            candidate_slaves
                .try_reserve(inner.slaves.len())
                .map_err(|_| SystemError::ENOMEM)?;
        }
        candidate_slaves.extend(inner.slaves.iter().filter_map(Weak::upgrade));
        Ok(CapturedMountPropagationState {
            flags: inner.flags,
            peer_group: inner.peer_group.clone(),
            master,
            candidate_slaves,
        })
    }

    pub(super) fn replace_state(
        &self,
        prepared: PreparedMountPropagationState,
    ) -> PreparedMountPropagationState {
        let replacement = MountPropagationInner {
            flags: prepared.flags,
            peer_group: prepared.peer_group,
            master: prepared.master,
            slaves: prepared.slaves,
        };
        let old = core::mem::replace(&mut *self.inner.lock(), replacement);
        PreparedMountPropagationState {
            flags: old.flags,
            peer_group: old.peer_group,
            master: old.master,
            slaves: old.slaves,
        }
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
                parts.push(alloc::format!("shared:{}", group.id().data()));
            }
        }
        if let Some(master) = inner.master.as_ref().and_then(|w| w.upgrade()) {
            let master_group = master.propagation().peer_group_id();
            if master_group.is_valid() {
                parts.push(alloc::format!("master:{}", master_group.data()));
                if let Some(dom) = dominating_peer_group_id(&master) {
                    if dom != master_group.data() {
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
            dominating = Some(group.data());
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
