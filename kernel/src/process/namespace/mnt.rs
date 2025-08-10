use crate::{
    filesystem::vfs::{
        FileSystem, IndexNode, MountFS,
        mount::{MountList, MountPath},
    },
    libs::{once::Once, spinlock::SpinLock},
    process::{ProcessManager, fork::CloneFlags, namespace::NamespaceType},
};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use system_error::SystemError;

use super::{NamespaceOps, nsproxy::NsCommon, user_namespace::UserNamespace};

static mut INIT_MNT_NAMESPACE: Option<Arc<MntNamespace>> = None;

/// 初始化root mount namespace
pub fn mnt_namespace_init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        INIT_MNT_NAMESPACE = Some(MntNamespace::new_root());
    });
}

int_like!(MntSharedGroupId, usize);

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
    /// 父namespace的弱引用
    _parent: Option<Weak<MntNamespace>>,
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
        let ramfs = MountFS::new(ramfs, None, MountPropagation::new_private(), None);

        let result = Arc::new_cyclic(|self_ref| Self {
            ns_common: NsCommon::new(0, NamespaceType::Mount),
            self_ref: self_ref.clone(),
            _parent: None,
            _user_ns: super::user_namespace::INIT_USER_NAMESPACE.clone(),
            root_mountfs: ramfs.clone(),
            inner: SpinLock::new(InnerMntNamespace {
                mount_list,
                _dead: false,
            }),
        });
        ramfs.set_namespace(Arc::downgrade(&result));

        return result;
    }

    /// 强制替换本MountNamespace的根挂载文件系统
    ///
    /// 本方法仅供dragonos初始化时使用
    pub unsafe fn force_change_root_mountfs(&self, new_root: Arc<MountFS>) {
        let ptr = self as *const Self as *mut Self;
        let self_mut = (ptr).as_mut().unwrap();
        self_mut.root_mountfs = new_root;
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
    pub fn copy_mnt_ns(
        &self,
        clone_flags: &CloneFlags,
        _user_ns: Arc<UserNamespace>,
    ) -> Result<Arc<MntNamespace>, SystemError> {
        if !clone_flags.contains(CloneFlags::CLONE_NEWNS) {
            // Return the current mount namespace if CLONE_NEWNS is not set
            return Ok(self.self_ref.upgrade().unwrap());
        }

        todo!("Implement MntNamespace::copy_mnt_ns");
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
        mount_path: Arc<MountPath>,
        mntfs: Arc<MountFS>,
    ) -> Result<(), SystemError> {
        self.inner.lock().mount_list.insert(mount_path, mntfs);
        return Ok(());
    }

    pub fn remove_mount(&self, mount_path: &str) -> Option<Arc<MountFS>> {
        return self.inner.lock().mount_list.remove(mount_path);
    }

    pub fn get_mount_point(&self, mount_point: &str) -> Option<(String, String, Arc<MountFS>)> {
        self.inner.lock().mount_list.get_mount_point(mount_point)
    }
}

/// Manages mount propagation relationships and state for mount points.
///
/// This struct tracks how mount events (mount, unmount, remount) propagate between
/// mount points according to their propagation types. It maintains relationships
/// between shared mounts, slave mounts, and their propagation groups.
#[derive(Clone)]
pub struct MountPropagation {
    /// The type of propagation behavior for this mount
    pub prop_type: PropagationType,
    /// Group ID for shared mounts that can propagate events to each other
    pub shared_group_id: Option<MntSharedGroupId>,
    /// Reference to the master mount for slave mounts
    pub master: Option<Weak<MountFS>>,
    /// List of slave mounts that receive events from this mount
    pub slaves: Vec<Weak<MountFS>>,
    /// Peer group ID for complex propagation relationships
    pub peer_group_id: Option<MntSharedGroupId>,
    /// Counter to prevent infinite loops during event propagation
    pub propagation_count: u32,
}

/// Defines the propagation type for mount points, controlling how mount events are shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropagationType {
    /// Mount events do not propagate to or from this mount
    Private,
    /// Mount events propagate bidirectionally with other mounts in the same peer group
    Shared,
    /// Mount events propagate from the master mount to this slave mount
    Slave,
    /// Mount cannot be bind mounted and events do not propagate
    Unbindable,
}

impl MountPropagation {
    pub fn new_private() -> Arc<Self> {
        Arc::new(Self {
            prop_type: PropagationType::Private,
            shared_group_id: None,
            master: None,
            slaves: Vec::new(),
            peer_group_id: None,
            propagation_count: 0,
        })
    }

    pub fn new_shared(group_id: MntSharedGroupId) -> Arc<Self> {
        Arc::new(Self {
            prop_type: PropagationType::Shared,
            shared_group_id: Some(group_id),
            master: None,
            slaves: Vec::new(),
            peer_group_id: Some(group_id),
            propagation_count: 0,
        })
    }

    pub fn new_slave(master: Weak<MountFS>) -> Arc<Self> {
        Arc::new(Self {
            prop_type: PropagationType::Slave,
            shared_group_id: None,
            master: Some(master),
            slaves: Vec::new(),
            peer_group_id: None,
            propagation_count: 0,
        })
    }

    pub fn new_unbindable() -> Arc<Self> {
        Arc::new(Self {
            prop_type: PropagationType::Unbindable,
            shared_group_id: None,
            master: None,
            slaves: Vec::new(),
            peer_group_id: None,
            propagation_count: 0,
        })
    }

    /// 添加一个从属挂载
    pub fn add_slave(&mut self, slave: Weak<MountFS>) {
        self.slaves.push(slave);
    }

    /// 移除一个从属挂载
    pub fn remove_slave(&mut self, slave: &Weak<MountFS>) {
        self.slaves.retain(|s| !Weak::ptr_eq(s, slave));
    }

    /// 清理无效的从属挂载引用
    pub fn cleanup_stale_slaves(&mut self) {
        self.slaves.retain(|s| s.upgrade().is_some());
    }

    /// 重置传播计数器
    pub fn reset_propagation_count(&mut self) {
        self.propagation_count = 0;
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
