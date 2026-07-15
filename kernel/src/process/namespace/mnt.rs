use crate::{
    filesystem::vfs::{
        mount::{MountFSInode, MountFlags, MOUNT_LIFECYCLE_LOCK},
        FileSystem, IndexNode, MountFS,
    },
    libs::{casting::DowncastArc, once::Once, rwsem::RwSem},
    process::{fork::CloneFlags, namespace::NamespaceType, ProcessManager},
};
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use system_error::SystemError;

use super::{
    nsproxy::NsCommon,
    propagation::{
        abort_moved_tree_propagation_locked, commit_moved_tree_propagation_locked,
        prepare_moved_tree_propagation_locked, MountPropagation, PreparedRegistrations,
    },
    user_namespace::UserNamespace,
    NamespaceOps,
};

static mut INIT_MNT_NAMESPACE: Option<Arc<MntNamespace>> = None;

const DEFAULT_MOUNT_MAX: u32 = 100_000;
static MOUNT_MAX: AtomicU32 = AtomicU32::new(DEFAULT_MOUNT_MAX);

#[cfg(test)]
static FAIL_COPY_REGISTRATION_PREPARE: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

pub fn mount_max() -> u32 {
    MOUNT_MAX.load(Ordering::Relaxed)
}

pub fn set_mount_max(value: u32) -> Result<(), SystemError> {
    if value == 0 || value > i32::MAX as u32 {
        return Err(SystemError::EINVAL);
    }
    MOUNT_MAX.store(value, Ordering::Relaxed);
    Ok(())
}

#[derive(Debug, Default)]
struct MountCountState {
    mounts: u32,
    pending_mounts: u32,
}

impl MountCountState {
    fn reserve(&mut self, amount: u32, limit: u32) -> Result<(), SystemError> {
        let used = self
            .mounts
            .checked_add(self.pending_mounts)
            .ok_or(SystemError::ENOSPC)?;
        let remaining = limit.checked_sub(used).ok_or(SystemError::ENOSPC)?;
        if amount > remaining {
            return Err(SystemError::ENOSPC);
        }
        self.pending_mounts = self
            .pending_mounts
            .checked_add(amount)
            .ok_or(SystemError::ENOSPC)?;
        Ok(())
    }

    fn commit(&mut self, amount: u32) {
        self.pending_mounts = self
            .pending_mounts
            .checked_sub(amount)
            .expect("mount reservation commit exceeds pending count");
        self.mounts = self
            .mounts
            .checked_add(amount)
            .expect("committed mount count overflow after validated reservation");
    }

    fn abort(&mut self, amount: u32) {
        self.pending_mounts = self
            .pending_mounts
            .checked_sub(amount)
            .expect("mount reservation rollback exceeds pending count");
    }

    fn release(&mut self, amount: u32) {
        self.mounts = self
            .mounts
            .checked_sub(amount)
            .expect("mount teardown exceeds committed count");
    }
}

/// Initialize the root mount namespace
pub fn mnt_namespace_init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        INIT_MNT_NAMESPACE = Some(MntNamespace::new_root());
    });
}

/// Get the global root mount namespace
pub fn root_mnt_namespace() -> Arc<MntNamespace> {
    unsafe {
        INIT_MNT_NAMESPACE
            .as_ref()
            .expect("Mount namespace not initialized")
            .clone()
    }
}

pub struct MntNamespace {
    ns_common: NsCommon,
    self_ref: Weak<MntNamespace>,
    _user_ns: Arc<UserNamespace>,
    inner: RwSem<InnerMntNamespace>,
}

pub struct InnerMntNamespace {
    _dead: bool,
    root_mountfs: Arc<MountFS>,
    mount_count: MountCountState,
    /// Exact old-mount to copied-mount projection used to rebind fs_struct
    /// root/pwd after CLONE_NEWNS. This is object identity, never a pathname.
    copy_sources: Vec<(Weak<MountFS>, Weak<MountFS>)>,
}

pub(crate) struct MountCountReservation {
    namespace: Arc<MntNamespace>,
    mounts: Vec<Arc<MountFS>>,
    pending: bool,
}

