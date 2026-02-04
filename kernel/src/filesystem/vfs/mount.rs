use core::{
    any::Any,
    fmt::Debug,
    hash::Hash,
    sync::atomic::{compiler_fence, Ordering},
};

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashMap;
use ida::IdAllocator;
use system_error::SystemError;

use crate::libs::mutex::{Mutex, MutexGuard};
use crate::{
    driver::base::device::device_number::{DeviceNumber, Major},
    filesystem::{
        page_cache::PageCache,
        vfs::{fcntl::AtFlags, syscall::RenameFlags, vcore::do_mkdir_at},
    },
    libs::{casting::DowncastArc, lazy_init::Lazy, rwsem::RwSem},
    mm::{fault::PageFaultMessage, VmFaultReason},
    process::{
        namespace::{
            mnt::MntNamespace,
            propagation::{
                propagate_mount, propagate_umount, register_peer, unregister_peer, MountPropagation,
            },
        },
        ProcessManager,
    },
};

use super::{
    file::FileFlags, utils::DName, FilePrivateData, FileSystem, FileType, IndexNode, InodeId,
    InodeMode, PollableInode, SuperBlock,
};

bitflags! {
    /// Mount flags for filesystem independent mount options
    /// These flags correspond to the MS_* constants in Linux
    ///
    /// Reference: https://code.dragonos.org.cn/xref/linux-6.6.21/include/uapi/linux/mount.h#13
    pub struct MountFlags: u32 {
        /// Mount read-only (MS_RDONLY)
        const RDONLY = 1;
        /// Ignore suid and sgid bits (MS_NOSUID)
        const NOSUID = 2;
        /// Disallow access to device special files (MS_NODEV)
        const NODEV = 4;
        /// Disallow program execution (MS_NOEXEC)
        const NOEXEC = 8;
        /// Writes are synced at once (MS_SYNCHRONOUS)
        const SYNCHRONOUS = 16;
        /// Alter flags of a mounted FS (MS_REMOUNT)
        const REMOUNT = 32;
        /// Allow mandatory locks on an FS (MS_MANDLOCK)
        const MANDLOCK = 64;
        /// Directory modifications are synchronous (MS_DIRSYNC)
        const DIRSYNC = 128;
        /// Do not follow symlinks (MS_NOSYMFOLLOW)
        const NOSYMFOLLOW = 256;
        /// Do not update access times (MS_NOATIME)
        const NOATIME = 1024;
        /// Do not update directory access times (MS_NODIRATIME)
        const NODIRATIME = 2048;
        /// Bind mount (MS_BIND)
        const BIND = 4096;
        /// Move mount (MS_MOVE)
        const MOVE = 8192;
        /// Recursive mount (MS_REC)
        const REC = 16384;
        /// Silent mount (MS_SILENT, deprecated MS_VERBOSE)
        const SILENT = 32768;
        /// VFS does not apply the umask (MS_POSIXACL)
        const POSIXACL = 1 << 16;
        /// Change to unbindable (MS_UNBINDABLE)
        const UNBINDABLE = 1 << 17;
        /// Change to private (MS_PRIVATE)
        const PRIVATE = 1 << 18;
        /// Change to slave (MS_SLAVE)
        const SLAVE = 1 << 19;
        /// Change to shared (MS_SHARED)
        const SHARED = 1 << 20;
        /// Update atime relative to mtime/ctime (MS_RELATIME)
        const RELATIME = 1 << 21;
        /// This is a kern_mount call (MS_KERNMOUNT)
        const KERNMOUNT = 1 << 22;
        /// Update inode I_version field (MS_I_VERSION)
        const I_VERSION = 1 << 23;
        /// Always perform atime updates (MS_STRICTATIME)
        const STRICTATIME = 1 << 24;
        /// Update the on-disk [acm]times lazily (MS_LAZYTIME)
        const LAZYTIME = 1 << 25;
        /// This is a submount (MS_SUBMOUNT)
        const SUBMOUNT = 1 << 26;
        /// Do not allow remote locking (MS_NOREMOTELOCK)
        const NOREMOTELOCK = 1 << 27;
        /// Do not perform security checks (MS_NOSEC)
        const NOSEC = 1 << 28;
        /// This mount has been created by the kernel (MS_BORN)
        const BORN = 1 << 29;
        /// This mount is active (MS_ACTIVE)
        const ACTIVE = 1 << 30;
        /// Mount flags not allowed from userspace (MS_NOUSER)
        const NOUSER = 1 << 31;

        /// Superblock flags that can be altered by MS_REMOUNT
        const RMT_MASK = MountFlags::RDONLY.bits() |
            MountFlags::SYNCHRONOUS.bits() |
            MountFlags::MANDLOCK.bits() |
            MountFlags::I_VERSION.bits() |
            MountFlags::LAZYTIME.bits();

        /// Old magic mount flag and mask
        const MGC_VAL = 0xC0ED0000; // Magic value for mount flags
        const MGC_MASK = 0xFFFF0000; // Mask for magic mount flags
    }
}

impl MountFlags {
    /// Convert mount flags to a comma-separated string representation
    ///
    /// This function converts MountFlags to a string format similar to /proc/mounts,
    /// such as "rw,nosuid,nodev,noexec,relatime".
    ///
    /// # Returns
    ///
    /// A String containing the mount options in comma-separated format.
    #[inline(never)]
    pub fn options_string(&self) -> String {
        let mut options = Vec::new();

        // Check read/write flag
        if self.contains(MountFlags::RDONLY) {
            options.push("ro");
        } else {
            options.push("rw");
        }

        // Check other flags
        if self.contains(MountFlags::NOSUID) {
            options.push("nosuid");
        }
        if self.contains(MountFlags::NODEV) {
            options.push("nodev");
        }
        if self.contains(MountFlags::NOEXEC) {
            options.push("noexec");
        }
        if self.contains(MountFlags::SYNCHRONOUS) {
            options.push("sync");
        }
        if self.contains(MountFlags::MANDLOCK) {
            options.push("mand");
        }
        if self.contains(MountFlags::DIRSYNC) {
            options.push("dirsync");
        }
        if self.contains(MountFlags::NOSYMFOLLOW) {
            options.push("nosymfollow");
        }
        if self.contains(MountFlags::NOATIME) {
            options.push("noatime");
        }
        if self.contains(MountFlags::NODIRATIME) {
            options.push("nodiratime");
        }
        if self.contains(MountFlags::RELATIME) {
            options.push("relatime");
        }
        if self.contains(MountFlags::STRICTATIME) {
            options.push("strictatime");
        }
        if self.contains(MountFlags::LAZYTIME) {
            options.push("lazytime");
        }

        // Mount propagation flags
        if self.contains(MountFlags::UNBINDABLE) {
            options.push("unbindable");
        }
        if self.contains(MountFlags::PRIVATE) {
            options.push("private");
        }
        if self.contains(MountFlags::SLAVE) {
            options.push("slave");
        }
        if self.contains(MountFlags::SHARED) {
            options.push("shared");
        }

        // Internal flags (typically not shown in /proc/mounts)
        // We'll skip flags like BIND, MOVE, REC, REMOUNT, etc. as they're
        // not typically displayed in mount options

        options.join(",")
    }
}

