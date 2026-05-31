use crate::{
    filesystem::vfs::{
        mount::{MountFSInode, MountFlags, MountList, MountPath},
        FileSystem, IndexNode, InodeId, MountFS,
    },
    libs::{once::Once, spinlock::SpinLock},
    process::{fork::CloneFlags, namespace::NamespaceType, ProcessManager},
};
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use system_error::SystemError;

use super::{
    nsproxy::NsCommon,
    propagation::{register_peer, register_slave_with_master, MountPropagation},
    user_namespace::UserNamespace,
    NamespaceOps,
};

static mut INIT_MNT_NAMESPACE: Option<Arc<MntNamespace>> = None;

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
    root_mountfs: Arc<MountFS>,
    inner: SpinLock<InnerMntNamespace>,
}

pub struct InnerMntNamespace {
    _dead: bool,
    mount_list: Arc<MountList>,
}

impl NamespaceOps for MntNamespace {
    fn ns_common(&self) -> &NsCommon {
        &self.ns_common
    }
}

impl MntNamespace {
    fn new_root() -> Arc<Self> {
        let mount_list = MountList::new();

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
            root_mountfs: ramfs.clone(),
            inner: SpinLock::new(InnerMntNamespace {
                mount_list,
                _dead: false,
            }),
        });

        ramfs.set_namespace(Arc::downgrade(&result));
        result
            .add_mount(None, Arc::new(MountPath::from("/")), ramfs)
            .expect("Failed to add root mount");

        return result;
    }

    pub fn user_ns(&self) -> &Arc<UserNamespace> {
        &self._user_ns
    }

    /// Forcibly replace the root mount filesystem of this MountNamespace.
    ///
    /// This method is only for use during DragonOS initialization.
    pub unsafe fn force_change_root_mountfs(&self, new_root: Arc<MountFS>) {
        let inner_guard = self.inner.lock();
        let ptr = self as *const Self as *mut Self;
        let self_mut = (ptr).as_mut().unwrap();
        self_mut.root_mountfs = new_root.clone();
        let (path, _, _) = inner_guard.mount_list.get_mount_point("/").unwrap();

        inner_guard.mount_list.insert(None, path, new_root);

        // update mount list ino
    }

    pub fn pivot_root(
        &self,
        new_root: Arc<MountFS>,
        put_old_mountpoint: Arc<MountFSInode>,
        old_new_root_path: &str,
        old_put_old_path: &str,
        new_put_old_path: &str,
    ) -> Result<(), SystemError> {
        let old_root = self.root_mountfs.clone();
        let old_root_mountpoint = old_root.self_mountpoint();
        let new_root_mountpoint = new_root.self_mountpoint().ok_or(SystemError::EINVAL)?;
        let new_root_parent = new_root_mountpoint.mount_fs();
        let put_old_parent = put_old_mountpoint.mount_fs();
        let put_old_is_new_root = old_put_old_path == old_new_root_path;
        let new_root_mountpoint_id = new_root_mountpoint.inode_id()?;
        let put_old_mountpoint_id = put_old_mountpoint.inode_id()?;

        {
            let put_old_mounts = put_old_parent.mountpoints();
            if put_old_mounts.contains_key(&put_old_mountpoint_id) {
                return Err(SystemError::EBUSY);
            }
        }

        {
            let mut parent_mounts = new_root_parent.mountpoints();
            parent_mounts
                .remove(&new_root_mountpoint_id)
                .ok_or(SystemError::EINVAL)?;
        }

        old_root.set_self_mountpoint(Some(put_old_mountpoint.clone()));
        {
            let mut put_old_mounts = put_old_parent.mountpoints();
            if put_old_mounts
                .insert(put_old_mountpoint_id, old_root.clone())
                .is_some()
            {
                old_root.set_self_mountpoint(old_root_mountpoint);
                new_root_parent
                    .add_mount(new_root_mountpoint_id, new_root.clone())
                    .map_err(|_| SystemError::EBUSY)?;
                return Err(SystemError::EBUSY);
            }
        }

        new_root.set_self_mountpoint(None);

        let inner_guard = self.inner.lock();
        let ptr = self as *const Self as *mut Self;
        let self_mut = unsafe { (ptr).as_mut().unwrap() };
        self_mut.root_mountfs = new_root.clone();

        inner_guard.mount_list.remove("/");
        if put_old_is_new_root {
            inner_guard.mount_list.rewrite_paths(|path| {
                if path == old_new_root_path || path_is_under(path, old_new_root_path) {
                    return Some(rewrite_pivot_path(
                        path,
                        old_new_root_path,
                        new_put_old_path,
                    ));
                }

                None
            });
            inner_guard.mount_list.insert(
                Some(put_old_mountpoint_id),
                Arc::new(MountPath::from("/")),
                old_root,
            );
        } else {
            inner_guard.mount_list.rewrite_paths(|path| {
                if path == old_put_old_path || path_is_under(path, old_put_old_path) {
                    return None;
                }

                Some(rewrite_pivot_path(
                    path,
                    old_new_root_path,
                    new_put_old_path,
                ))
            });
            inner_guard.mount_list.insert(
                Some(put_old_mountpoint_id),
                Arc::new(MountPath::from(new_put_old_path)),
                old_root,
            );
        }

        Ok(())
    }

    /// Implement the topology move and mount_list subtree path rewrite for mount(MS_MOVE).
    ///
    /// Aligns with Linux `attach_recursive_mnt(MNT_TREE_MOVE)`: detaches `source_mfs`
    /// (along with its entire child mount subtree) from the old parent mount, attaches it
    /// to the target parent mount where `target_mountpoint` resides, and rewrites all
    /// mount_list paths prefixed with `old_source_path` to `new_target_path`.
    ///
    /// Child mounts' parent-child relationships (`mountpoints`) and their respective
    /// `self_mountpoint` remain unchanged, so only the moved mount's own `self_mountpoint`
    /// and the path records of the entire subtree in mount_list need updating.
    ///
    /// On attach failure, rolls back to the original mount position, ensuring all-or-nothing.
    /// Propagation is handled by the caller after success.
    ///
    /// Pre-checks (belongs to current mntns, source is mount root, type match, cycle prevention,
    /// parent mount not shared, etc.) are performed by the caller (syscall layer); this method
    /// only handles the topology changes.
    pub fn move_mount(
        &self,
        source_mfs: &Arc<MountFS>,
        target_mountpoint: &Arc<MountFSInode>,
        old_source_path: &str,
        new_target_path: &str,
    ) -> Result<(), SystemError> {
        let old_mountpoint = source_mfs.self_mountpoint().ok_or(SystemError::EINVAL)?;
        let old_parent = old_mountpoint.mount_fs();
        let old_mp_id = old_mountpoint.inode_id()?;

        let target_parent = target_mountpoint.mount_fs();
        let target_mp_id = target_mountpoint.inode_id()?;

        // 1. Detach from the old parent mount.
        let removed = old_parent
            .mountpoints()
            .remove(&old_mp_id)
            .ok_or(SystemError::ENOENT)?;

        // 2. Attach to the target parent mount; on failure, roll back by reattaching source to its original position.
        if let Err(e) = target_parent.add_mount(target_mp_id, source_mfs.clone()) {
            old_parent.mountpoints().insert(old_mp_id, removed);
            return Err(e);
        }
        source_mfs.set_self_mountpoint(Some(target_mountpoint.clone()));

        // 3. mount_list subtree path rewrite + root mount point inode update (rebuilt atomically
        //    within the ns lock, keeping mountpoints and mount_list's four tables consistent).
        //
        //    Critical: the moved subtree root's mount point inode has changed from old_mp_id to
        //    target_mp_id. The ino in the mount_list root record must be updated accordingly,
        //    otherwise copy_mnt_ns() will fail when traversing mountpoints and looking up
        //    target_mp_id in ino2mp.
        let inner = self.inner.lock();
        inner
            .mount_list
            .move_subtree(source_mfs, target_mp_id, old_source_path, new_target_path);

        Ok(())
    }

    fn copy_with_mountfs(&self, new_root: Arc<MountFS>, _user_ns: Arc<UserNamespace>) -> Arc<Self> {
        let mut ns_common = self.ns_common.clone();
        ns_common.level += 1;

        let result = Arc::new_cyclic(|self_ref| Self {
            ns_common,
            self_ref: self_ref.clone(),
            _user_ns,
            root_mountfs: new_root.clone(),
            inner: SpinLock::new(InnerMntNamespace {
                _dead: false,
                mount_list: MountList::new(),
            }),
        });

        new_root.set_namespace(Arc::downgrade(&result));
        result
            .add_mount(None, Arc::new(MountPath::from("/")), new_root)
            .expect("Failed to add root mount");

        result
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
    /// - If `CLONE_NEWNS` is set, creates a new mount namespace (currently unimplemented)
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
        let inner = self.inner.lock();

        let old_root_mntfs = self.root_mntfs().clone();
        let mut queue: Vec<MountFSCopyInfo> = Vec::new();

        // The root mntfs is special, so it is copied separately.
        let new_root_mntfs = old_root_mntfs.deepcopy(None);

        // If root mount was shared, register the new root in the same peer group
        let old_root_propagation = old_root_mntfs.propagation();
        if old_root_propagation.is_shared() {
            let group_id = old_root_propagation.peer_group_id();
            register_peer(group_id, &new_root_mntfs);
            log::debug!(
                "copy_mnt_ns: registered new root mount in peer group {}",
                group_id.data()
            );
        }
        if old_root_propagation.is_slave() {
            register_slave_with_master(&new_root_mntfs);
        }

        let new_mntns = self.copy_with_mountfs(new_root_mntfs, user_ns);

        for x in inner.mount_list.clone_inner().values() {
            if Arc::ptr_eq(x, new_mntns.root_mntfs()) {
                continue; // Skip the root mountfs
            }
        }

        // Copy all mount points under root mntfs into the new mntns
        for (ino, mfs) in old_root_mntfs.mountpoints().iter() {
            let mount_path = inner
                .mount_list
                .get_mount_path_by_ino(*ino)
                .unwrap_or_else(|| {
                    panic!(
                        "copy_mnt_ns: mount_path not found for ino={:?}, mfs name={}. \
                         mountpoints and mount_list are out of sync.",
                        ino,
                        mfs.name()
                    )
                });

            queue.push(MountFSCopyInfo {
                old_mount_fs: mfs.clone(),
                parent_mount_fs: new_mntns.root_mntfs().clone(),
                self_mp_inode_id: *ino,
                mount_path,
            });
        }

        // Process mount points in the queue
        while let Some(data) = queue.pop() {
            let old_self_mp = data.old_mount_fs.self_mountpoint().unwrap();
            let new_self_mp = old_self_mp.clone_with_new_mount_fs(data.parent_mount_fs.clone());
            let new_mount_fs = data.old_mount_fs.deepcopy(Some(new_self_mp));

            // copy_mnt_ns second pass
            new_mount_fs.set_namespace(Arc::downgrade(&new_mntns));

            // If the old mount was shared, register the new mount in the same peer group
            // This establishes the peer relationship for cross-namespace propagation
            let old_propagation = data.old_mount_fs.propagation();
            if old_propagation.is_shared() {
                let group_id = old_propagation.peer_group_id();
                register_peer(group_id, &new_mount_fs);
            }
            if old_propagation.is_slave() {
                register_slave_with_master(&new_mount_fs);
            }

            data.parent_mount_fs
                .add_mount(data.self_mp_inode_id, new_mount_fs.clone())
                .expect("Failed to add mount");
            new_mntns
                .add_mount(
                    Some(data.self_mp_inode_id),
                    data.mount_path.clone(),
                    new_mount_fs.clone(),
                )
                .expect("Failed to add mount to mount namespace");

            // Add child mounts of the original mount point to the queue

            for (child_ino, child_mfs) in data.old_mount_fs.mountpoints().iter() {
                let child_mount_path = inner
                    .mount_list
                    .get_mount_path_by_ino(*child_ino)
                    .unwrap_or_else(|| {
                        panic!(
                            "copy_mnt_ns: child mount_path not found for ino={:?}, mfs name={}, \
                             parent path={}. mountpoints and mount_list are out of sync.",
                            child_ino,
                            child_mfs.name(),
                            data.mount_path.as_str()
                        )
                    });
                queue.push(MountFSCopyInfo {
                    old_mount_fs: child_mfs.clone(),
                    parent_mount_fs: new_mount_fs.clone(),
                    self_mp_inode_id: *child_ino,
                    mount_path: child_mount_path,
                });
            }
        }

        // todo: register in procfs

        // Return the newly created mount namespace
        Ok(new_mntns)
    }

    pub fn root_mntfs(&self) -> &Arc<MountFS> {
        &self.root_mountfs
    }

    /// Get the root inode of this mount namespace
    pub fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.root_mountfs.root_inode()
    }

    pub fn add_mount(
        &self,
        ino: Option<InodeId>,
        mount_path: Arc<MountPath>,
        mntfs: Arc<MountFS>,
    ) -> Result<(), SystemError> {
        self.inner.lock().mount_list.insert(ino, mount_path, mntfs);
        Ok(())
    }

    pub fn mount_list(&self) -> Arc<MountList> {
        self.inner.lock().mount_list.clone()
    }

    pub fn remove_mount(&self, mount_path: &str) -> Option<Arc<MountFS>> {
        self.inner.lock().mount_list.remove(mount_path)
    }

    pub fn get_mount_point(
        &self,
        mount_point: &str,
    ) -> Option<(Arc<MountPath>, String, Arc<MountFS>)> {
        self.inner.lock().mount_list.get_mount_point(mount_point)
    }
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