impl MountCountReservation {
    pub(crate) fn commit(mut self) {
        for mount in &self.mounts {
            assert!(
                mount.can_mark_namespace_accounted(&self.namespace),
                "mount reservation ownership changed before commit"
            );
        }

        let amount = u32::try_from(self.mounts.len())
            .expect("validated mount reservation length remains representable");
        self.namespace.inner.write().mount_count.commit(amount);
        for mount in &self.mounts {
            mount.mark_namespace_accounted(&self.namespace);
        }
        self.pending = false;
    }
}

impl Drop for MountCountReservation {
    fn drop(&mut self) {
        if !self.pending {
            return;
        }
        let amount = u32::try_from(self.mounts.len())
            .expect("validated mount reservation length remains representable");
        self.namespace.inner.write().mount_count.abort(amount);
    }
}

fn tree_contains_unbindable(root: &Arc<MountFS>) -> bool {
    if root.propagation().is_unbindable() {
        return true;
    }

    let mut pending = root.mount_children();
    while let Some(mount) = pending.pop() {
        if mount.propagation().is_unbindable() {
            return true;
        }
        pending.extend(mount.mount_children());
    }
    false
}

impl NamespaceOps for MntNamespace {
    fn ns_common(&self) -> &NsCommon {
        &self.ns_common
    }
}

impl MntNamespace {
    fn new_root() -> Arc<Self> {
        let ramfs = crate::filesystem::ramfs::RamFS::new();
        let ramfs = MountFS::new(
            ramfs,
            None,
            None,
            MountPropagation::new_private(),
            None,
            MountFlags::empty(),
            None,
        );

        let result = Arc::new_cyclic(|self_ref| Self {
            ns_common: NsCommon::new(0, NamespaceType::Mount),
            self_ref: self_ref.clone(),
            _user_ns: super::user_namespace::INIT_USER_NAMESPACE.clone(),
            inner: RwSem::new(InnerMntNamespace {
                root_mountfs: ramfs.clone(),
                mount_count: MountCountState::default(),
                copy_sources: Vec::new(),
                _dead: false,
            }),
        });

        {
            let _topology = MOUNT_LIFECYCLE_LOCK.lock();
            result
                .add_mount(None, None, ramfs)
                .expect("Failed to add root mount");
        }

        return result;
    }

    pub fn user_ns(&self) -> &Arc<UserNamespace> {
        &self._user_ns
    }

    /// Forcibly replace the root mount filesystem of this MountNamespace.
    ///
    /// This method is only for use during DragonOS initialization.
    pub fn force_change_root_mountfs(&self, new_root: Arc<MountFS>) {
        let mut inner_guard = self.inner.write();
        new_root.set_namespace(self.self_ref.clone());
        let old_root = core::mem::replace(&mut inner_guard.root_mountfs, new_root);
        assert!(
            old_root.take_namespace_accounted(&self.self_ref),
            "the old namespace root must own one committed mount slot"
        );
        inner_guard
            .root_mountfs
            .mark_namespace_accounted_weak(&self.self_ref);
        drop(inner_guard);

        assert!(
            old_root.mount_children().is_empty(),
            "initial root replacement requires every child mount to be migrated"
        );
        old_root.clear_namespace();
        old_root.deactivate();
    }

