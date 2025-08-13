use core::{
    any::Any,
    fmt::Debug,
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

use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::{
        page_cache::PageCache,
        vfs::{fcntl::AtFlags, vcore::do_mkdir_at},
    },
    libs::{
        casting::DowncastArc,
        lazy_init::Lazy,
        rwlock::RwLock,
        spinlock::{SpinLock, SpinLockGuard},
    },
    mm::{fault::PageFaultMessage, VmFaultReason},
    process::{
        namespace::mnt::{MntNamespace, MountPropagation},
        ProcessManager,
    },
};

use super::{
    file::FileMode, syscall::ModeType, utils::DName, FilePrivateData, FileSystem, FileType,
    IndexNode, InodeId, Magic, PollableInode, SuperBlock,
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

static MOUNT_ID_ALLOCATOR: SpinLock<IdAllocator> =
    SpinLock::new(IdAllocator::new(0, usize::MAX).unwrap());

impl MountId {
    fn alloc() -> Self {
        let id = MOUNT_ID_ALLOCATOR.lock().alloc().unwrap();

        MountId(id)
    }

    unsafe fn free(&mut self) {
        MOUNT_ID_ALLOCATOR.lock().free(self.0);
    }
}

const MOUNTFS_BLOCK_SIZE: u64 = 512;
const MOUNTFS_MAX_NAMELEN: u64 = 64;
/// @brief 挂载文件系统
/// 挂载文件系统的时候，套了MountFS这一层，以实现文件系统的递归挂载
pub struct MountFS {
    // MountFS内部的文件系统
    inner_filesystem: Arc<dyn FileSystem>,
    /// 用来存储InodeID->挂载点的MountFS的B树
    mountpoints: SpinLock<BTreeMap<InodeId, Arc<MountFS>>>,
    /// 当前文件系统挂载到的那个挂载点的Inode
    self_mountpoint: RwLock<Option<Arc<MountFSInode>>>,
    /// 指向当前MountFS的弱引用
    self_ref: Weak<MountFS>,

    namespace: Lazy<Weak<MntNamespace>>,
    propagation: Arc<MountPropagation>,
    mount_id: MountId,

    mount_flags: MountFlags,
}

impl Debug for MountFS {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MountFS")
            .field("mount_id", &self.mount_id)
            .finish()
    }
}

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
            mountpoints: SpinLock::new(BTreeMap::new()),
            self_mountpoint: RwLock::new(self_mountpoint),
            self_ref: self_ref.clone(),
            namespace: Lazy::new(),
            propagation,
            mount_id: MountId::alloc(),
            mount_flags,
        });

        if let Some(mnt_ns) = mnt_ns {
            result.set_namespace(Arc::downgrade(mnt_ns));
        }

        result
    }

    pub fn mount_flags(&self) -> MountFlags {
        self.mount_flags
    }

    pub fn propagation(&self) -> Arc<MountPropagation> {
        self.propagation.clone()
    }

    pub fn set_namespace(&self, namespace: Weak<MntNamespace>) {
        self.namespace.init(namespace);
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
        let r = self
            .self_mountpoint()
            .ok_or(SystemError::EINVAL)?
            .do_umount();

        self.self_mountpoint.write().take();

        return r;
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
                    let inner_inode = inode.parent()?;
                    return Ok(Arc::new_cyclic(|self_ref| MountFSInode {
                        inner_inode,
                        mount_fs: self.mount_fs.clone(),
                        self_ref: self_ref.clone(),
                    }));
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
        return self
            .mount_fs
            .mountpoints
            .lock()
            .remove(&self.inner_inode.metadata()?.inode_id)
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
        let root_inode = ProcessManager::current_mntns().root_inode();
        let inode_id = root_inode.metadata()?.inode_id;
        while current.metadata()?.inode_id != inode_id {
            let name = current.dname()?;
            path_parts.push(name.0);

            // 防循环检查：如果路径深度超过1024，抛出警告
            if path_parts.len() > 1024 {
                #[inline(never)]
                fn __log_warn(root: usize, cur: usize) {
                    log::warn!(
                        "Path depth exceeds 1024, possible infinite loop. root: {}, cur: {}",
                        root,
                        cur
                    );
                }
                __log_warn(inode_id.data(), current.metadata().unwrap().inode_id.data());
                return Err(SystemError::ELOOP);
            }

            current = current.do_parent()?;
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

        Ok(absolute_path)
    }
}

impl IndexNode for MountFSInode {
    fn open(
        &self,
        data: SpinLockGuard<FilePrivateData>,
        mode: &FileMode,
    ) -> Result<(), SystemError> {
        return self.inner_inode.open(data, mode);
    }

