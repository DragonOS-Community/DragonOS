//! Allocation-complete mount attribute and propagation transactions.
//!
//! Selection, preparation and publication share the lifecycle lock. Writer
//! holds only exclude new writers: this module never waits for existing ones.
//! Flags/idmap getters do not take the lifecycle lock, so publication does not
//! promise a cross-field or cross-mount atomic snapshot to ordinary I/O.

use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{
    libs::mutex::MutexGuard,
    process::{
        cred::{ns_capable, CAPFlags},
        namespace::propagation::{
            collect_change_targets, PreparedPropagationChange, PropagationType,
        },
        ProcessManager,
    },
};

use super::{
    idmap::MountIdmap, writer::MountWriterHoldGuard, MountFS, MountFlags, MOUNT_LIFECYCLE_LOCK,
};

/// Validated per-mount masks (not superblock flags). ABI validation and atime
/// enum conversion belong to mount_api; overlap is intentionally clear-then-set.
pub(crate) struct MountAttributeChange {
    pub set: MountFlags,
    pub clear: MountFlags,
    pub propagation: Option<PropagationType>,
    pub idmap: Option<Arc<MountIdmap>>,
}

struct PreparedMountUpdate {
    mount: Arc<MountFS>,
    flags: MountFlags,
    idmap: Option<Arc<MountIdmap>>,
    hold: Option<MountWriterHoldGuard>,
}

/// Dropping an uncommitted transaction cancels holds and prepared resources
/// before releasing topology serialization. It never changes old attributes.
struct PreparedMountAttributes {
    updates: Vec<PreparedMountUpdate>,
    propagation: Option<PreparedPropagationChange>,
    _topology_guard: MutexGuard<'static, ()>,
}