    pub fn pivot_root(
        &self,
        old_root: Arc<MountFS>,
        new_root: Arc<MountFS>,
        put_old_mountpoint: Arc<MountFSInode>,
    ) -> Result<(), SystemError> {
        let new_root_mountpoint = new_root.self_mountpoint().ok_or(SystemError::EINVAL)?;
        let new_root_parent = new_root_mountpoint.mount_fs();
        let put_old_parent = put_old_mountpoint.mount_fs();
        let _topology = MOUNT_LIFECYCLE_LOCK.lock();
        let mut inner_guard = self.inner.write();
        let old_root_mountpoint = old_root.self_mountpoint();
        let old_root_tucked_under = old_root.is_tucked_under();
        let new_root_tucked_under = new_root.is_tucked_under();
        let namespace_root = Self::root_mntfs_locked(&inner_guard);
        if !old_root.is_belongs_to_mntns(&self.self_ref.upgrade().ok_or(SystemError::EINVAL)?)
            || new_root.is_locked()
        {
            return Err(SystemError::EINVAL);
        }

        if put_old_parent.lookup_top(&put_old_mountpoint).is_some() {
            return Err(SystemError::EBUSY);
        }

        new_root_parent.detach_exact(&new_root)?;

        let old_parent = old_root_mountpoint
            .as_ref()
            .map(|mountpoint| mountpoint.mount_fs());
        if let (Some(parent), Some(_)) = (&old_parent, &old_root_mountpoint) {
            if let Err(error) = parent.detach_exact(&old_root) {
                new_root_parent
                    .attach_top(&new_root_mountpoint, new_root.clone())
                    .expect("pivot_root rollback must restore the detached new root");
                new_root.restore_tucked_under(new_root_tucked_under);
                return Err(error);
            }
        }

        old_root.relocate_mountpoint(Some(put_old_mountpoint.clone()));
        if let Err(error) = put_old_parent.attach_new_top(&put_old_mountpoint, old_root.clone()) {
            old_root.relocate_mountpoint(old_root_mountpoint.clone());
            if let (Some(parent), Some(mountpoint)) = (&old_parent, &old_root_mountpoint) {
                parent
                    .attach_top(mountpoint, old_root.clone())
                    .expect("pivot_root rollback must restore the current root edge");
                old_root.restore_tucked_under(old_root_tucked_under);
            }
            new_root_parent
                .attach_top(&new_root_mountpoint, new_root.clone())
                .expect("pivot_root rollback must restore the detached new root");
            new_root.restore_tucked_under(new_root_tucked_under);
            return Err(error);
        }

        if let (Some(parent), Some(mountpoint)) = (&old_parent, &old_root_mountpoint) {
            new_root.relocate_mountpoint(Some(mountpoint.clone()));
            if let Err(error) = parent.attach_top(mountpoint, new_root.clone()) {
                put_old_parent
                    .detach_exact(&old_root)
                    .expect("pivot_root rollback must detach the temporary old root edge");
                old_root.relocate_mountpoint(Some(mountpoint.clone()));
                parent
                    .attach_top(mountpoint, old_root.clone())
                    .expect("pivot_root rollback must restore the current root edge");
                old_root.restore_tucked_under(old_root_tucked_under);
                new_root.relocate_mountpoint(Some(new_root_mountpoint.clone()));
                new_root_parent
                    .attach_top(&new_root_mountpoint, new_root.clone())
                    .expect("pivot_root rollback must restore the new-root edge");
                new_root.restore_tucked_under(new_root_tucked_under);
                return Err(error);
            }
        } else {
            debug_assert!(Arc::ptr_eq(&namespace_root, &old_root));
            new_root.relocate_mountpoint(None);
            inner_guard.root_mountfs = new_root.clone();
        }

        // Linux transfers MNT_LOCKED from the old root to the new root at
        // commit. The old root now lives below put_old, outside the boundary
        // that the lock protected before pivot_root.
        if old_root.is_locked() {
            old_root.unlock_mount();
            new_root.lock_mount();
        }

        Ok(())
    }