// MountId类型
int_like!(MountId, usize);

static MOUNT_ID_ALLOCATOR: Mutex<IdAllocator> =
    Mutex::new(IdAllocator::new(0, usize::MAX).unwrap());

impl MountId {
    fn alloc() -> Self {
        let id = MOUNT_ID_ALLOCATOR.lock().alloc().unwrap();

        MountId(id)
    }

    unsafe fn free(&mut self) {
        MOUNT_ID_ALLOCATOR.lock().free(self.0);
    }
}

/// @brief 挂载文件系统
/// 挂载文件系统的时候，套了MountFS这一层，以实现文件系统的递归挂载
pub struct MountFS {
    // MountFS内部的文件系统
    inner_filesystem: Arc<dyn FileSystem>,
    /// 用来存储InodeID->挂载点的MountFS的B树
    mountpoints: Mutex<BTreeMap<InodeId, Arc<MountFS>>>,
    /// 当前文件系统挂载到的那个挂载点的Inode
    self_mountpoint: RwSem<Option<Arc<MountFSInode>>>,
    /// 指向当前MountFS的弱引用
    self_ref: Weak<MountFS>,

    namespace: Lazy<Weak<MntNamespace>>,
    propagation: Arc<MountPropagation>,
    mount_id: MountId,

    mount_flags: MountFlags,

    /// 对于bind mount，存储bind target目录的inode。
    /// 当这个MountFS被用作根文件系统时，root_inode() 应该返回这个inode，
    /// 而不是inner_filesystem的root。
    /// 这是为了支持container场景：bind mount /tmp/xxx/rootfs 后，
    /// pivot_root到这个mount时，看到的应该是rootfs的内容，而不是底层tmpfs的根。
    bind_target_root: RwSem<Option<Arc<MountFSInode>>>,
}

impl Debug for MountFS {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MountFS")
            .field("mount_id", &self.mount_id)
            .finish()
    }
}

impl PartialEq for MountFS {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.self_ref(), &other.self_ref())
    }
}

impl Hash for MountFS {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.mount_id.hash(state);
    }
}

impl Eq for MountFS {}

/// @brief MountFS的Index Node 注意，这个IndexNode只是一个中间层。它的目的是将具体文件系统的Inode与挂载机制连接在一起。
#[derive(Debug)]
#[cast_to([sync] IndexNode)]
pub struct MountFSInode {
    /// 当前挂载点对应到具体的文件系统的Inode
    inner_inode: Arc<dyn IndexNode>,
    /// 当前Inode对应的MountFS
    mount_fs: Arc<MountFS>,
    /// 指向自身的弱引用
    self_ref: Weak<MountFSInode>,
}

impl MountFS {
    pub fn new(
        inner_filesystem: Arc<dyn FileSystem>,
        self_mountpoint: Option<Arc<MountFSInode>>,
        propagation: Arc<MountPropagation>,
        mnt_ns: Option<&Arc<MntNamespace>>,
        mount_flags: MountFlags,
    ) -> Arc<Self> {
        let result = Arc::new_cyclic(|self_ref| MountFS {
            inner_filesystem,
            mountpoints: Mutex::new(BTreeMap::new()),
            self_mountpoint: RwSem::new(self_mountpoint),
            self_ref: self_ref.clone(),
            namespace: Lazy::new(),
            propagation,
            mount_id: MountId::alloc(),
            mount_flags,
            bind_target_root: RwSem::new(None),
        });

        if let Some(mnt_ns) = mnt_ns {
            result.set_namespace(Arc::downgrade(mnt_ns));
        }

        result
    }

    pub fn deepcopy(&self, self_mountpoint: Option<Arc<MountFSInode>>) -> Arc<Self> {
        // Clone propagation state for the new mount copy
        let new_propagation = self.propagation.clone_for_copy();

        let mountfs = Arc::new_cyclic(|self_ref| MountFS {
            inner_filesystem: self.inner_filesystem.clone(),
            mountpoints: Mutex::new(BTreeMap::new()),
            self_mountpoint: RwSem::new(self_mountpoint),
            self_ref: self_ref.clone(),
            namespace: Lazy::new(),
            propagation: new_propagation,
            mount_id: MountId::alloc(),
            mount_flags: self.mount_flags,
            bind_target_root: RwSem::new(None),
        });

        return mountfs;
    }

    pub fn mount_flags(&self) -> MountFlags {
        self.mount_flags
    }

    /// 设置挂载标志
    ///
    /// 用于 MS_REMOUNT | MS_BIND 场景，修改已存在挂载的标志（如只读状态）
    pub fn set_mount_flags(&self, flags: MountFlags) {
        // 使用 unsafe 来修改不可变字段
        // 这是安全的，因为我们只修改挂载标志，且在挂载命名空间的锁保护下
        let ptr = self as *const Self as *mut Self;
        unsafe {
            (*ptr).mount_flags = flags;
        }
    }

