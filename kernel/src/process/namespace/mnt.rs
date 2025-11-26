use crate::{
    filesystem::vfs::{
        mount::{MountFlags, MountList, MountPath},
        FileSystem, IndexNode, InodeId, MountFS,
    },
    libs::{once::Once, spinlock::SpinLock},
    process::{fork::CloneFlags, namespace::NamespaceType, ProcessManager},
};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use system_error::SystemError;

use super::{
    nsproxy::NsCommon,
    propagation::{register_peer, MountPropagation},
    user_namespace::UserNamespace,
    NamespaceOps,
};

static mut INIT_MNT_NAMESPACE: Option<Arc<MntNamespace>> = None;

/// 初始化root mount namespace
pub fn mnt_namespace_init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        INIT_MNT_NAMESPACE = Some(MntNamespace::new_root());
    });
}

/// 获取全局的根挂载namespace
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
            MountPropagation::new_private(),
            None,
            MountFlags::empty(),
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

    /// 强制替换本MountNamespace的根挂载文件系统
    ///
    /// 本方法仅供dragonos初始化时使用
    pub unsafe fn force_change_root_mountfs(&self, new_root: Arc<MountFS>) {
        let inner_guard = self.inner.lock();
        let ptr = self as *const Self as *mut Self;
        let self_mut = (ptr).as_mut().unwrap();
        self_mut.root_mountfs = new_root.clone();
        let (path, _, _) = inner_guard.mount_list.get_mount_point("/").unwrap();

        inner_guard.mount_list.insert(None, path, new_root);

        // update mount list ino
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

        // 由于root mntfs比较特殊，因此单独复制。
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

        let new_mntns = self.copy_with_mountfs(new_root_mntfs, user_ns);
        new_mntns
            .add_mount(
                None,
                Arc::new(MountPath::from("/")),
                new_mntns.root_mntfs().clone(),
            )
            .expect("Failed to add root mount");

        for x in inner.mount_list.clone_inner().values() {
            if Arc::ptr_eq(x, new_mntns.root_mntfs()) {
                continue; // Skip the root mountfs
            }
        }
        // 将root mntfs下的所有挂载点复制到新的mntns中
        for (ino, mfs) in old_root_mntfs.mountpoints().iter() {
            let mount_path = inner
                .mount_list
                .get_mount_path_by_ino(*ino)
                .ok_or_else(|| {
                    panic!(
                        "mount_path not found for inode {:?}, mfs name: {}",
                        ino,
                        mfs.name()
                    );
                })
                .unwrap();

            queue.push(MountFSCopyInfo {
                old_mount_fs: mfs.clone(),
                parent_mount_fs: new_mntns.root_mntfs().clone(),
                self_mp_inode_id: *ino,
                mount_path,
            });
        }

        // 处理队列中的挂载点
        while let Some(data) = queue.pop() {
            let old_self_mp = data.old_mount_fs.self_mountpoint().unwrap();
            let new_self_mp = old_self_mp.clone_with_new_mount_fs(data.parent_mount_fs.clone());
            let new_mount_fs = data.old_mount_fs.deepcopy(Some(new_self_mp));

            // If the old mount was shared, register the new mount in the same peer group
            // This establishes the peer relationship for cross-namespace propagation
            let old_propagation = data.old_mount_fs.propagation();
            if old_propagation.is_shared() {
                let group_id = old_propagation.peer_group_id();
                register_peer(group_id, &new_mount_fs);
                log::debug!(
                    "copy_mnt_ns: registered new mount in peer group {}",
                    group_id.data()
                );
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

            // 原有的挂载点的子挂载点加入队列中

            for (child_ino, child_mfs) in data.old_mount_fs.mountpoints().iter() {
                queue.push(MountFSCopyInfo {
                    old_mount_fs: child_mfs.clone(),
                    parent_mount_fs: new_mount_fs.clone(),
                    self_mp_inode_id: *child_ino,
                    mount_path: inner
                        .mount_list
                        .get_mount_path_by_ino(*child_ino)
                        .expect("mount_path not found"),
                });
            }
        }

        // todo: 注册到procfs

        // 返回新创建的mount namespace
        Ok(new_mntns)
    }

    pub fn root_mntfs(&self) -> &Arc<MountFS> {
        &self.root_mountfs
    }

    /// 获取该挂载命名空间的根inode
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
        return Ok(());
    }

    pub fn mount_list(&self) -> Arc<MountList> {
        self.inner.lock().mount_list.clone()
    }

    pub fn remove_mount(&self, mount_path: &str) -> Option<Arc<MountFS>> {
        return self.inner.lock().mount_list.remove(mount_path);
    }

    pub fn get_mount_point(
        &self,
        mount_point: &str,
    ) -> Option<(Arc<MountPath>, String, Arc<MountFS>)> {
        self.inner.lock().mount_list.get_mount_point(mount_point)
    }
}

impl ProcessManager {
    /// 获取当前进程的挂载namespace
    pub fn current_mntns() -> Arc<MntNamespace> {
        if Self::initialized() {
            ProcessManager::current_pcb().nsproxy.read().mnt_ns.clone()
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

// impl Drop for MntNamespace {
//     fn drop(&mut self) {
//         log::warn!("mntns (level: {}) dropped", self.ns_common.level);
//     }
// }