    /// Move a complete mount subtree onto an exact shared dentry.
    ///
    /// Aligns with Linux `attach_recursive_mnt(MNT_TREE_MOVE)`: detaches `source_mfs`
    /// (along with its entire child mount subtree) from the old parent mount and attaches
    /// it to the target parent mount where `target_mountpoint` resides.
    ///
    /// Child edges remain unchanged. Paths are rendered from the resulting object
    /// topology, so the move never rewrites pathname records.
    ///
    /// On attach failure, rolls back to the original mount position, ensuring all-or-nothing.
    /// Propagation is handled by the caller after success.
    ///
    /// Path and inode type checks are performed by the syscall layer. Topology-dependent
    /// admission checks are repeated here while holding `MOUNT_LIFECYCLE_LOCK`, so a concurrent
    /// move or detach cannot invalidate the decision before the edge mutation commits.
    pub fn move_mount(
        &self,
        source_mfs: &Arc<MountFS>,
        target_mountpoint: &Arc<MountFSInode>,
    ) -> Result<(), SystemError> {
        let _topology = MOUNT_LIFECYCLE_LOCK.lock();
        let namespace = self.self_ref.upgrade().ok_or(SystemError::EINVAL)?;
        let target_parent = target_mountpoint.mount_fs();
        if !source_mfs.is_live()
            || !target_parent.accepts_topology_edges()
            || !source_mfs.is_belongs_to_mntns(&namespace)
            || !target_parent.is_belongs_to_mntns(&namespace)
            || source_mfs.is_locked()
        {
            return Err(SystemError::EINVAL);
        }

        let old_mountpoint = source_mfs.self_mountpoint().ok_or(SystemError::EINVAL)?;
        let old_tucked_under = source_mfs.is_tucked_under();
        let old_parent = old_mountpoint.mount_fs();
        if !old_parent.is_live()
            || !old_parent.is_belongs_to_mntns(&namespace)
            || old_parent.propagation().is_shared()
        {
            return Err(SystemError::EINVAL);
        }

        if target_parent.propagation().is_shared() && tree_contains_unbindable(source_mfs) {
            return Err(SystemError::EINVAL);
        }

        let mut ancestor = target_parent.clone();
        loop {
            if Arc::ptr_eq(&ancestor, source_mfs) {
                return Err(SystemError::ELOOP);
            }
            match ancestor.self_mountpoint() {
                Some(mountpoint) => ancestor = mountpoint.mount_fs(),
                None => break,
            }
        }

        // Keep the old stack allocation alive until the move either commits
        // or restores this exact edge. Successful moves drop the token and
        // remove the now-empty key; rollback reuses the original Vec.
        let _old_edge_reservation = old_parent.reserve_mount_edge(&old_mountpoint, 0)?;

        // Match Linux attach_recursive_mnt(MNT_TREE_MOVE): allocate group IDs
        // and clone every propagation target while the source still occupies
        // its old edge. Resource failure therefore cannot expose a transient
        // move and needs no topology rollback.
        let prepared_propagation = if target_parent.propagation().is_shared() {
            Some(prepare_moved_tree_propagation_locked(
                &target_parent,
                source_mfs,
                target_mountpoint,
            )?)
        } else {
            None
        };
        // Shared destinations reserve this edge as part of propagation
        // prepare. A private destination still needs the same guarantee:
        // attaching the moved root after detaching its old edge must not be
        // the first operation that tries to grow the target stack.
        let _private_target_reservation = if prepared_propagation.is_none() {
            Some(target_parent.reserve_mount_edge(target_mountpoint, 1)?)
        } else {
            None
        };

        if let Err(error) = old_parent.detach_exact_keep_slot(source_mfs) {
            if let Some(prepared) = prepared_propagation {
                abort_moved_tree_propagation_locked(prepared);
            }
            return Err(error);
        }
        source_mfs.relocate_mountpoint(Some(target_mountpoint.clone()));
        if let Err(error) = target_parent.attach_new_top(target_mountpoint, source_mfs.clone()) {
            source_mfs.relocate_mountpoint(Some(old_mountpoint));
            source_mfs.restore_tucked_under(old_tucked_under);
            old_parent
                .attach_top(
                    &source_mfs
                        .self_mountpoint()
                        .expect("move rollback restored the old mountpoint"),
                    source_mfs.clone(),
                )
                .expect("move rollback must restore the exact detached edge");
            if let Some(prepared) = prepared_propagation {
                abort_moved_tree_propagation_locked(prepared);
            }
            return Err(error);
        }

        if let Some(prepared) = prepared_propagation {
            if let Err(error) = commit_moved_tree_propagation_locked(prepared) {
                target_parent
                    .detach_exact(source_mfs)
                    .expect("failed move propagation must detach the target edge");
                source_mfs.relocate_mountpoint(Some(old_mountpoint.clone()));
                source_mfs.restore_tucked_under(old_tucked_under);
                old_parent
                    .attach_top(&old_mountpoint, source_mfs.clone())
                    .expect("failed move propagation must restore the old edge");
                return Err(error);
            }
        }

        Ok(())
    }