    pub fn add_mount(&self, inode_id: InodeId, mount_fs: Arc<MountFS>) -> Result<(), SystemError> {
        // 检查是否已经存在同名的挂载点
        if self.mountpoints.lock().contains_key(&inode_id) {
            return Err(SystemError::EEXIST);
        }

        // 将新的挂载点添加到当前MountFS的挂载点列表中
        self.mountpoints.lock().insert(inode_id, mount_fs.clone());

        Ok(())
    }

    pub fn mountpoints(&self) -> MutexGuard<'_, BTreeMap<InodeId, Arc<MountFS>>> {
        self.mountpoints.lock()
    }

    pub fn propagation(&self) -> Arc<MountPropagation> {
        self.propagation.clone()
    }

    /// Get the mount ID
    pub fn mount_id(&self) -> MountId {
        self.mount_id
    }

    pub fn set_namespace(&self, namespace: Weak<MntNamespace>) {
        self.namespace.init(namespace);
    }

    /// 设置 bind mount 的 target 目录 inode。
    ///
    /// 当 bind mount 被用作根文件系统时（例如容器场景），
    /// root_inode() 应该返回这个 inode，而不是底层文件系统的根。
    pub fn set_bind_target_root(&self, target_root: Arc<MountFSInode>) {
        *self.bind_target_root.write() = Some(target_root);
    }

    /// 获取 bind mount 的 target 目录 inode（如果设置了）
    pub fn bind_target_root(&self) -> Option<Arc<MountFSInode>> {
        self.bind_target_root.read().clone()
    }

    pub fn fs_type(&self) -> &str {
        self.inner_filesystem.name()
    }

    #[inline(never)]
    pub fn self_mountpoint(&self) -> Option<Arc<MountFSInode>> {
        self.self_mountpoint.read().as_ref().cloned()
    }

    /// @brief 用Arc指针包裹MountFS对象。
    /// 本函数的主要功能为，初始化MountFS对象中的自引用Weak指针
    /// 本函数只应在构造器中被调用
    #[allow(dead_code)]
    #[deprecated]
    fn wrap(self) -> Arc<Self> {
        // 创建Arc指针
        let mount_fs: Arc<MountFS> = Arc::new(self);
        // 创建weak指针
        let weak: Weak<MountFS> = Arc::downgrade(&mount_fs);

        // 将Arc指针转为Raw指针并对其内部的self_ref字段赋值
        let ptr: *mut MountFS = mount_fs.as_ref() as *const Self as *mut Self;
        unsafe {
            (*ptr).self_ref = weak;
            // 返回初始化好的MountFS对象
            return mount_fs;
        }
    }

    /// @brief 获取挂载点的文件系统的root inode
    pub fn mountpoint_root_inode(&self) -> Arc<MountFSInode> {
        // 如果设置了 bind_target_root（用于 bind mount 场景），
        // 则返回 bind target 目录，而不是底层文件系统的根。
        // 这对于容器场景很关键：bind mount /tmp/xxx/rootfs 后，
        // 作为根文件系统时应该看到 rootfs 的内容。
        if let Some(bind_target) = self.bind_target_root() {
            return bind_target;
        }

        // 默认行为：返回底层文件系统的根
        return Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode: self.inner_filesystem.root_inode(),
            mount_fs: self.self_ref.upgrade().unwrap(),
            self_ref: self_ref.clone(),
        });
    }

    pub fn inner_filesystem(&self) -> Arc<dyn FileSystem> {
        return self.inner_filesystem.clone();
    }

    pub fn self_ref(&self) -> Arc<Self> {
        self.self_ref.upgrade().unwrap()
    }

    /// 卸载文件系统
    /// # Errors
    /// 如果当前文件系统是根文件系统，那么将会返回`EINVAL`
    pub fn umount(&self) -> Result<Arc<MountFS>, SystemError> {
        // Unregister from peer group before unmounting
        let propagation = self.propagation();
        if propagation.is_shared() {
            let group_id = propagation.peer_group_id();
            unregister_peer(group_id, &self.self_ref());
        }

        // 获取 self_mountpoint
        let self_mp = self.self_mountpoint();

        if let Some(mp) = self_mp {
            // 正常情况：有父挂载点
            let r = mp.do_umount();
            self.self_mountpoint.write().take();
            return r;
        }

        // 特殊情况：self_mountpoint 为 None，说明这曾经是根文件系统
        // 需要检查是否仍然是当前 namespace 的根
        // 如果 pivot_root 已经切换到新的根，旧的根应该可以被卸载
        let current_ns = ProcessManager::current_mntns();
        let is_current_root = Arc::ptr_eq(current_ns.root_mntfs(), &self.self_ref());

        if is_current_root {
            // 仍然是当前根，不能卸载
            return Err(SystemError::EINVAL);
        }

        // 不是当前根，说明是 pivot_root 后的旧根，可以卸载
        // 这种情况下，我们只需要清理挂载列表中的记录
        // 不需要调用 do_umount（因为没有父挂载点）
        // log::debug!(
        //     "[MountFS::umount] unmounting old root mount id={:?}",
        //     self.mount_id()
        // );

        // 从当前 namespace 的挂载列表中移除
        // 首先需要找到这个挂载的路径
        let mount_list = current_ns.mount_list();
        if let Some(mount_path) = mount_list.get_mount_path_by_mountfs(&self.self_ref()) {
            // log::debug!(
            //     "[MountFS::umount] removing old root from mount list: {:?}",
            //     mount_path
            // );
            current_ns.remove_mount(mount_path.as_str());
        }

        Ok(self.self_ref())
    }
}

impl Drop for MountFS {
    fn drop(&mut self) {
        // 释放MountId
        unsafe {
            self.mount_id.free();
        }
    }
}

impl MountFSInode {
    /// 获取当前 inode 所在的 MountFS
    pub fn mount_fs(&self) -> &Arc<MountFS> {
        &self.mount_fs
    }

    /// @brief 用Arc指针包裹MountFSInode对象。
    /// 本函数的主要功能为，初始化MountFSInode对象中的自引用Weak指针
    /// 本函数只应在构造器中被调用
    #[allow(dead_code)]
    #[deprecated]
    fn wrap(self) -> Arc<Self> {
        // 创建Arc指针
        let inode: Arc<MountFSInode> = Arc::new(self);
        // 创建Weak指针
        let weak: Weak<MountFSInode> = Arc::downgrade(&inode);
        // 将Arc指针转为Raw指针并对其内部的self_ref字段赋值
        compiler_fence(Ordering::SeqCst);
        let ptr: *mut MountFSInode = inode.as_ref() as *const Self as *mut Self;
        compiler_fence(Ordering::SeqCst);
        unsafe {
            (*ptr).self_ref = weak;
            compiler_fence(Ordering::SeqCst);

            // 返回初始化好的MountFSInode对象
            return inode;
        }
    }