fn check_idmap_eligibility(
    mount: &Arc<MountFS>,
    idmap: Option<&Arc<MountIdmap>>,
) -> Result<(), SystemError> {
    let Some(idmap) = idmap else {
        return Ok(());
    };
    let superblock = mount.super_block_state();
    let owner = superblock.owner_user_ns();
    // Preserve Linux can_idmap_mount's error precedence within each target.
    if Arc::ptr_eq(idmap.owner(), owner) {
        return Err(SystemError::EINVAL);
    }
    if mount.idmap().is_some() {
        return Err(SystemError::EPERM);
    }
    if !mount.inner_filesystem().supports_idmapped_mounts() {
        return Err(SystemError::EINVAL);
    }
    if !ns_capable(owner, CAPFlags::CAP_SYS_ADMIN) {
        return Err(SystemError::EPERM);
    }
    if !mount.namespace().is_some_and(|ns| ns.is_anonymous()) {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

impl PreparedMountAttributes {
    fn prepare(
        root: &Arc<MountFS>,
        change: &MountAttributeChange,
        recursive: bool,
    ) -> Result<Self, SystemError> {
        let topology_guard = MOUNT_LIFECYCLE_LOCK.lock();
        let namespace = root.namespace().ok_or(SystemError::EINVAL)?;
        if !root.is_live()
            || ((root.parent_mount().is_some() || !namespace.is_anonymous())
                && !root.is_belongs_to_mntns(&ProcessManager::current_mntns()))
        {
            return Err(SystemError::EINVAL);
        }

        // Include every shadow-stack mount. Do not recollect for propagation,
        // or sort MountIDs and claim Linux's insertion-order traversal.
        let targets = collect_change_targets(root, recursive, &mut || Ok(()))?;
        let mut updates = Vec::new();
        updates
            .try_reserve(targets.len())
            .map_err(|_| SystemError::ENOMEM)?;
        let propagation = change
            .propagation
            .map(|kind| PreparedPropagationChange::prepare_selected_locked(&targets, kind))
            .transpose()?;

        for mount in targets {
            let old_flags = mount.mount_flags();
            let flags = (old_flags & !change.clear) | change.set;
            // This must remain one per-node sequence, rather than three
            // whole-tree passes that change the first reported error.
            if !mount.can_reconfigure_mount_flags(flags) {
                return Err(SystemError::EPERM);
            }
            check_idmap_eligibility(&mount, change.idmap.as_ref())?;
            let hold = if change.idmap.is_some()
                || (change.set.contains(MountFlags::RDONLY)
                    && !old_flags.contains(MountFlags::RDONLY))
            {
                Some(mount.try_hold_writers()?)
            } else {
                None
            };
            updates.push(PreparedMountUpdate {
                mount,
                flags,
                idmap: change.idmap.clone(),
                hold,
            });
        }
        Ok(Self {
            updates,
            propagation,
            _topology_guard: topology_guard,
        })
    }

    fn commit(self) {
        let Self {
            updates,
            propagation,
            _topology_guard,
        } = self;
        for update in updates {
            if let Some(hold) = update.hold {
                hold.commit_attributes(update.flags, update.idmap);
            } else {
                // IDMAP always requires a hold. Flags-only changes still
                // publish through the same short admission gate.
                assert!(update.idmap.is_none());
                update.mount.set_mount_flags(update.flags);
            }
        }
        if let Some(propagation) = propagation {
            propagation.commit_locked();
        }
        drop(_topology_guard);
    }
}

/// The resolved caller must retain its existing semantic path/mount pin. The
/// lifecycle lock then prevents removal or reparenting throughout the batch.
pub(crate) fn apply_mount_attributes(
    root: &Arc<MountFS>,
    change: &MountAttributeChange,
    recursive: bool,
) -> Result<(), SystemError> {
    PreparedMountAttributes::prepare(root, change, recursive)?.commit();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        filesystem::ramfs::RamFS,
        process::namespace::{
            mnt::MntNamespace, propagation::MountPropagation, user_namespace::INIT_USER_NAMESPACE,
        },
    };

    fn mount() -> Arc<MountFS> {
        let mount = MountFS::new(
            RamFS::new(),
            None,
            None,
            MountPropagation::new_private(),
            None,
            MountFlags::empty(),
            None,
        )
        .unwrap();
        mount.activate().unwrap();
        mount
    }

    fn anonymous_tree(root: &Arc<MountFS>, members: Vec<Arc<MountFS>>) -> Arc<MntNamespace> {
        let _topology = MOUNT_LIFECYCLE_LOCK.lock();
        MntNamespace::new_anonymous(root.clone(), members, INIT_USER_NAMESPACE.clone()).unwrap()
    }

    #[test]
    fn noexec_allows_an_existing_writer_but_readonly_does_not() {
        let root = mount();
        let _namespace = anonymous_tree(&root, alloc::vec![root.clone()]);
        let writer = root.want_write().unwrap();
        let mut change = MountAttributeChange {
            set: MountFlags::NOEXEC,
            clear: MountFlags::empty(),
            propagation: None,
            idmap: None,
        };
        apply_mount_attributes(&root, &change, false).unwrap();
        assert!(root.mount_flags().contains(MountFlags::NOEXEC));
        change.set = MountFlags::RDONLY;
        assert_eq!(
            apply_mount_attributes(&root, &change, false),
            Err(SystemError::EBUSY)
        );
        assert!(!root.mount_flags().contains(MountFlags::RDONLY));
        drop(writer);
        apply_mount_attributes(&root, &change, false).unwrap();
        assert!(root.mount_flags().contains(MountFlags::RDONLY));
    }

    #[test]
    fn recursive_locked_child_aborts_attributes_and_prepared_propagation() {
        let root = mount();
        let child = mount();
        {
            let _topology = MOUNT_LIFECYCLE_LOCK.lock();
            let mountpoint = root.mountpoint_root_inode();
            child.set_self_mountpoint(Some(mountpoint.clone()));
            root.attach_top(&mountpoint, child.clone()).unwrap();
        }
        let _namespace = anonymous_tree(&root, alloc::vec![root.clone(), child.clone()]);
        child.set_mount_flags(MountFlags::NOEXEC);
        child.lock_cross_user_mount();
        let writer = child.want_write().unwrap();
        let change = MountAttributeChange {
            set: MountFlags::RDONLY,
            clear: MountFlags::NOEXEC,
            propagation: Some(PropagationType::Shared),
            idmap: None,
        };
        assert_eq!(
            apply_mount_attributes(&root, &change, true),
            Err(SystemError::EPERM)
        );
        assert!(root.mount_flags().is_empty());
        assert_eq!(child.mount_flags(), MountFlags::NOEXEC);
        assert!(root.propagation().is_private() && child.propagation().is_private());
        // The root was held before inspecting the child. Abort must unhold it.
        drop(root.try_hold_writers().unwrap());
        drop(writer);
    }
}