    /// Creates a copy of the mount namespace for process cloning.
    ///
    /// This function is called during process creation to determine whether to create
    /// a new mount namespace or share the existing one based on the clone flags.
    ///
    /// # Arguments
    /// * `clone_flags` - Flags that control namespace creation behavior
    /// * `user_ns` - The user namespace to associate with the new mount namespace
    ///
    /// # Returns
    /// * `Ok(Arc<MntNamespace>)` - The appropriate mount namespace for the new process
    /// * `Err(SystemError)` - If namespace creation fails
    ///
    /// # Behavior
    /// - If `CLONE_NEWNS` is not set, returns the current mount namespace
    /// - If `CLONE_NEWNS` is set, copies the complete ordered mount topology
    #[inline(never)]
    pub fn copy_mnt_ns(
        &self,
        clone_flags: &CloneFlags,
        user_ns: Arc<UserNamespace>,
    ) -> Result<Arc<MntNamespace>, SystemError> {
        if !clone_flags.contains(CloneFlags::CLONE_NEWNS) {
            // Return the current mount namespace if CLONE_NEWNS is not set
            return Ok(self.self_ref.upgrade().unwrap());
        }
        // Keep the global topology lock outside the namespace lock. All other
        // topology mutations use the same order, so namespace copy cannot
        // deadlock with move_mount() or namespace teardown.
        let _topology = MOUNT_LIFECYCLE_LOCK.lock();
        let inner = self.inner.read();
        let cross_user_namespace = !Arc::ptr_eq(&self._user_ns, &user_ns);

        if inner.mount_count.pending_mounts != 0 {
            return Err(SystemError::EBUSY);
        }

        let old_root_mntfs = Self::root_mntfs_locked(&inner);
        let new_root_mntfs = old_root_mntfs.deepcopy(None)?;
        restrict_cross_user_propagation(&old_root_mntfs, &new_root_mntfs, cross_user_namespace);
        let mut copied_mounts = vec![(old_root_mntfs.clone(), new_root_mntfs.clone())];
        let mut queue = VecDeque::from([(old_root_mntfs, new_root_mntfs.clone())]);

        // Build the complete detached tree first. Every copied edge retains the
        // exact shared dentry, and each shadow stack is replayed bottom-to-top.
        // Consequently any projection failure leaves only constructing mounts,
        // with no namespace or lifecycle state to roll back.
        let copy_result = (|| {
            while let Some((old_parent, new_parent)) = queue.pop_front() {
                let child_stacks: Vec<Vec<Arc<MountFS>>> =
                    old_parent.mountpoints().values().cloned().collect();
                for child_stack in child_stacks {
                    for old_child in child_stack {
                        let old_mountpoint =
                            old_child.self_mountpoint().ok_or(SystemError::EINVAL)?;
                        let new_mountpoint =
                            new_parent.wrapper_for_existing_edge(old_mountpoint.shared_dentry());
                        let new_child = old_child.deepcopy(Some(new_mountpoint.clone()))?;
                        restrict_cross_user_propagation(
                            &old_child,
                            &new_child,
                            cross_user_namespace,
                        );
                        if let Err(error) =
                            new_parent.attach_top(&new_mountpoint, new_child.clone())
                        {
                            MountFS::deactivate_disconnected_subtree(&new_child);
                            return Err(error);
                        }
                        copied_mounts.push((old_child.clone(), new_child.clone()));
                        queue.push_back((old_child, new_child));
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = copy_result {
            MountFS::deactivate_disconnected_subtree(&new_root_mntfs);
            return Err(error);
        }

        let prepared_metadata = (|| {
            let copied_count =
                u32::try_from(copied_mounts.len()).map_err(|_| SystemError::ENOSPC)?;
            assert_eq!(
                copied_count, inner.mount_count.mounts,
                "namespace copy topology must match the source committed count"
            );

            let mut copy_sources = Vec::new();
            copy_sources
                .try_reserve(copied_mounts.len())
                .map_err(|_| SystemError::ENOMEM)?;
            copy_sources.extend(
                copied_mounts
                    .iter()
                    .map(|(old, new)| (Arc::downgrade(old), Arc::downgrade(new))),
            );
            Ok::<_, SystemError>((copied_count, copy_sources))
        })();
        let (copied_count, copy_sources) = match prepared_metadata {
            Ok(metadata) => metadata,
            Err(error) => {
                MountFS::deactivate_disconnected_subtree(&new_root_mntfs);
                return Err(error);
            }
        };

        #[cfg(test)]
        if FAIL_COPY_REGISTRATION_PREPARE.swap(false, Ordering::AcqRel) {
            MountFS::deactivate_disconnected_subtree(&new_root_mntfs);
            return Err(SystemError::ENOMEM);
        }
        let prepared_registrations =
            match PreparedRegistrations::prepare_iter(copied_mounts.iter().map(|(_, copy)| copy)) {
                Ok(registrations) => registrations,
                Err(error) => {
                    MountFS::deactivate_disconnected_subtree(&new_root_mntfs);
                    return Err(error);
                }
            };

        let mut ns_common = self.ns_common.clone();
        ns_common.level += 1;
        let new_mntns = Arc::new_cyclic(|self_ref| Self {
            ns_common,
            self_ref: self_ref.clone(),
            _user_ns: user_ns,
            inner: RwSem::new(InnerMntNamespace {
                _dead: false,
                root_mountfs: new_root_mntfs,
                // Linux copy_mnt_ns() initializes the copied namespace's
                // existing mount count directly. mount-max only admits new
                // mount trees; it must not reject cloning existing topology.
                mount_count: MountCountState {
                    mounts: copied_count,
                    pending_mounts: 0,
                },
                copy_sources,
            }),
        });

        // Publication is infallible and occurs only after the detached copy is
        // complete, so observers can never see a partially copied namespace.
        for (_old_mount, new_mount) in copied_mounts {
            new_mount.set_namespace(Arc::downgrade(&new_mntns));
            new_mount.mark_namespace_accounted(&new_mntns);
            new_mount
                .activate()
                .expect("a detached namespace copy is published exactly once");
        }
        prepared_registrations.commit();

        Ok(new_mntns)
    }

    fn root_mntfs_locked(inner: &InnerMntNamespace) -> Arc<MountFS> {
        inner.root_mountfs.clone()
    }

    pub fn root_mntfs(&self) -> Arc<MountFS> {
        Self::root_mntfs_locked(&self.inner.read())
    }

    /// Get the root inode of this mount namespace
    pub fn root_inode(&self) -> Arc<dyn IndexNode> {
        let root = self.root_mntfs();
        root.root_inode()
    }

    /// Project a path inode from the namespace that was copied to create this
    /// namespace. The mount context changes, while the shared dentry identity
    /// is retained exactly (including hard-link aliases and renamed parents).
    pub fn project_copy_source_inode(
        &self,
        inode: &Arc<dyn IndexNode>,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let old_inode = inode
            .clone()
            .downcast_arc::<MountFSInode>()
            .ok_or(SystemError::EXDEV)?;
        let old_mount = old_inode.mount_fs();
        let new_mount = self
            .inner
            .read()
            .copy_sources
            .iter()
            .find_map(|(source, copy)| {
                source
                    .upgrade()
                    .filter(|source| Arc::ptr_eq(source, &old_mount))
                    .and_then(|_| copy.upgrade())
            })
            .ok_or(SystemError::EXDEV)?;
        Ok(new_mount.wrapper_for_existing_edge(old_inode.shared_dentry()) as Arc<dyn IndexNode>)
    }

    pub fn add_mount(
        &self,
        parent: Option<&Arc<MountFS>>,
        mountpoint: Option<&Arc<MountFSInode>>,
        mntfs: Arc<MountFS>,
    ) -> Result<(), SystemError> {
        // Publication is only valid for a freshly constructed mount. Moving a
        // live mount uses `move_mount`; accepting one here could silently move
        // namespace ownership while leaving its old topology edge connected.
        if mntfs.is_live() {
            return Err(SystemError::EBUSY);
        }
        let count_reservation = self.reserve_mounts(vec![mntfs.clone()])?;
        // Initialize namespace ownership before publishing the edge. Path
        // lookup reads mount edges without taking the global topology lock, so
        // it must never observe an attached mount with missing ownership.
        mntfs.set_namespace(self.self_ref.clone());
        let attached_parent = match (parent, mountpoint) {
            (None, None) => {
                if !Arc::ptr_eq(&self.root_mntfs(), &mntfs) || mntfs.self_mountpoint().is_some() {
                    mntfs.clear_namespace();
                    return Err(SystemError::EINVAL);
                }
                None
            }
            (Some(parent), Some(mountpoint)) => {
                if !Arc::ptr_eq(&mountpoint.mount_fs(), parent)
                    || mntfs
                        .self_mountpoint()
                        .as_ref()
                        .is_none_or(|child_mountpoint| !Arc::ptr_eq(child_mountpoint, mountpoint))
                {
                    mntfs.clear_namespace();
                    return Err(SystemError::EINVAL);
                }
                if let Err(error) = parent.attach_new_top(mountpoint, mntfs.clone()) {
                    mntfs.clear_namespace();
                    return Err(error);
                }
                Some(parent)
            }
            _ => {
                mntfs.clear_namespace();
                return Err(SystemError::EINVAL);
            }
        };

        if let Err(error) = mntfs.activate() {
            if let Some(parent) = attached_parent {
                parent
                    .detach_exact(&mntfs)
                    .expect("failed mount publication must roll back its exact edge");
            }
            mntfs.clear_namespace();
            return Err(error);
        }
        count_reservation.commit();
        Ok(())
    }

    /// Publish a completely prepared mount tree. Every descendant is made
    /// live and namespace-owned before the root edge becomes reachable, so a
    /// concurrent lookup can only observe either the old topology or the
    /// complete new tree.
    pub fn add_mount_tree(
        &self,
        parent: &Arc<MountFS>,
        mountpoint: &Arc<MountFSInode>,
        root: Arc<MountFS>,
    ) -> Result<(), SystemError> {
        if root.is_live()
            || !Arc::ptr_eq(&mountpoint.mount_fs(), parent)
            || root
                .self_mountpoint()
                .as_ref()
                .is_none_or(|root_mp| !Arc::ptr_eq(root_mp, mountpoint))
        {
            return Err(SystemError::EINVAL);
        }

        let namespace = self.self_ref.clone();
        let mut mounts: Vec<Arc<MountFS>> = Vec::new();
        let mut pending = vec![root.clone()];
        while let Some(mount) = pending.pop() {
            if mount.is_live() {
                for published in mounts.into_iter().rev() {
                    published.clear_namespace();
                    published.deactivate();
                }
                return Err(SystemError::EBUSY);
            }
            pending.extend(mount.mount_children());
            mount.set_namespace(namespace.clone());
            if let Err(error) = mount.activate() {
                mount.clear_namespace();
                for published in mounts.into_iter().rev() {
                    published.clear_namespace();
                    published.deactivate();
                }
                return Err(error);
            }
            mounts.push(mount);
        }

        if let Err(error) = parent.attach_new_top(mountpoint, root) {
            for mount in mounts.into_iter().rev() {
                mount.clear_namespace();
                mount.deactivate();
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn remove_mount_exact(&self, mntfs: &Arc<MountFS>) -> Option<Arc<MountFS>> {
        if !mntfs.is_belongs_to_mntns(&self.self_ref.upgrade()?) {
            return None;
        }
        if mntfs.take_namespace_accounted(&self.self_ref) {
            self.inner.write().mount_count.release(1);
        }
        Some(mntfs.clone())
    }

    pub(crate) fn reserve_mounts(
        &self,
        mut mounts: Vec<Arc<MountFS>>,
    ) -> Result<MountCountReservation, SystemError> {
        if mounts.is_empty() {
            return Err(SystemError::EINVAL);
        }
        mounts.sort_unstable_by_key(|mount| mount.mount_id().data());
        if mounts
            .windows(2)
            .any(|pair| pair[0].mount_id() == pair[1].mount_id())
        {
            return Err(SystemError::EINVAL);
        }
        let amount = u32::try_from(mounts.len()).map_err(|_| SystemError::ENOSPC)?;
        let namespace = self.self_ref.upgrade().ok_or(SystemError::EINVAL)?;
        self.inner
            .write()
            .mount_count
            .reserve(amount, mount_max())?;
        Ok(MountCountReservation {
            namespace,
            mounts,
            pending: true,
        })
    }
}

fn restrict_cross_user_propagation(
    source: &Arc<MountFS>,
    copy: &Arc<MountFS>,
    cross_user_namespace: bool,
) {
    if !cross_user_namespace {
        return;
    }
    copy.lock_cross_user_mount();
    if !source.propagation().is_shared() {
        return;
    }
    let propagation = copy.propagation();
    propagation.set_private();
    propagation.set_slave(Some(Arc::downgrade(source)));
}

impl ProcessManager {
    /// Get the mount namespace of the current process
    pub fn current_mntns() -> Arc<MntNamespace> {
        if Self::initialized() {
            ProcessManager::current_pcb().nsproxy().mnt_ns.clone()
        } else {
            root_mnt_namespace()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::namespace::{
        propagation::{get_peers, register_peer},
        user_namespace::INIT_USER_NAMESPACE,
    };

    #[test]
    fn registration_prepare_failure_cleans_namespace_copy() {
        let namespace = MntNamespace::new_root();
        let root = namespace.root_mntfs();
        root.propagation().set_shared().unwrap();
        let group_id = root.propagation().peer_group_id();
        register_peer(group_id, &root);
        let peers_before = get_peers(group_id, &root).len();
        let slaves_before = root.propagation().slaves().len();
        let pins_before = root.superblock_external_pin_count();
        let mount_count_before = namespace.inner.read().mount_count.mounts;

        FAIL_COPY_REGISTRATION_PREPARE.store(true, Ordering::Release);
        let result = namespace.copy_mnt_ns(&CloneFlags::CLONE_NEWNS, INIT_USER_NAMESPACE.clone());

        assert!(matches!(result, Err(SystemError::ENOMEM)));
        assert_eq!(root.superblock_external_pin_count(), pins_before);
        assert_eq!(get_peers(group_id, &root).len(), peers_before);
        assert_eq!(root.propagation().slaves().len(), slaves_before);
        assert_eq!(
            namespace.inner.read().mount_count.mounts,
            mount_count_before
        );
        assert!(root.is_live());
        assert!(root.is_belongs_to_mntns(&namespace));
    }
}

impl Drop for MntNamespace {
    fn drop(&mut self) {
        // Namespace destruction is a topology teardown, not filesystem I/O.
        // Deactivation only updates explicit counters and schedules the final
        // superblock worker when the last mount/path reference is gone.
        let _topology = MOUNT_LIFECYCLE_LOCK.lock();
        let root = self.inner.read().root_mountfs.clone();
        let mut pending = vec![root.clone()];
        let mut released = 0u32;
        while let Some(mount) = pending.pop() {
            pending.extend(mount.mount_children());
            if mount.take_namespace_accounted(&self.self_ref) {
                released = released
                    .checked_add(1)
                    .expect("namespace teardown mount count overflow");
            }
        }
        {
            let mut inner = self.inner.write();
            assert_eq!(
                inner.mount_count.pending_mounts, 0,
                "a mount namespace cannot drop with pending reservations"
            );
            assert_eq!(
                inner.mount_count.mounts, released,
                "namespace teardown must consume every committed mount exactly once"
            );
            inner.mount_count.mounts = 0;
        }
        MountFS::deactivate_disconnected_subtree(&root);
    }
}

#[cfg(test)]
mod mount_count_tests {
    use super::MountCountState;
    use system_error::SystemError;

    #[test]
    fn exact_limit_succeeds_and_next_reservation_fails() {
        let mut state = MountCountState {
            mounts: 3,
            pending_mounts: 2,
        };
        state.reserve(5, 10).unwrap();
        assert_eq!(state.pending_mounts, 7);
        assert_eq!(state.reserve(1, 10), Err(SystemError::ENOSPC));
        assert_eq!(state.pending_mounts, 7);
    }

    #[test]
    fn rollback_and_commit_transfer_only_their_own_pending_count() {
        let mut state = MountCountState {
            mounts: 4,
            pending_mounts: 0,
        };
        state.reserve(3, 10).unwrap();
        state.reserve(2, 10).unwrap();
        state.abort(3);
        assert_eq!(state.mounts, 4);
        assert_eq!(state.pending_mounts, 2);
        state.commit(2);
        assert_eq!(state.mounts, 6);
        assert_eq!(state.pending_mounts, 0);
        state.release(2);
        assert_eq!(state.mounts, 4);
    }

    #[test]
    fn arithmetic_overflow_and_lowered_limit_leave_state_unchanged() {
        let mut overflow = MountCountState {
            mounts: u32::MAX,
            pending_mounts: 1,
        };
        assert_eq!(overflow.reserve(1, u32::MAX), Err(SystemError::ENOSPC));
        assert_eq!(overflow.mounts, u32::MAX);
        assert_eq!(overflow.pending_mounts, 1);

        let mut lowered = MountCountState {
            mounts: 20,
            pending_mounts: 0,
        };
        assert_eq!(lowered.reserve(1, 10), Err(SystemError::ENOSPC));
        assert_eq!(lowered.mounts, 20);
        assert_eq!(lowered.pending_mounts, 0);
    }
}