    /// @brief 判断当前inode是否为它所在的文件系统的root inode
    fn is_mountpoint_root(&self) -> Result<bool, SystemError> {
        return Ok(self.inner_inode.fs().root_inode().metadata()?.inode_id
            == self.inner_inode.metadata()?.inode_id);
    }

    /// @brief 在挂载树上进行inode替换。
    /// 如果当前inode是父MountFS内的一个挂载点，那么，本函数将会返回挂载到这个挂载点下的文件系统的root inode.
    /// 如果当前inode在父MountFS内，但不是挂载点，那么说明在这里不需要进行inode替换，因此直接返回当前inode。
    ///
    /// @return Arc<MountFSInode>
    fn overlaid_inode(&self) -> Arc<MountFSInode> {
        // 某些情况下，底层 inode 可能已被删除或失效，此时 metadata() 可能返回错误
        // 为避免因 unwrap 导致内核 panic，这里将错误视作“非挂载点”，直接返回自身
        let inode_id = match self.metadata() {
            Ok(md) => md.inode_id,
            Err(e) => {
                log::warn!(
                    "MountFSInode::overlaid_inode: metadata() failed: {:?}; treat as non-mountpoint",
                    e
                );
                return self.self_ref.upgrade().unwrap();
            }
        };

        if let Some(sub_mountfs) = self.mount_fs.mountpoints.lock().get(&inode_id) {
            return sub_mountfs.mountpoint_root_inode();
        } else {
            return self.self_ref.upgrade().unwrap();
        }
    }

    fn do_find(&self, name: &str) -> Result<Arc<MountFSInode>, SystemError> {
        // 直接调用当前inode所在的文件系统的find方法进行查找
        // 由于向下查找可能会跨越文件系统的边界，因此需要尝试替换inode
        let inner_inode = self.inner_inode.find(name)?;
        return Ok(Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode,
            mount_fs: self.mount_fs.clone(),
            self_ref: self_ref.clone(),
        })
        .overlaid_inode());
    }

    pub(super) fn do_parent(&self) -> Result<Arc<MountFSInode>, SystemError> {
        if self.is_mountpoint_root()? {
            // 当前inode是它所在的文件系统的root inode
            match self.mount_fs.self_mountpoint() {
                Some(inode) => {
                    // `inode` 是“父挂载树中”的挂载点 inode。
                    // Linux 语义：从被挂载文件系统的根目录向上（..）应当回到挂载点的父目录，
                    // 并且后续路径遍历应当发生在父挂载（inode.mount_fs）上。
                    //
                    // 这里直接复用挂载点 inode 的 do_parent()，确保 mount_fs 正确切换。
                    return inode.do_parent();
                }
                None => {
                    return Ok(self.self_ref.upgrade().unwrap());
                }
            }
        } else {
            let inner_inode = self.inner_inode.parent()?;
            // 向上查找时，不会跨过文件系统的边界，因此直接调用当前inode所在的文件系统的find方法进行查找
            return Ok(Arc::new_cyclic(|self_ref| MountFSInode {
                inner_inode,
                mount_fs: self.mount_fs.clone(),
                self_ref: self_ref.clone(),
            }));
        }
    }

    /// 移除挂载点下的文件系统
    fn do_umount(&self) -> Result<Arc<MountFS>, SystemError> {
        if self.metadata()?.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        let mountpoint_id = self.inner_inode.metadata()?.inode_id;

        // Get the child mount that will be unmounted
        let child_mount = self
            .mount_fs
            .mountpoints
            .lock()
            .get(&mountpoint_id)
            .cloned();

        if let Some(ref child) = child_mount {
            // Unregister from peer group if shared
            let child_prop = child.propagation();
            if child_prop.is_shared() {
                unregister_peer(child_prop.peer_group_id(), child);
            }
        }

        // Propagate umount to peers and slaves of the parent mount
        let parent_prop = self.mount_fs.propagation();
        if parent_prop.is_shared() {
            if let Err(e) = propagate_umount(&self.mount_fs, mountpoint_id) {
                log::warn!("do_umount: propagation failed: {:?}", e);
            }
        }

        // Remove the mount
        return self
            .mount_fs
            .mountpoints
            .lock()
            .remove(&mountpoint_id)
            .ok_or(SystemError::ENOENT);
    }

    #[inline(never)]
    fn do_absolute_path(&self) -> Result<String, SystemError> {
        let mut current = self.self_ref.upgrade().unwrap();

        // For special inode, we can directly get the absolute path
        if let Ok(p) = current.inner_inode.absolute_path() {
            return Ok(p);
        }

        let mut path_parts = Vec::new();

        // 注意：不同文件系统的 inode_id 空间可能互相独立，不能用“全局根 inode_id”作为终止条件。
        // 正确做法应当按挂载树向上走，直到到达“命名空间根”（即 rootfs 的 mount，self_mountpoint 为 None）。
        loop {
            // 到达全局根（该 mount 没有挂载点）：结束
            if current.is_mountpoint_root()? && current.mount_fs.self_mountpoint().is_none() {
                break;
            }

            let name = current.dname()?;
            path_parts.push(name.0);

            // 防循环检查：如果路径深度超过1024，抛出警告
            if path_parts.len() > 1024 {
                #[inline(never)]
                fn __log_warn(cur: usize) {
                    log::warn!(
                        "Path depth exceeds 1024, possible infinite loop. cur: {}",
                        cur
                    );
                }
                __log_warn(current.metadata().unwrap().inode_id.data());
                return Err(SystemError::ELOOP);
            }

            let parent = current.do_parent()?;
            if Arc::ptr_eq(&parent, &current) {
                // parent == self 但还没达到全局根，说明挂载树信息不完整或出现环
                log::warn!(
                    "absolute_path: parent == self before reaching namespace root, inode_id={}",
                    current.metadata().unwrap().inode_id.data()
                );
                return Err(SystemError::ELOOP);
            }
            current = parent;
        }

        // 由于我们从叶子节点向上遍历到根节点，所以需要反转路径部分
        path_parts.reverse();

        // 构建最终的绝对路径字符串
        let mut absolute_path = String::with_capacity(
            path_parts.iter().map(|s| s.len()).sum::<usize>() + path_parts.len(),
        );
        for part in path_parts {
            absolute_path.push('/');
            absolute_path.push_str(&part);
        }

        if absolute_path.is_empty() {
            absolute_path.push('/');
        }
        Ok(absolute_path)
    }

    pub fn clone_with_new_mount_fs(&self, mount_fs: Arc<MountFS>) -> Arc<MountFSInode> {
        Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode: self.inner_inode.clone(),
            mount_fs,
            self_ref: self_ref.clone(),
        })
    }

    /// 创建一个新的 MountFSInode
    ///
    /// # 参数
    /// - inner_inode: 底层文件系统的 inode
    /// - mount_fs: 所属的 MountFS
    pub fn new(inner_inode: Arc<dyn IndexNode>, mount_fs: Arc<MountFS>) -> Arc<MountFSInode> {
        Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode,
            mount_fs,
            self_ref: self_ref.clone(),
        })
    }
}