struct MountFSCopyInfo {
    old_mount_fs: Arc<MountFS>,
    parent_mount_fs: Arc<MountFS>,
    self_mp_inode_id: InodeId,
    mount_path: Arc<MountPath>,
}

fn rewrite_pivot_path(path: &str, old_new_root_path: &str, new_put_old_path: &str) -> String {
    if path == old_new_root_path {
        return "/".to_string();
    }

    if let Some(suffix) = path.strip_prefix(old_new_root_path) {
        if suffix.is_empty() {
            return "/".to_string();
        }

        return normalize_pivot_path(suffix);
    }

    if path == "/" {
        return new_put_old_path.to_string();
    }

    join_pivot_paths(new_put_old_path, path)
}

fn path_is_under(path: &str, prefix: &str) -> bool {
    path != prefix
        && path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn normalize_pivot_path(path: &str) -> String {
    if path.is_empty() || path == "/" {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{}", path)
    }
}

fn join_pivot_paths(prefix: &str, suffix: &str) -> String {
    if prefix == "/" {
        return normalize_pivot_path(suffix);
    }

    let mut result = prefix.trim_end_matches('/').to_string();
    result.push('/');
    result.push_str(suffix.trim_start_matches('/'));
    result
}

// impl Drop for MntNamespace {
//     fn drop(&mut self) {
//         log::warn!("mntns (level: {}) dropped", self.ns_common.level);
//     }
// }