    fn close(&self, data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        self.inner_inode.close(data)
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: ModeType,
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
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        return self.inner_inode.read_at(offset, len, buf, data);
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        return self.inner_inode.write_at(offset, len, buf, data);
    }

    fn read_direct(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.inner_inode.read_direct(offset, len, buf, data)
    }

    fn write_direct(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: SpinLockGuard<FilePrivateData>,
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
        return self.inner_inode.metadata();
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
        mode: ModeType,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let inner_inode = self.inner_inode.create(name, file_type, mode)?;
        return Ok(Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode,
            mount_fs: self.mount_fs.clone(),
            self_ref: self_ref.clone(),
        }));
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        return self.inner_inode.link(name, other);
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
    ) -> Result<(), SystemError> {
        return self.inner_inode.move_to(old_name, target, new_name);
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

        if self.is_mountpoint_root()? {
            return Err(SystemError::EBUSY);
        }

        // 若已有挂载系统，保证MountFS只包一层
        let to_mount_fs = fs
            .clone()
            .downcast_arc::<MountFS>()
            .map(|it| it.inner_filesystem())
            .unwrap_or(fs);

        let new_mount_fs = MountFS::new(
            to_mount_fs,
            Some(self.self_ref.upgrade().unwrap()),
            MountPropagation::new_private(), // 暂时不支持传播，后续会补充完善挂载传播性
            Some(&ProcessManager::current_mntns()),
            mount_flags,
        );

        self.mount_fs
            .mountpoints
            .lock()
            .insert(metadata.inode_id, new_mount_fs.clone());

        // todo: 这里也许不应该存储路径到MountList，而是应该存储inode的引用。因为同一个inner inode的路径在不同的mntns中可能是不一样的。
        let mount_path = self.absolute_path();
        let mount_path = Arc::new(MountPath::from(mount_path?));
        ProcessManager::current_mntns().add_mount(mount_path, new_mount_fs.clone())?;

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
            .mountpoints
            .lock()
            .insert(metadata.inode_id, new_mount_fs.clone());
        // 更新当前挂载点的self_mountpoint
        new_mount_fs
            .self_mountpoint
            .write()
            .replace(self.self_ref.upgrade().unwrap());
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
        mode: ModeType,
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
        "mountfs"
    }
    fn super_block(&self) -> SuperBlock {
        SuperBlock::new(Magic::MOUNT_MAGIC, MOUNTFS_BLOCK_SIZE, MOUNTFS_MAX_NAMELEN)
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
    mounts: RwLock<HashMap<Arc<MountPath>, Arc<MountFS>>>,
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
            mounts: RwLock::new(HashMap::new()),
        })
    }

    /// Inserts a filesystem mount point into the mount list.
    ///
    /// This function adds a new filesystem mount point to the mount list. If a mount point
    /// already exists at the specified path, it will be updated with the new filesystem.
    ///
    /// # Thread Safety
    /// This function is thread-safe as it uses a RwLock to ensure safe concurrent access.
    ///
    /// # Arguments
    /// * `path` - The mount path where the filesystem will be mounted
    /// * `fs` - The filesystem instance to be mounted at the specified path
    #[inline]
    pub fn insert(&self, path: Arc<MountPath>, fs: Arc<MountFS>) {
        self.mounts.write().insert(path, fs);
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
    #[inline]
    #[allow(dead_code)]
    pub fn get_mount_point<T: AsRef<str>>(
        &self,
        path: T,
    ) -> Option<(Arc<MountPath>, String, Arc<MountFS>)> {
        self.mounts
            .upgradeable_read()
            .iter()
            .filter_map(|(key, fs)| {
                let strkey = key.as_str();
                if let Some(rest) = path.as_ref().strip_prefix(strkey) {
                    return Some((key.clone(), rest.to_string(), fs.clone()));
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
    #[inline]
    pub fn remove<T: Into<MountPath>>(&self, path: T) -> Option<Arc<MountFS>> {
        self.mounts.write().remove(&path.into())
    }

    /// # clone_inner - 克隆内部挂载点列表
    pub fn clone_inner(&self) -> HashMap<Arc<MountPath>, Arc<MountFS>> {
        self.mounts.read().clone()
    }
}

impl Debug for MountList {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_map().entries(self.mounts.read().iter()).finish()
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
        FileMode::from_bits_truncate(0o755),
    )?;
    let result = ProcessManager::current_mntns().get_mount_point(mount_point);
    if let Some((_, rest, _fs)) = result {
        if rest.is_empty() {
            return Err(SystemError::EBUSY);
        }
    }
    return inode.mount(fs, mount_flags);
}