impl IndexNode for MountFSInode {
    fn open(
        &self,
        data: MutexGuard<FilePrivateData>,
        flags: &FileFlags,
    ) -> Result<(), SystemError> {
        return self.inner_inode.open(data, flags);
    }

    fn mmap(&self, start: usize, len: usize, offset: usize) -> Result<(), SystemError> {
        return self.inner_inode.mmap(start, len, offset);
    }

    fn sync(&self) -> Result<(), SystemError> {
        return self.inner_inode.sync();
    }

    fn fadvise(
        &self,
        file: &Arc<super::file::File>,
        offset: i64,
        len: i64,
        advise: i32,
    ) -> Result<usize, SystemError> {
        return self.inner_inode.fadvise(file, offset, len, advise);
    }

    fn close(&self, data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        self.inner_inode.close(data)
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let inner_inode = self
            .inner_inode
            .create_with_data(name, file_type, mode, data)?;
        return Ok(Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode,
            mount_fs: self.mount_fs.clone(),
            self_ref: self_ref.clone(),
        }));
    }

    fn truncate(&self, len: usize) -> Result<(), SystemError> {
        return self.inner_inode.truncate(len);
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        return self.inner_inode.read_at(offset, len, buf, data);
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        return self.inner_inode.write_at(offset, len, buf, data);
    }

    fn read_direct(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.inner_inode.read_direct(offset, len, buf, data)
    }

    fn write_direct(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.inner_inode.write_direct(offset, len, buf, data)
    }

    #[inline]
    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.mount_fs.clone();
    }

    #[inline]
    fn as_any_ref(&self) -> &dyn core::any::Any {
        return self.inner_inode.as_any_ref();
    }

    #[inline]
    fn metadata(&self) -> Result<super::Metadata, SystemError> {
        let mut md = self.inner_inode.metadata()?;

        // 为每个挂载点提供稳定且唯一的 st_dev（通过 metadata.dev_id）。
        // 这里针对的是底层文件系统没有提供dev_id的情况
        if md.dev_id == 0 {
            let mnt_id: usize = self.mount_fs.mount_id().into();
            let minor = (mnt_id as u32) & DeviceNumber::MINOR_MASK;
            md.dev_id = DeviceNumber::new(Major::UNNAMED_MAJOR, minor).data() as usize;
        }

        Ok(md)
    }

    #[inline]
    fn set_metadata(&self, metadata: &super::Metadata) -> Result<(), SystemError> {
        return self.inner_inode.set_metadata(metadata);
    }

    #[inline]
    fn resize(&self, len: usize) -> Result<(), SystemError> {
        return self.inner_inode.resize(len);
    }

    #[inline]
    fn create(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let inner_inode = self.inner_inode.create(name, file_type, mode)?;
        return Ok(Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode,
            mount_fs: self.mount_fs.clone(),
            self_ref: self_ref.clone(),
        }));
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        // 文件系统实现期望 `other` 是同一具体文件系统的 inode（例如 LockedExt4Inode）。当启用 VFS 挂载包装时，
        // `other` 通常是 `MountFSInode`，这会导致文件系统层面的向下转换失败并错误地返回 EINVAL。
        //
        // 因此在link之前，我们需要解包挂载包装器（与 move_to 相同）。
        let other_inner: Arc<dyn IndexNode> = other
            .clone()
            .downcast_arc::<MountFSInode>()
            .map(|mnt| mnt.inner_inode.clone())
            .unwrap_or_else(|| other.clone());

        return self.inner_inode.link(name, &other_inner);
    }

    /// @brief 在挂载文件系统中删除文件/文件夹
    #[inline]
    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        let inode_id = self.inner_inode.find(name)?.metadata()?.inode_id;

        // 先检查这个inode是否为一个挂载点，如果当前inode是一个挂载点，那么就不能删除这个inode
        if self.mount_fs.mountpoints.lock().contains_key(&inode_id) {
            return Err(SystemError::EBUSY);
        }
        // 调用内层的inode的方法来删除这个inode
        return self.inner_inode.unlink(name);
    }

    #[inline]
    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        let inode_id = self.inner_inode.find(name)?.metadata()?.inode_id;

        // 先检查这个inode是否为一个挂载点，如果当前inode是一个挂载点，那么就不能删除这个inode
        if self.mount_fs.mountpoints.lock().contains_key(&inode_id) {
            return Err(SystemError::EBUSY);
        }
        // 调用内层的rmdir的方法来删除这个inode
        let r = self.inner_inode.rmdir(name);

        return r;
    }

    #[inline]
    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
        flags: RenameFlags,
    ) -> Result<(), SystemError> {
        // Filesystem implementations generally expect `target` to be an inode
        // of the same concrete FS (e.g. tmpfs' LockedTmpfsInode). When VFS
        // mount wrapping is enabled, `target` is often a `MountFSInode`, which
        // would make FS-level downcasts fail and incorrectly return EINVAL.
        //
        // So we unwrap the mount wrapper before delegating.
        let target_inner: Arc<dyn IndexNode> = target
            .clone()
            .downcast_arc::<MountFSInode>()
            .map(|mnt| mnt.inner_inode.clone())
            .unwrap_or_else(|| target.clone());

        return self
            .inner_inode
            .move_to(old_name, &target_inner, new_name, flags);
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        match name {
            // 查找的是当前目录
            "" | "." => self
                .self_ref
                .upgrade()
                .map(|inode| inode as Arc<dyn IndexNode>)
                .ok_or(SystemError::ENOENT),
            // 往父级查找
            ".." => self.parent(),
            // 在当前目录下查找
            // 直接调用当前inode所在的文件系统的find方法进行查找
            // 由于向下查找可能会跨越文件系统的边界，因此需要尝试替换inode
            _ => self.do_find(name).map(|inode| inode as Arc<dyn IndexNode>),
        }
    }

    #[inline]
    fn get_entry_name(&self, ino: InodeId) -> Result<alloc::string::String, SystemError> {
        return self.inner_inode.get_entry_name(ino);
    }

    #[inline]
    fn get_entry_name_and_metadata(
        &self,
        ino: InodeId,
    ) -> Result<(alloc::string::String, super::Metadata), SystemError> {
        return self.inner_inode.get_entry_name_and_metadata(ino);
    }

    #[inline]
    fn ioctl(
        &self,
        cmd: u32,
        data: usize,
        private_data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        return self.inner_inode.ioctl(cmd, data, private_data);
    }

    #[inline]
    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, SystemError> {
        return self.inner_inode.list();
    }

    fn mount(
        &self,
        fs: Arc<dyn FileSystem>,
        mount_flags: MountFlags,
    ) -> Result<Arc<MountFS>, SystemError> {
        let metadata = self.inner_inode.metadata()?;
        if metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 若已有挂载系统，保证MountFS只包一层
        let to_mount_fs = fs
            .clone()
            .downcast_arc::<MountFS>()
            .map(|it| it.inner_filesystem())
            .unwrap_or(fs);

        // Check if parent mount is shared - if so, new mount should also be shared
        let parent_propagation = self.mount_fs.propagation();
        let new_propagation = if parent_propagation.is_shared() {
            // Create shared propagation with a new group
            MountPropagation::new_shared()
        } else {
            MountPropagation::new_private()
        };

        let new_mount_fs = MountFS::new(
            to_mount_fs,
            Some(self.self_ref.upgrade().unwrap()),
            new_propagation,
            Some(&ProcessManager::current_mntns()),
            mount_flags,
        );

        // Perform all potentially-failing operations first before registering in peer group
        self.mount_fs
            .add_mount(metadata.inode_id, new_mount_fs.clone())?;

        let mount_path = self.absolute_path();
        let mount_path = Arc::new(MountPath::from(mount_path?));
        ProcessManager::current_mntns().add_mount(
            Some(metadata.inode_id),
            mount_path,
            new_mount_fs.clone(),
        )?;

        // Now that all operations succeeded, register in peer group if shared
        // This ensures we don't leave dangling registrations if earlier operations fail
        if new_mount_fs.propagation().is_shared() {
            let group_id = new_mount_fs.propagation().peer_group_id();
            register_peer(group_id, &new_mount_fs);
        }

        // Propagate this mount to all peers and slaves of the parent mount
        if parent_propagation.is_shared() {
            if let Err(e) = propagate_mount(&self.mount_fs, metadata.inode_id, &new_mount_fs) {
                log::warn!("mount: propagation failed: {:?}", e);
                // Don't fail the mount, just log warning
            }
        }

        return Ok(new_mount_fs);
    }

    fn mount_from(&self, from: Arc<dyn IndexNode>) -> Result<Arc<MountFS>, SystemError> {
        let metadata = self.metadata()?;
        if from.metadata()?.file_type != FileType::Dir || metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        if self.is_mountpoint_root()? {
            return Err(SystemError::EBUSY);
        }
        // debug!("from {:?}, to {:?}", from, self);
        let new_mount_fs = from.umount()?;
        self.mount_fs
            .add_mount(metadata.inode_id, new_mount_fs.clone())?;
        // 更新当前挂载点的self_mountpoint
        new_mount_fs
            .self_mountpoint
            .write()
            .replace(self.self_ref.upgrade().unwrap());
        let mntns = ProcessManager::current_mntns();

        let mount_path = mntns
            .mount_list()
            .get_mount_path_by_mountfs(&new_mount_fs)
            .unwrap_or_else(|| {
                panic!(
                    "MountFS::mount_from: failed to get mount path for {:?}",
                    self.mount_fs.name()
                );
            });

        mntns.mount_list().remove(mount_path.as_str());
        ProcessManager::current_mntns()
            .add_mount(Some(metadata.inode_id), mount_path, new_mount_fs.clone())
            .expect("MountFS::mount_from: failed to add mount.");
        return Ok(new_mount_fs);
    }

    fn umount(&self) -> Result<Arc<MountFS>, SystemError> {
        if !self.is_mountpoint_root()? {
            return Err(SystemError::EINVAL);
        }
        return self.mount_fs.umount();
    }

    fn absolute_path(&self) -> Result<String, SystemError> {
        self.do_absolute_path()
    }

    #[inline]
    fn mknod(
        &self,
        filename: &str,
        mode: InodeMode,
        dev_t: DeviceNumber,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let inner_inode = self.inner_inode.mknod(filename, mode, dev_t)?;
        return Ok(Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode,
            mount_fs: self.mount_fs.clone(),
            self_ref: self_ref.clone(),
        }));
    }

    #[inline]
    fn special_node(&self) -> Option<super::SpecialNodeData> {
        self.inner_inode.special_node()
    }

    /// 若不支持，则调用第二种情况来从父目录获取文件名
    /// # Performance
    /// 应尽可能引入DName，
    /// 在默认情况下，性能非常差！！！
    fn dname(&self) -> Result<DName, SystemError> {
        if self.is_mountpoint_root()? {
            if let Some(inode) = self.mount_fs.self_mountpoint() {
                return inode.inner_inode.dname();
            }
        }
        return self.inner_inode.dname();
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        return self.do_parent().map(|inode| inode as Arc<dyn IndexNode>);
    }

    fn page_cache(&self) -> Option<Arc<PageCache>> {
        self.inner_inode.page_cache()
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        self.inner_inode.as_pollable_inode()
    }

    fn read_sync(&self, offset: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        self.inner_inode.read_sync(offset, buf)
    }

    fn write_sync(&self, offset: usize, buf: &[u8]) -> Result<usize, SystemError> {
        self.inner_inode.write_sync(offset, buf)
    }

    fn getxattr(&self, name: &str, buf: &mut [u8]) -> Result<usize, SystemError> {
        self.inner_inode.getxattr(name, buf)
    }

    fn setxattr(&self, name: &str, value: &[u8]) -> Result<usize, SystemError> {
        self.inner_inode.setxattr(name, value)
    }
}

impl FileSystem for MountFS {
    fn support_readahead(&self) -> bool {
        self.inner_filesystem.support_readahead()
    }
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        match self.self_mountpoint() {
            Some(inode) => return inode.mount_fs.root_inode(),
            // 当前文件系统是rootfs
            None => self.mountpoint_root_inode(),
        }
    }

    fn info(&self) -> super::FsInfo {
        return self.inner_filesystem.info();
    }

    /// @brief 本函数用于实现动态转换。
    /// 具体的文件系统在实现本函数时，最简单的方式就是：直接返回self
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        self.inner_filesystem.name()
    }
    fn super_block(&self) -> SuperBlock {
        let mut sb = self.inner_filesystem.super_block();
        sb.flags = self.mount_flags.bits() as u64;
        sb
    }

    unsafe fn fault(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        self.inner_filesystem.fault(pfm)
    }

    unsafe fn map_pages(
        &self,
        pfm: &mut PageFaultMessage,
        start_pgoff: usize,
        end_pgoff: usize,
    ) -> VmFaultReason {
        self.inner_filesystem.map_pages(pfm, start_pgoff, end_pgoff)
    }
}

/// MountList
/// ```rust
/// use alloc::collection::BTreeSet;
/// let map = BTreeSet::from([
///     "/sys", "/dev", "/", "/bin", "/proc"
/// ]);
/// assert_eq!(format!("{:?}", map), "{\"/\", \"/bin\", \"/dev\", \"/proc\", \"/sys\"}");
/// // {"/", "/bin", "/dev", "/proc", "/sys"}
/// ```
#[derive(PartialEq, Eq, Debug, Hash)]
pub struct MountPath(String);

impl From<&str> for MountPath {
    fn from(value: &str) -> Self {
        Self(String::from(value))
    }
}

impl From<String> for MountPath {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl AsRef<str> for MountPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialOrd for MountPath {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MountPath {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        let self_dep = self.0.chars().filter(|c| *c == '/').count();
        let othe_dep = other.0.chars().filter(|c| *c == '/').count();
        if self_dep == othe_dep {
            // 深度一样时反序来排
            // 根目录和根目录下的文件的绝对路径都只有一个'/'
            other.0.cmp(&self.0)
        } else {
            // 根据深度，深度
            othe_dep.cmp(&self_dep)
        }
    }
}

impl MountPath {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// 维护一个挂载点的记录，以支持特定于文件系统的索引
pub struct MountList {
    inner: RwSem<InnerMountList>,
}

#[derive(Clone, Debug)]
struct MountRecord {
    fs: Arc<MountFS>,
    ino: Option<InodeId>,
}

struct InnerMountList {
    /// 同一路径可能被重复挂载，按栈保存，栈顶为当前可见挂载。
    mounts: HashMap<Arc<MountPath>, Vec<MountRecord>>,
    /// 便于通过 fs 反查挂载点 inode。
    mfs2ino: HashMap<Arc<MountFS>, InodeId>,
    /// inode 到路径的映射，用于子挂载查找。
    ino2mp: HashMap<InodeId, Arc<MountPath>>,
}

impl MountList {
    /// # new - 创建新的MountList实例
    ///
    /// 创建一个空的挂载点列表。
    ///
    /// ## 返回值
    ///
    /// - `MountList`: 新的挂载点列表实例
    pub fn new() -> Arc<Self> {
        Arc::new(MountList {
            inner: RwSem::new(InnerMountList {
                mounts: HashMap::new(),
                ino2mp: HashMap::new(),
                mfs2ino: HashMap::new(),
            }),
        })
    }

    /// Inserts a filesystem mount point into the mount list.
    ///
    /// This function adds a new filesystem mount point to the mount list. If a mount point
    /// already exists at the specified path, it will be updated with the new filesystem.
    ///
    /// # Thread Safety
    /// This function is thread-safe as it uses a RwSem to ensure safe concurrent access.
    ///
    /// # Arguments
    /// * `ino` - An optional InodeId representing the inode of the `fs` mounted at.
    /// * `path` - The mount path where the filesystem will be mounted
    /// * `fs` - The filesystem instance to be mounted at the specified path
    #[inline(never)]
    pub fn insert(&self, ino: Option<InodeId>, path: Arc<MountPath>, fs: Arc<MountFS>) {
        let mut inner = self.inner.write();
        let entry = inner.mounts.entry(path.clone()).or_default();
        entry.push(MountRecord {
            fs: fs.clone(),
            ino,
        });
        if let Some(ino) = ino {
            inner.ino2mp.insert(ino, path.clone());
            inner.mfs2ino.insert(fs.clone(), ino);
        }
        // 若 ino 为 None（如根挂载），仍然保留 mounts 栈用于后续 pop。
    }

    /// # get_mount_point - 获取挂载点的路径
    ///
    /// 这个函数用于查找给定路径的挂载点。它搜索一个内部映射，找到与路径匹配的挂载点。
    ///
    /// ## 参数
    ///
    /// - `path: T`: 这是一个可转换为字符串的引用，表示要查找其挂载点的路径。
    ///
    /// ## 返回值
    ///
    /// - `Option<(String, String, Arc<MountFS>)>`:
    ///   - `Some((mount_point, rest_path, fs))`: 如果找到了匹配的挂载点，返回一个包含挂载点路径、剩余路径和挂载文件系统的元组。
    ///   - `None`: 如果没有找到匹配的挂载点，返回 None。
    #[inline(never)]
    #[allow(dead_code)]
    pub fn get_mount_point<T: AsRef<str>>(
        &self,
        path: T,
    ) -> Option<(Arc<MountPath>, String, Arc<MountFS>)> {
        self.inner
            .read()
            .mounts
            .iter()
            .filter_map(|(key, stack)| {
                let strkey = key.as_str();
                if let Some(rest) = path.as_ref().strip_prefix(strkey) {
                    return stack
                        .last()
                        .map(|rec| (key.clone(), rest.to_string(), rec.fs.clone()));
                }
                None
            })
            .next()
    }

    /// # remove - 移除挂载点
    ///
    /// 从挂载点管理器中移除一个挂载点。
    ///
    /// 此函数用于从挂载点管理器中移除一个已经存在的挂载点。如果挂载点不存在，则不进行任何操作。
    ///
    /// ## 参数
    ///
    /// - `path: T`: `T` 实现了 `Into<MountPath>`  trait，代表要移除的挂载点的路径。
    ///
    /// ## 返回值
    ///
    /// - `Option<Arc<MountFS>>`: 返回一个 `Arc<MountFS>` 类型的可选值，表示被移除的挂载点，如果挂载点不存在则返回 `None`。
    #[inline(never)]
    pub fn remove<T: Into<MountPath>>(&self, path: T) -> Option<Arc<MountFS>> {
        let mut inner = self.inner.write();
        let path: MountPath = path.into();
        if let Some(stack) = inner.mounts.get_mut(&path) {
            if let Some(rec) = stack.pop() {
                let empty = stack.is_empty();
                let rec_fs = rec.fs.clone();
                let rec_ino = rec.ino;
                if empty {
                    inner.mounts.remove(&path);
                }
                if let Some(ino) = inner.mfs2ino.remove(&rec_fs) {
                    inner.ino2mp.remove(&ino);
                }
                if let Some(ino) = rec_ino {
                    inner.ino2mp.remove(&ino);
                }
                return Some(rec_fs);
            }
        }
        None
    }

    /// # clone_inner - 克隆内部挂载点列表
    pub fn clone_inner(&self) -> HashMap<Arc<MountPath>, Arc<MountFS>> {
        self.inner
            .read()
            .mounts
            .iter()
            .map(|(p, stack)| (p.clone(), stack.last().unwrap().fs.clone()))
            .collect()
    }

    #[inline(never)]
    pub fn get_mount_path_by_ino(&self, ino: InodeId) -> Option<Arc<MountPath>> {
        self.inner.read().ino2mp.get(&ino).cloned()
    }

    #[inline(never)]
    pub fn get_mount_path_by_mountfs(&self, mountfs: &Arc<MountFS>) -> Option<Arc<MountPath>> {
        let inner = self.inner.read();
        inner
            .mfs2ino
            .get(mountfs)
            .and_then(|ino| inner.ino2mp.get(ino).cloned())
    }

    /// 根据文件系统查找对应的 MountFS
    /// 用于 pivot_root 等场景，需要从底层 inode 找到其所在的 MountFS
    pub fn find_mount_by_fs(&self, fs: &Arc<dyn FileSystem>) -> Option<Arc<MountFS>> {
        let inner = self.inner.read();
        // 遍历所有挂载点，找到文件系统相同的 MountFS
        for (_path, stack) in inner.mounts.iter() {
            if let Some(record) = stack.last() {
                // 比较 Arc 指针是否相同
                let inner_fs = record.fs.inner_filesystem();
                // 首先尝试直接比较 Arc 指针
                if Arc::ptr_eq(&inner_fs, fs) {
                    return Some(record.fs.clone());
                }
                // 如果指针不同但文件系统类型相同，也返回
                // (处理 bind mount 等情况)
                if inner_fs.name() == fs.name() {
                    return Some(record.fs.clone());
                }
            }
        }
        None
    }
}

impl Debug for MountList {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let inner = self.inner.read();
        f.debug_map().entries(inner.mounts.iter()).finish()
    }
}

/// 判断给定的inode是否为其所在文件系统的根inode
///
/// ## 返回值
///
/// - `true`: 是根inode
/// - `false`: 不是根inode或者传入的inode不是MountFSInode类型，或者调用inode的metadata方法时报错了。
pub fn is_mountpoint_root(inode: &Arc<dyn IndexNode>) -> bool {
    let mnt_inode = inode.as_any_ref().downcast_ref::<MountFSInode>();
    if let Some(mnt) = mnt_inode {
        return mnt.is_mountpoint_root().unwrap_or(false);
    }

    return false;
}

/// # do_mount_mkdir - 在指定挂载点创建目录并挂载文件系统
///
/// 在指定的挂载点创建一个目录，并将其挂载到文件系统中。如果挂载点已经存在，并且不是空的，
/// 则会返回错误。成功时，会返回一个新的挂载文件系统的引用。
///
/// ## 参数
///
/// - `fs`: FileSystem - 文件系统的引用，用于创建和挂载目录。
/// - `mount_point`: &str - 挂载点路径，用于创建和挂载目录。
///
/// ## 返回值
///
/// - `Ok(Arc<MountFS>)`: 成功挂载文件系统后，返回挂载文件系统的共享引用。
/// - `Err(SystemError)`: 挂载失败时，返回系统错误。
pub fn do_mount_mkdir(
    fs: Arc<dyn FileSystem>,
    mount_point: &str,
    mount_flags: MountFlags,
) -> Result<Arc<MountFS>, SystemError> {
    let inode = do_mkdir_at(
        AtFlags::AT_FDCWD.bits(),
        mount_point,
        InodeMode::from_bits_truncate(0o755),
    )?;
    let result = ProcessManager::current_mntns().get_mount_point(mount_point);
    if let Some((_, rest, _fs)) = result {
        if rest.is_empty() {
            return Err(SystemError::EBUSY);
        }
    }
    return inode.mount(fs, mount_flags);
}
