use super::{
    file::FileFlags, utils::DName, FilePrivateData, FileSystem, FileType, IndexNode, InodeId,
    InodeMode, PollableInode, SuperBlock,
};
use crate::{
    driver::base::device::device_number::{DeviceNumber, Major},
    filesystem::{
        page_cache::list_page_caches,
        page_cache::PageCache,
        vfs::{fcntl::AtFlags, syscall::RenameFlags, vcore::do_mkdir_at},
    },
    libs::{
        casting::DowncastArc,
        errseq::{ErrSeq, ErrSeqValue},
        mutex::{Mutex, MutexGuard},
        rwsem::RwSem,
        spinlock::SpinLock,
    },
    mm::{fault::PageFaultMessage, VmFaultReason, VmFlags},
    process::{
        namespace::{
            mnt::MntNamespace,
            propagation::{
                inherit_bind_mount_propagation, propagate_mount, propagate_umount, register_peer,
                register_slave_with_master, unregister_peer, MountPropagation,
            },
        },
        ProcessManager,
    },
};
use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    fmt::Debug,
    hash::Hash,
    mem,
    sync::atomic::{compiler_fence, AtomicUsize, Ordering},
};
use hashbrown::HashMap;
use ida::IdAllocator;
use lazy_static::lazy_static;
use system_error::SystemError;

mod subtree_move;

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

        const SB_SETTABLE_MASK = MountFlags::RDONLY.bits()
            | MountFlags::SYNCHRONOUS.bits()
            | MountFlags::MANDLOCK.bits()
            | MountFlags::DIRSYNC.bits()
            | MountFlags::SILENT.bits()
            | MountFlags::POSIXACL.bits()
            | MountFlags::I_VERSION.bits()
            | MountFlags::LAZYTIME.bits();

        /// Old magic mount flag and mask
        const MGC_VAL = 0xC0ED0000; // Magic value for mount flags
        const MGC_MASK = 0xFFFF0000; // Mask for magic mount flags

        /// Set of mount flags that userspace can modify via MS_REMOUNT.
        const MNT_USER_SETTABLE_MASK = MountFlags::RDONLY.bits()
            | MountFlags::NOSUID.bits()
            | MountFlags::NODEV.bits()
            | MountFlags::NOEXEC.bits()
            | MountFlags::NOATIME.bits()
            | MountFlags::NODIRATIME.bits()
            | MountFlags::RELATIME.bits()
            | MountFlags::NOSYMFOLLOW.bits();

        const MNT_ATIME_MASK = MountFlags::NOATIME.bits()
            | MountFlags::NODIRATIME.bits()
            | MountFlags::RELATIME.bits();
    }
}

impl MountFlags {
    /// `ro` or `rw` token for proc mount options.
    pub fn proc_rw_token(&self) -> &'static str {
        if self.contains(MountFlags::RDONLY) {
            "ro"
        } else {
            "rw"
        }
    }

    /// Per-mount options excluding rw and super-block flags.
    pub fn proc_per_mount_options(&self) -> String {
        let mut options = Vec::new();

        if self.contains(MountFlags::NOSUID) {
            options.push("nosuid");
        }
        if self.contains(MountFlags::NODEV) {
            options.push("nodev");
        }
        if self.contains(MountFlags::NOEXEC) {
            options.push("noexec");
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

        options.join(",")
    }

    /// Super-block options excluding rw and per-mount flags.
    pub fn proc_super_block_options(&self) -> String {
        let mut options = Vec::new();

        if self.contains(MountFlags::SYNCHRONOUS) {
            options.push("sync");
        }
        if self.contains(MountFlags::MANDLOCK) {
            options.push("mand");
        }
        if self.contains(MountFlags::DIRSYNC) {
            options.push("dirsync");
        }
        if self.contains(MountFlags::LAZYTIME) {
            options.push("lazytime");
        }

        options.join(",")
    }

    /// Convert mount flags to a comma-separated string representation
    ///
    /// This function converts MountFlags to a string format similar to /proc/mounts,
    /// such as "rw,nosuid,nodev,noexec,relatime".
    #[inline(never)]
    pub fn options_string(&self) -> String {
        let mut options = self.proc_rw_token().to_string();
        append_comma_options(&mut options, self.proc_per_mount_options());
        append_comma_options(&mut options, self.proc_super_block_options());
        options
    }
}

pub(crate) fn append_comma_options(base: &mut String, extra: String) {
    if extra.is_empty() {
        return;
    }
    if !base.is_empty() {
        base.push(',');
    }
    base.push_str(&extra);
}

// MountId type
int_like!(MountId, usize);

static MOUNT_ID_ALLOCATOR: Mutex<IdAllocator> =
    Mutex::new(IdAllocator::new(0, usize::MAX).unwrap());

lazy_static! {
    static ref MOUNTED_SUPERBLOCKS: SpinLock<Vec<Weak<MountFS>>> = SpinLock::new(Vec::new());
}

impl MountId {
    fn alloc() -> Self {
        let id = MOUNT_ID_ALLOCATOR.lock().alloc().unwrap();

        MountId(id)
    }

    unsafe fn free(&mut self) {
        MOUNT_ID_ALLOCATOR.lock().free(self.0);
    }
}

/// @brief Mount filesystem
/// When mounting a filesystem, a MountFS wrapper layer is applied to support recursive mounting.
pub struct MountFS {
    // The inner filesystem wrapped by MountFS
    inner_filesystem: Arc<dyn FileSystem>,
    /// The root inode exposed by this mount. For bind-mount subdirectories, this is not the global root of the underlying filesystem.
    root_inner_inode: Arc<dyn IndexNode>,
    /// B-tree mapping InodeId -> MountFS at that mount point
    mountpoints: Mutex<BTreeMap<InodeId, Arc<MountFS>>>,
    /// The inode of the mount point where this filesystem is mounted
    self_mountpoint: RwSem<Option<Arc<MountFSInode>>>,
    /// Weak reference to this MountFS
    self_ref: Weak<MountFS>,

    namespace: RwSem<Option<Weak<MntNamespace>>>,
    propagation: Arc<MountPropagation>,
    mount_id: MountId,

    mount_flags: RwSem<MountFlags>,
    super_block_state: Arc<SuperBlockState>,
    mount_source: RwSem<Option<String>>,
}

#[derive(Debug)]
pub struct SuperBlockState {
    flags: RwSem<MountFlags>,
    write_count: AtomicUsize,
    wb_error: ErrSeq,
    umount_lock: RwSem<()>,
}

struct MountStateInit {
    super_block_state: Arc<SuperBlockState>,
    mount_source: Option<String>,
}

impl SuperBlockState {
    pub fn new(flags: MountFlags) -> Self {
        Self {
            flags: RwSem::new(flags & MountFlags::SB_SETTABLE_MASK),
            write_count: AtomicUsize::new(0),
            wb_error: ErrSeq::new(),
            umount_lock: RwSem::new(()),
        }
    }

    pub fn flags(&self) -> MountFlags {
        *self.flags.read()
    }

    pub fn set_flags(&self, flags: MountFlags) {
        *self.flags.write() = flags & MountFlags::SB_SETTABLE_MASK;
    }

    pub fn inc_write_count(&self) {
        self.write_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_write_count(&self) {
        self.write_count.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn has_writers(&self) -> bool {
        self.write_count.load(Ordering::Acquire) != 0
    }

    pub fn sample_wb_error(&self) -> ErrSeqValue {
        self.wb_error.sample()
    }

    pub fn check_and_advance_wb_error(&self, since: &mut ErrSeqValue) -> Option<SystemError> {
        self.wb_error.check_and_advance(since)
    }

    pub fn record_wb_error(&self, error: SystemError) {
        self.wb_error.set(error);
    }

    pub fn umount_read(&self) -> crate::libs::rwsem::RwSemReadGuard<'_, ()> {
        self.umount_lock.read()
    }

    pub fn try_umount_read(&self) -> Option<crate::libs::rwsem::RwSemReadGuard<'_, ()>> {
        self.umount_lock.try_read()
    }

    pub fn umount_write(&self) -> crate::libs::rwsem::RwSemWriteGuard<'_, ()> {
        self.umount_lock.write()
    }
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

/// @brief The Index Node of MountFS. Note that this IndexNode is merely an intermediary layer.
/// Its purpose is to connect the concrete filesystem's Inode with the mount mechanism.
#[derive(Debug)]
#[cast_to([sync] IndexNode)]
pub struct MountFSInode {
    /// The concrete filesystem's Inode corresponding to this mount point
    inner_inode: Arc<dyn IndexNode>,
    /// The MountFS this Inode belongs to
    mount_fs: Arc<MountFS>,
    /// Weak reference to self
    self_ref: Weak<MountFSInode>,
}

impl MountFS {
    pub fn new(
        inner_filesystem: Arc<dyn FileSystem>,
        root_inner_inode: Option<Arc<dyn IndexNode>>,
        self_mountpoint: Option<Arc<MountFSInode>>,
        propagation: Arc<MountPropagation>,
        mnt_ns: Option<&Arc<MntNamespace>>,
        mount_flags: MountFlags,
        mount_source: Option<String>,
    ) -> Arc<Self> {
        Self::new_with_super_block_state(
            inner_filesystem,
            root_inner_inode,
            self_mountpoint,
            propagation,
            mnt_ns,
            mount_flags,
            MountStateInit {
                super_block_state: Arc::new(SuperBlockState::new(mount_flags)),
                mount_source,
            },
        )
    }

    fn new_with_super_block_state(
        inner_filesystem: Arc<dyn FileSystem>,
        root_inner_inode: Option<Arc<dyn IndexNode>>,
        self_mountpoint: Option<Arc<MountFSInode>>,
        propagation: Arc<MountPropagation>,
        mnt_ns: Option<&Arc<MntNamespace>>,
        mount_flags: MountFlags,
        state_init: MountStateInit,
    ) -> Arc<Self> {
        let root_inner_inode = root_inner_inode.unwrap_or_else(|| inner_filesystem.root_inode());
        let result = Arc::new_cyclic(|self_ref| MountFS {
            inner_filesystem,
            root_inner_inode,
            mountpoints: Mutex::new(BTreeMap::new()),
            self_mountpoint: RwSem::new(self_mountpoint),
            self_ref: self_ref.clone(),
            namespace: RwSem::new(None),
            propagation,
            mount_id: MountId::alloc(),
            mount_flags: RwSem::new(mount_flags),
            super_block_state: state_init.super_block_state,
            mount_source: RwSem::new(state_init.mount_source),
        });

        if let Some(mnt_ns) = mnt_ns {
            result.set_namespace(Arc::downgrade(mnt_ns));
        }

        register_mounted_superblock(&result);
        result
    }

    pub fn deepcopy(&self, self_mountpoint: Option<Arc<MountFSInode>>) -> Arc<Self> {
        // Clone propagation state for the new mount copy
        let new_propagation = self.propagation.clone_for_copy();
        let mount_source = self.mount_source();

        let mountfs = Arc::new_cyclic(|self_ref| MountFS {
            inner_filesystem: self.inner_filesystem.clone(),
            root_inner_inode: self.root_inner_inode.clone(),
            mountpoints: Mutex::new(BTreeMap::new()),
            self_mountpoint: RwSem::new(self_mountpoint),
            self_ref: self_ref.clone(),
            namespace: RwSem::new(None),
            propagation: new_propagation,
            mount_id: MountId::alloc(),
            mount_flags: RwSem::new(self.mount_flags()),
            super_block_state: self.super_block_state.clone(),
            mount_source: RwSem::new(mount_source),
        });

        register_mounted_superblock(&mountfs);
        return mountfs;
    }

    pub fn mount_flags(&self) -> MountFlags {
        *self.mount_flags.read()
    }

    pub fn super_block_flags(&self) -> MountFlags {
        self.super_block_state.flags()
    }

    pub fn set_super_block_flags(&self, flags: MountFlags) {
        self.super_block_state.set_flags(flags);
    }

    pub fn combined_flags(&self) -> MountFlags {
        self.mount_flags() | self.super_block_flags()
    }

    pub fn is_readonly(&self) -> bool {
        self.combined_flags().contains(MountFlags::RDONLY)
    }

    pub fn is_sb_readonly(&self) -> bool {
        self.super_block_flags().contains(MountFlags::RDONLY)
    }

    pub fn has_writers(&self) -> bool {
        self.super_block_state.has_writers()
    }

    pub fn inc_write_count(&self) {
        self.super_block_state.inc_write_count();
    }

    pub fn dec_write_count(&self) {
        self.super_block_state.dec_write_count();
    }

    pub fn super_block_state(&self) -> Arc<SuperBlockState> {
        self.super_block_state.clone()
    }

    pub fn set_mount_flags(&self, mount_flags: MountFlags) {
        *self.mount_flags.write() = mount_flags;
    }

    pub fn update_mount_flags(&self, update: impl FnOnce(&mut MountFlags)) {
        let mut mount_flags = self.mount_flags.write();
        update(&mut mount_flags);
    }

    pub fn add_mount(&self, inode_id: InodeId, mount_fs: Arc<MountFS>) -> Result<(), SystemError> {
        let mut mountpoints = self.mountpoints.lock();
        if mountpoints.contains_key(&inode_id) {
            return Err(SystemError::EEXIST);
        }
        mountpoints.insert(inode_id, mount_fs);
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
        *self.namespace.write() = Some(namespace);
    }

    pub fn namespace(&self) -> Option<Arc<MntNamespace>> {
        self.namespace.read().as_ref().and_then(|ns| ns.upgrade())
    }

    pub fn clear_namespace(&self) {
        *self.namespace.write() = None;
    }

    /// check_mnt(): Check whether the current MountFS belongs to the specified mount namespace.
    pub fn is_belongs_to_mntns(&self, mntns: &Arc<MntNamespace>) -> bool {
        self.namespace().is_some_and(|ns| Arc::ptr_eq(&ns, mntns))
    }

    pub fn fs_type(&self) -> &str {
        self.inner_filesystem.name()
    }

    pub fn mount_source(&self) -> Option<String> {
        self.mount_source.read().clone()
    }

    pub fn set_mount_source(&self, mount_source: Option<String>) {
        *self.mount_source.write() = mount_source;
    }

    #[inline(never)]
    pub fn self_mountpoint(&self) -> Option<Arc<MountFSInode>> {
        self.self_mountpoint.read().as_ref().cloned()
    }

    pub fn parent_mount(&self) -> Option<Arc<MountFS>> {
        self.self_mountpoint().map(|inode| inode.mount_fs.clone())
    }

    pub fn set_self_mountpoint(&self, mountpoint: Option<Arc<MountFSInode>>) {
        *self.self_mountpoint.write() = mountpoint;
    }

    /// @brief Wrap a MountFS object in an Arc pointer.
    /// The main purpose of this function is to initialize the self-referencing Weak pointer within the MountFS object.
    /// This function should only be called in constructors.
    #[allow(dead_code)]
    #[deprecated]
    fn wrap(self) -> Arc<Self> {
        // Create Arc pointer
        let mount_fs: Arc<MountFS> = Arc::new(self);
        // Create weak pointer
        let weak: Weak<MountFS> = Arc::downgrade(&mount_fs);

        // Convert the Arc pointer to a raw pointer and assign to its internal self_ref field
        let ptr: *mut MountFS = mount_fs.as_ref() as *const Self as *mut Self;
        unsafe {
            (*ptr).self_ref = weak;
            // Return the initialized MountFS object
            return mount_fs;
        }
    }

    /// @brief Get the root inode of the filesystem at this mount point
    pub fn mountpoint_root_inode(&self) -> Arc<MountFSInode> {
        return Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode: self.root_inner_inode.clone(),
            mount_fs: self.self_ref.upgrade().unwrap(),
            self_ref: self_ref.clone(),
        });
    }

    pub fn inner_filesystem(&self) -> Arc<dyn FileSystem> {
        return self.inner_filesystem.clone();
    }

    pub fn root_inner_inode(&self) -> Arc<dyn IndexNode> {
        self.root_inner_inode.clone()
    }

    pub fn self_ref(&self) -> Arc<Self> {
        self.self_ref.upgrade().unwrap()
    }

    /// Unmount the filesystem.
    ///
    /// Modeled after Linux `deactivate_super()` + `generic_shutdown_super()`:
    /// take the superblock write lock first, then run the sync body without
    /// trying to recursively acquire `umount_read`. All propagation clones share
    /// the same `super_block_state` (via `Arc::clone` in `deepcopy`), so a single
    /// top-level sync covers every peer.
    ///
    /// # Errors
    /// Returns `EINVAL` if this is the root filesystem.
    pub fn umount(&self) -> Result<Arc<MountFS>, SystemError> {
        let mountpoint = self.self_mountpoint().ok_or(SystemError::EINVAL)?;

        // Phase 1: Exclusive lock — excludes concurrent sync/IO during teardown.
        let sb_state = self.super_block_state();
        let _umount_guard = sb_state.umount_write();

        // Phase 2: Sync while the superblock lock is already held.
        // Errors during pre-umount sync are non-fatal (warn only).
        if let Err(e) = self.sync_filesystem_locked() {
            log::warn!("umount: pre-sync failed: {:?}, proceeding with umount", e);
        }

        // Phase 3: Detach and propagate (no syncing under the lock).
        let result = mountpoint.do_umount();

        if result.is_ok() {
            // Clear self_mountpoint to drop the back-reference to the old parent mountpoint.
            self.self_mountpoint.write().take();
            self.inner_filesystem.on_umount();
            self.clear_namespace();
        }

        return result;
    }

    /// Recursively unmount a mount and all its child mounts, removing them from the namespace's mount_list.
    ///
    /// Used for atomic rollback on recursive bind mount failure, ensuring all-or-nothing semantics.
    pub fn umount_tree(root: &Arc<MountFS>) {
        let mntns = ProcessManager::current_mntns();

        // 1. DFS collect all descendant MountFS
        let mut all_descendants: Vec<Arc<MountFS>> = Vec::new();
        let mut stack: Vec<Arc<MountFS>> = Vec::new();

        for (_, child_mfs) in root.mountpoints().iter() {
            stack.push(child_mfs.clone());
        }

        while let Some(mfs) = stack.pop() {
            for (_, child_mfs) in mfs.mountpoints().iter() {
                stack.push(child_mfs.clone());
            }
            all_descendants.push(mfs);
        }

        // 2. Process in reverse order (deepest child mounts first), ensuring child mounts are cleaned up before parent mounts
        all_descendants.reverse();

        for child_mfs in &all_descendants {
            if let Some(path) = mntns.mount_list().get_mount_path_by_mountfs(child_mfs) {
                mntns.remove_mount(path.as_str());
            }
            let _ = child_mfs.umount();
        }

        // 3. Finally unmount the root mount itself
        if let Some(path) = mntns.mount_list().get_mount_path_by_mountfs(root) {
            mntns.remove_mount(path.as_str());
        }
        let _ = root.umount();
    }

    /// Corresponds to Linux `sync_inodes_sb()`: write back all dirty page caches under the specified mount.
    /// DragonOS has no per-sb dirty inode list, so it iterates the global `PAGECACHE_REGISTRY` to find matches.
    fn sync_inodes_of_mount(&self) -> Result<(), SystemError> {
        let inner_fs = self.inner_filesystem();
        let caches = list_page_caches();
        let mut last_err = Ok(());
        for page_cache in caches {
            // Fast-skip page caches with no dirty pages, avoiding unnecessary inode.upgrade() and Arc::ptr_eq.
            if !page_cache.has_dirty_pages() {
                continue;
            }

            let belongs = page_cache
                .inode()
                .and_then(|weak| weak.upgrade())
                .is_some_and(|inode| Arc::ptr_eq(&inode.fs(), &inner_fs));

            if belongs {
                if let Err(e) = page_cache.manager().sync() {
                    log::warn!("sync_inodes_of_mount: page cache sync failed: {:?}", e);
                    self.record_wb_error(e.clone());
                    last_err = Err(e);
                }
            }
        }
        last_err
    }

    pub fn sync_inodes_with_umount_read(&self) -> Result<(), SystemError> {
        let sb_state = self.super_block_state();
        let _umount_guard = sb_state.umount_read();

        if self.is_sb_readonly() {
            return Ok(());
        }

        self.sync_inodes_of_mount()
    }

    pub fn sync_fs_with_umount_read(&self, wait: bool) -> Result<(), SystemError> {
        let sb_state = self.super_block_state();
        let _umount_guard = sb_state.umount_read();

        if self.is_sb_readonly() {
            return Ok(());
        }

        if let Err(e) = self.sync_fs(wait) {
            self.record_wb_error(e.clone());
            return Err(e);
        }

        Ok(())
    }

    pub fn try_sync_fs_with_umount_read(&self, wait: bool) -> Result<bool, SystemError> {
        let sb_state = self.super_block_state();
        let Some(_umount_guard) = sb_state.try_umount_read() else {
            return Ok(false);
        };

        if self.is_sb_readonly() {
            return Ok(true);
        }

        if let Err(e) = self.sync_fs(wait) {
            self.record_wb_error(e.clone());
            return Err(e);
        }

        Ok(true)
    }

    pub fn sync_blockdev_with_umount_read(&self, wait: bool) -> Result<(), SystemError> {
        let sb_state = self.super_block_state();
        let _umount_guard = sb_state.umount_read();

        if self.is_sb_readonly() {
            return Ok(());
        }

        if let Err(e) = self.sync_blockdev(wait) {
            self.record_wb_error(e.clone());
            return Err(e);
        }

        Ok(())
    }

    /// Flush all pending filesystem metadata and cached file data to the underlying filesystem.
    ///
    /// Modeled after Linux `sync_filesystem(sb)`: the caller must already hold
    /// this mount's superblock `umount_lock`, either for read (`syncfs`) or write
    /// (`umount`).
    fn sync_filesystem_locked(&self) -> Result<(), SystemError> {
        if self.is_sb_readonly() {
            return Ok(());
        }

        // writeback_inodes_sb(sb) — void
        let mut last_err = self.sync_inodes_of_mount();
        // sync_fs(sb, 0)
        if let Err(e) = self.sync_fs(false) {
            self.record_wb_error(e.clone());
            return Err(e);
        }

        if let Err(e) = self.sync_blockdev(false) {
            self.record_wb_error(e.clone());
            return Err(e);
        }

        // sync_inodes_sb(sb) — void
        if let Err(e) = self.sync_inodes_of_mount() {
            last_err = Err(e);
        }
        // sync_fs(sb, 1)
        if let Err(e) = self.sync_fs(true) {
            self.record_wb_error(e.clone());
            return Err(e);
        }

        if let Err(e) = self.sync_blockdev(true) {
            self.record_wb_error(e.clone());
            return Err(e);
        }

        last_err
    }

    /// Public read-locked wrapper for callers that do not already hold the
    /// superblock `umount_lock`.
    pub fn sync_filesystem(&self) -> Result<(), SystemError> {
        let sb_state = self.super_block_state();
        let _umount_guard = sb_state.umount_read();

        self.sync_filesystem_locked()
    }

    pub fn sync_blockdev(&self, _wait: bool) -> Result<(), SystemError> {
        Ok(())
    }

    pub fn record_wb_error(&self, error: SystemError) {
        self.super_block_state.record_wb_error(error);
    }

    pub fn sample_wb_error(&self) -> ErrSeqValue {
        self.super_block_state.sample_wb_error()
    }

    pub fn check_and_advance_wb_error(&self, since: &mut ErrSeqValue) -> Option<SystemError> {
        self.super_block_state.check_and_advance_wb_error(since)
    }
}

fn register_mounted_superblock(mount: &Arc<MountFS>) {
    MOUNTED_SUPERBLOCKS
        .lock_irqsave()
        .push(Arc::downgrade(mount));
}

pub fn list_unique_mounted_superblocks() -> Vec<Arc<MountFS>> {
    let mut guard = MOUNTED_SUPERBLOCKS.lock_irqsave();
    let mut mounts: Vec<Arc<MountFS>> = Vec::new();
    guard.retain(|weak| {
        if let Some(mount) = weak.upgrade() {
            let state = mount.super_block_state();
            if !mounts
                .iter()
                .any(|existing| Arc::ptr_eq(&existing.super_block_state(), &state))
            {
                mounts.push(mount);
            }
            true
        } else {
            false
        }
    });
    mounts
}

pub fn record_writeback_error_for_fs(inner_fs: &Arc<dyn FileSystem>, error: SystemError) {
    for mount in list_unique_mounted_superblocks() {
        if Arc::ptr_eq(&mount.inner_filesystem(), inner_fs) {
            mount.record_wb_error(error.clone());
        }
    }
}

impl Drop for MountFS {
    fn drop(&mut self) {
        // Release MountId
        unsafe {
            self.mount_id.free();
        }
    }
}

impl MountFSInode {
    #[inline]
    fn ensure_mount_writable(&self) -> Result<(), SystemError> {
        if self.mount_fs.is_readonly() {
            return Err(SystemError::EROFS);
        }
        Ok(())
    }

    pub(crate) fn mount_subtree(
        &self,
        inner_fs: Arc<dyn FileSystem>,
        root_inner_inode: Arc<dyn IndexNode>,
        mount_flags: MountFlags,
    ) -> Result<Arc<MountFS>, SystemError> {
        self.mount_subtree_with_state(inner_fs, root_inner_inode, mount_flags, None, None, None)
    }

    pub(crate) fn mount_subtree_with_state(
        &self,
        inner_fs: Arc<dyn FileSystem>,
        root_inner_inode: Arc<dyn IndexNode>,
        mount_flags: MountFlags,
        super_block_state: Option<Arc<SuperBlockState>>,
        bind_source: Option<&Arc<MountFS>>,
        mount_path_override: Option<Arc<MountPath>>,
    ) -> Result<Arc<MountFS>, SystemError> {
        // Linux do_add_mount: the parent mount point must belong to the current mount namespace.
        let current_mntns = ProcessManager::current_mntns();
        if !self.mount_fs.is_belongs_to_mntns(&current_mntns) {
            return Err(SystemError::EINVAL);
        }

        let metadata = self.inner_inode.metadata()?;
        let root_metadata = root_inner_inode.metadata()?;
        let is_dir = metadata.file_type == FileType::Dir;
        let root_is_dir = root_metadata.file_type == FileType::Dir;
        if is_dir != root_is_dir {
            return Err(SystemError::ENOTDIR);
        }

        let parent_propagation = self.mount_fs.propagation();
        let new_propagation = if parent_propagation.is_shared() {
            MountPropagation::new_shared()
        } else {
            MountPropagation::new_private()
        };

        let super_block_state =
            super_block_state.unwrap_or_else(|| Arc::new(SuperBlockState::new(mount_flags)));
        let new_mount_fs = MountFS::new_with_super_block_state(
            inner_fs,
            Some(root_inner_inode),
            Some(self.self_ref.upgrade().unwrap()),
            new_propagation,
            Some(&ProcessManager::current_mntns()),
            mount_flags,
            MountStateInit {
                super_block_state,
                mount_source: None,
            },
        );

        // 调用者可以传入已经按 VFS 解析过的命名空间路径，避免 FUSE/virtiofs 等
        // 文件系统的 inner inode absolute_path() 返回合成路径或无法反查真实挂载点。
        let mount_path = match mount_path_override {
            Some(p) => p,
            None => Arc::new(MountPath::from(self.absolute_path()?)),
        };

        if let Some(source) = bind_source {
            inherit_bind_mount_propagation(source, &new_mount_fs);
        }

        self.mount_fs
            .add_mount(metadata.inode_id, new_mount_fs.clone())?;

        ProcessManager::current_mntns().add_mount(
            Some(metadata.inode_id),
            mount_path.clone(),
            new_mount_fs.clone(),
        )?;

        if parent_propagation.is_shared() {
            if let Err(e) = propagate_mount(
                &self.mount_fs,
                metadata.inode_id,
                &new_mount_fs,
                &mount_path,
            ) {
                log::warn!("mount: propagation failed: {:?}", e);
            }
        }

        let new_mount_prop = new_mount_fs.propagation();
        if new_mount_prop.is_shared() {
            register_peer(new_mount_prop.peer_group_id(), &new_mount_fs);
        }
        if bind_source.is_some() && new_mount_prop.is_slave() {
            register_slave_with_master(&new_mount_fs);
        }

        Ok(new_mount_fs)
    }

    /// Return the underlying inode wrapped by the mount wrapper.
    #[inline]
    pub(crate) fn underlying_inode(&self) -> Arc<dyn IndexNode> {
        self.inner_inode.clone()
    }

    /// @brief Wrap a MountFSInode object in an Arc pointer.
    /// The main purpose of this function is to initialize the self-referencing Weak pointer within the MountFSInode object.
    /// This function should only be called in constructors.
    #[allow(dead_code)]
    #[deprecated]
    fn wrap(self) -> Arc<Self> {
        // Create Arc pointer
        let inode: Arc<MountFSInode> = Arc::new(self);
        // Create Weak pointer
        let weak: Weak<MountFSInode> = Arc::downgrade(&inode);
        // Convert the Arc pointer to a raw pointer and assign to its internal self_ref field
        compiler_fence(Ordering::SeqCst);
        let ptr: *mut MountFSInode = inode.as_ref() as *const Self as *mut Self;
        compiler_fence(Ordering::SeqCst);
        unsafe {
            (*ptr).self_ref = weak;
            compiler_fence(Ordering::SeqCst);

            // Return the initialized MountFSInode object
            return inode;
        }
    }

    /// @brief Determine whether the current inode is the root inode of its filesystem
    fn is_mountpoint_root(&self) -> Result<bool, SystemError> {
        return Ok(self.mount_fs.root_inner_inode().metadata()?.inode_id
            == self.inner_inode.metadata()?.inode_id);
    }

    /// @brief Perform inode replacement on the mount tree.
    /// If the current inode is a mount point within the parent MountFS, this function returns
    /// the root inode of the filesystem mounted at that mount point.
    /// If the current inode is within the parent MountFS but is not a mount point, no inode
    /// replacement is needed, so the current inode is returned directly.
    ///
    /// @return Arc<MountFSInode>
    pub(crate) fn overlaid_inode(&self) -> Arc<MountFSInode> {
        let mut current = self.self_ref.upgrade().unwrap();
        for _ in 0..1024 {
            let inode_id = match current.metadata() {
                Ok(md) => md.inode_id,
                Err(e) => {
                    log::warn!(
                        "MountFSInode::overlaid_inode: metadata() failed: {:?}; treat as non-mountpoint",
                        e
                    );
                    return current;
                }
            };

            let Some(sub_mountfs) = current.mount_fs.mountpoints.lock().get(&inode_id).cloned()
            else {
                return current;
            };

            let next = sub_mountfs.mountpoint_root_inode();
            if Arc::ptr_eq(&next, &current) {
                return current;
            }
            current = next;
        }

        log::warn!("MountFSInode::overlaid_inode: overlay depth exceeds 1024");
        current
    }

    fn do_find(&self, name: &str) -> Result<Arc<MountFSInode>, SystemError> {
        let base = self.overlaid_inode();
        // Directly call the find method of the filesystem the current inode belongs to.
        // Since downward lookups may cross filesystem boundaries, we need to attempt inode replacement.
        let inner_inode = base.inner_inode.find(name)?;
        let mount_inode = Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode: inner_inode.clone(),
            mount_fs: base.mount_fs.clone(),
            self_ref: self_ref.clone(),
        });
        if let Some(fuse_node) =
            inner_inode.downcast_arc::<crate::filesystem::fuse::inode::FuseNode>()
        {
            let mut submount_path = base.absolute_path()?;
            if !submount_path.starts_with('/') {
                return Err(SystemError::EINVAL);
            }
            if submount_path != "/" {
                submount_path.push('/');
            }
            submount_path.push_str(name);
            crate::filesystem::fuse::fs::fuse_try_automount_submount(
                &fuse_node,
                &mount_inode,
                Some(Arc::new(MountPath::from(submount_path))),
            )?;
        }
        Ok(mount_inode.overlaid_inode())
    }

    pub(super) fn do_parent(&self) -> Result<Arc<MountFSInode>, SystemError> {
        if self.is_mountpoint_root()? {
            // The current inode is the root inode of its filesystem
            match self.mount_fs.self_mountpoint() {
                Some(inode) => {
                    // `inode` is the mount point inode in the “parent mount tree”.
                    // Linux semantics: going up (..) from the root of a mounted filesystem should
                    // return to the parent directory of the mount point, and subsequent path traversal
                    // should occur on the parent mount (inode.mount_fs).
                    //
                    // Here we directly reuse the mount point inode's do_parent() to ensure mount_fs is switched correctly.
                    return inode.do_parent();
                }
                None => {
                    return Ok(self.self_ref.upgrade().unwrap());
                }
            }
        } else {
            let inner_inode = self.inner_inode.parent()?;
            // When looking up parent, we don't cross filesystem boundaries, so directly call the parent method of the current inode's filesystem
            return Ok(Arc::new_cyclic(|self_ref| MountFSInode {
                inner_inode,
                mount_fs: self.mount_fs.clone(),
                self_ref: self_ref.clone(),
            }));
        }
    }

    /// Remove the filesystem mounted at this mount point
    fn do_umount(&self) -> Result<Arc<MountFS>, SystemError> {
        // Allow umount for both directory and file bind mounts
        let mountpoint_id = self.inner_inode.metadata()?.inode_id;

        // Detach first. Follow-up bookkeeping (peer registry and propagation)
        // must not run if detach itself failed.
        let child_mount = self
            .mount_fs
            .mountpoints
            .lock()
            .remove(&mountpoint_id)
            .ok_or_else(|| {
                log::warn!(
                    "do_umount: mountpoint id {:?} not found in parent fs '{}'",
                    mountpoint_id,
                    self.mount_fs.name()
                );
                SystemError::ENOENT
            })?;

        // Propagate umount to peers and slaves of the parent mount
        let parent_prop = self.mount_fs.propagation();
        if parent_prop.is_shared() {
            if let Err(e) = propagate_umount(&self.mount_fs, mountpoint_id) {
                log::warn!("do_umount: propagation failed: {:?}", e);
            }
        }

        // Remove detached mount from peer registry if needed.
        let child_prop = child_mount.propagation();
        if child_prop.is_shared() {
            unregister_peer(child_prop.peer_group_id(), &child_mount);
        }

        return Ok(child_mount);
    }

    #[inline(never)]
    fn do_absolute_path(&self) -> Result<String, SystemError> {
        // Prefer mount_list records: FUSE/virtiofs inodes may report synthetic paths
        // such as "fuse:<nodeid>" from absolute_path(), which breaks MS_MOVE path rewrite.
        if self.is_mountpoint_root()? {
            if let Some(path) = ProcessManager::current_mntns()
                .mount_list()
                .get_mount_path_by_mountfs(&self.mount_fs)
            {
                return Ok(path.as_str().to_string());
            }
        }

        let mut current = self.self_ref.upgrade().unwrap();

        // Only accept filesystem-provided paths that look like real VFS paths.
        if let Ok(p) = current.inner_inode.absolute_path() {
            if p.starts_with('/') {
                return Ok(p);
            }
        }

        let mut path_parts = Vec::new();

        // Note: different filesystems may have independent inode_id spaces, so “global root inode_id” cannot be used as a termination condition.
        // The correct approach is to walk up the mount tree until reaching the “namespace root” (i.e., the rootfs mount where self_mountpoint is None).
        loop {
            // Reached the current namespace root: stop.
            if current.is_mountpoint_root()?
                && current
                    .mount_fs
                    .namespace()
                    .is_some_and(|ns| Arc::ptr_eq(&current.mount_fs, ns.root_mntfs()))
            {
                break;
            }

            // Compatibility with the old model: if the mount has no mount point, treat it as root.
            if current.is_mountpoint_root()? && current.mount_fs.self_mountpoint().is_none() {
                break;
            }

            let name = current.dname()?;
            path_parts.push(name.0);

            // Loop prevention: if path depth exceeds 1024, emit a warning
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
                // parent == self but haven't reached the global root, indicating incomplete mount tree info or a cycle
                log::warn!(
                    "absolute_path: parent == self before reaching namespace root, inode_id={}",
                    current.metadata().unwrap().inode_id.data()
                );
                return Err(SystemError::ELOOP);
            }
            current = parent;
        }

        // Since we traversed from leaf to root, reverse the path parts
        path_parts.reverse();

        // Build the final absolute path string
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

    pub fn mount_fs(&self) -> Arc<MountFS> {
        self.mount_fs.clone()
    }

    pub fn inode_id(&self) -> Result<InodeId, SystemError> {
        Ok(self.inner_inode.metadata()?.inode_id)
    }
}

impl IndexNode for MountFSInode {
    fn open(
        &self,
        data: MutexGuard<FilePrivateData>,
        flags: &FileFlags,
    ) -> Result<(), SystemError> {
        let access = flags.access_flags();
        if (access == FileFlags::O_WRONLY
            || access == FileFlags::O_RDWR
            || flags.contains(FileFlags::O_TRUNC))
            && self.mount_fs.is_readonly()
        {
            return Err(SystemError::EROFS);
        }
        return self.inner_inode.open(data, flags);
    }

    fn mmap(&self, start: usize, len: usize, offset: usize) -> Result<(), SystemError> {
        return self.inner_inode.mmap(start, len, offset);
    }

    fn check_mmap_file(
        &self,
        file: &Arc<super::file::File>,
        len: usize,
        offset: usize,
        vm_flags: VmFlags,
    ) -> Result<(), SystemError> {
        self.inner_inode
            .check_mmap_file(file, len, offset, vm_flags)
    }

    fn mmap_file(
        &self,
        file: &Arc<super::file::File>,
        start: usize,
        len: usize,
        offset: usize,
        vm_flags: VmFlags,
    ) -> Result<(), SystemError> {
        self.inner_inode
            .mmap_file(file, start, len, offset, vm_flags)
    }

    fn truncate_before_open(&self, flags: &FileFlags) -> bool {
        self.inner_inode.truncate_before_open(flags)
    }

    fn sync(&self) -> Result<(), SystemError> {
        return self.inner_inode.sync();
    }

    fn sync_file(
        &self,
        datasync: bool,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        self.inner_inode.sync_file(datasync, data)
    }

    fn sync_file_range(
        &self,
        start: usize,
        end: usize,
        datasync: bool,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        self.inner_inode.sync_file_range(start, end, datasync, data)
    }

    fn write_inode(&self, wbc: &super::WritebackControl) -> Result<(), SystemError> {
        self.inner_inode.write_inode(wbc)
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
        self.ensure_mount_writable()?;
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
        self.ensure_mount_writable()?;
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
        self.ensure_mount_writable()?;
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
        self.ensure_mount_writable()?;
        self.inner_inode.write_direct(offset, len, buf, data)
    }

    #[inline]
    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.overlaid_inode().mount_fs.clone();
    }

    #[inline]
    fn mount_flags(&self) -> MountFlags {
        self.mount_fs.mount_flags()
    }

    #[inline]
    fn as_any_ref(&self) -> &dyn core::any::Any {
        return self.inner_inode.as_any_ref();
    }

    #[inline]
    fn metadata(&self) -> Result<super::Metadata, SystemError> {
        let mut md = self.inner_inode.metadata()?;

        // Provide a stable and unique st_dev for each mount point (via metadata.dev_id).
        // This handles the case where the underlying filesystem does not provide a dev_id.
        if md.dev_id == 0 {
            let mnt_id: usize = self.mount_fs.mount_id().into();
            let minor = (mnt_id as u32) & DeviceNumber::MINOR_MASK;
            md.dev_id = DeviceNumber::new(Major::UNNAMED_MAJOR, minor).data() as usize;
        }

        Ok(md)
    }

    #[inline]
    fn set_metadata(&self, metadata: &super::Metadata) -> Result<(), SystemError> {
        self.ensure_mount_writable()?;
        return self.inner_inode.set_metadata(metadata);
    }

    #[inline]
    fn resize(&self, len: usize) -> Result<(), SystemError> {
        self.ensure_mount_writable()?;
        return self.inner_inode.resize(len);
    }

    #[inline]
    fn create(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.ensure_mount_writable()?;
        let inner_inode = self.inner_inode.create(name, file_type, mode)?;
        return Ok(Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode,
            mount_fs: self.mount_fs.clone(),
            self_ref: self_ref.clone(),
        }));
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        self.ensure_mount_writable()?;
        // Filesystem implementations expect `other` to be an inode of the same concrete filesystem (e.g. LockedExt4Inode).
        // When VFS mount wrapping is enabled, `other` is typically a `MountFSInode`, which causes
        // filesystem-level downcasts to fail and incorrectly return EINVAL.
        //
        // Therefore, before linking, we need to unwrap the mount wrapper (same as move_to).
        let other_inner: Arc<dyn IndexNode> = other
            .clone()
            .downcast_arc::<MountFSInode>()
            .map(|mnt| mnt.inner_inode.clone())
            .unwrap_or_else(|| other.clone());

        return self.inner_inode.link(name, &other_inner);
    }

    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.ensure_mount_writable()?;
        let inner_inode = self.inner_inode.symlink(name, target)?;
        Ok(Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode,
            mount_fs: self.mount_fs.clone(),
            self_ref: self_ref.clone(),
        }))
    }

    /// @brief Delete a file/directory in the mounted filesystem
    #[inline]
    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        self.ensure_mount_writable()?;
        let inode_id = self.inner_inode.find(name)?.metadata()?.inode_id;

        // First check if this inode is a mount point; if so, it cannot be deleted
        if self.mount_fs.mountpoints.lock().contains_key(&inode_id) {
            return Err(SystemError::EBUSY);
        }
        // Delegate to the inner inode's unlink method to delete this inode
        return self.inner_inode.unlink(name);
    }

    #[inline]
    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        self.ensure_mount_writable()?;
        let inode_id = self.inner_inode.find(name)?.metadata()?.inode_id;

        // First check if this inode is a mount point; if so, it cannot be deleted
        if self.mount_fs.mountpoints.lock().contains_key(&inode_id) {
            return Err(SystemError::EBUSY);
        }
        // Delegate to the inner inode's rmdir method to delete this inode
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
        self.ensure_mount_writable()?;
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

    fn check_access(
        &self,
        mask: crate::filesystem::vfs::permission::PermissionMask,
    ) -> Result<(), SystemError> {
        self.inner_inode.check_access(mask)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        match name {
            // Looking up the current directory
            "" | "." => self
                .self_ref
                .upgrade()
                .map(|inode| inode.overlaid_inode() as Arc<dyn IndexNode>)
                .ok_or(SystemError::ENOENT),
            // Looking up the parent directory
            ".." => self.parent(),
            // Looking up within the current directory
            // Directly call the find method of the filesystem the current inode belongs to.
            // Since downward lookups may cross filesystem boundaries, we need to attempt inode replacement.
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
        private_data: MutexGuard<FilePrivateData>,
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
        let (to_mount_fs, root_inner_inode) = fs
            .clone()
            .downcast_arc::<MountFS>()
            .map(|it| (it.inner_filesystem(), it.root_inner_inode()))
            .unwrap_or_else(|| {
                let root_inner_inode = fs.root_inode();
                (fs, root_inner_inode)
            });

        self.mount_subtree(to_mount_fs, root_inner_inode, mount_flags)
    }

    fn mount_from(&self, from: Arc<dyn IndexNode>) -> Result<Arc<MountFS>, SystemError> {
        let metadata = self.metadata()?;
        if from.metadata()?.file_type != FileType::Dir || metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        if self.is_mountpoint_root()? {
            return Err(SystemError::EBUSY);
        }

        // Corresponds to Linux do_move_mount → attach_recursive_mnt(MNT_TREE_MOVE):
        // unhash_mnt (detach) then attach directly to the new location, without clearing mnt_ns or notifying the filesystem.
        //
        // Reuse the core topology move logic of MS_MOVE (detach + attach + mount_list subtree path rewrite)
        // to avoid maintaining two separate move implementations. This path is only used for system initialization
        // migration of proc/dev/sys, where the target parent mount is private and the moved mount has no child mounts,
        // so propagation is not needed.
        let from_mfs = from
            .fs()
            .downcast_arc::<MountFS>()
            .ok_or(SystemError::EINVAL)?;

        let target_mountpoint = self.self_ref.upgrade().unwrap();
        let mntns = ProcessManager::current_mntns();
        let old_source_path = mntns
            .mount_list()
            .get_mount_path_by_mountfs(&from_mfs)
            .map(|p| p.as_str().to_string())
            .or_else(|| {
                from_mfs
                    .self_mountpoint()
                    .and_then(|mp| mp.absolute_path().ok())
            })
            .filter(|p| p.starts_with('/'))
            .ok_or(SystemError::EINVAL)?;
        let new_target_path = target_mountpoint
            .absolute_path()
            .ok()
            .filter(|p| p.starts_with('/'))
            .ok_or(SystemError::EINVAL)?;
        mntns.move_mount(
            &from_mfs,
            &target_mountpoint,
            &old_source_path,
            &new_target_path,
        )?;

        return Ok(from_mfs);
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
        self.ensure_mount_writable()?;
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

    /// If not supported, fall back to getting the filename from the parent directory.
    /// # Performance
    /// DName should be introduced wherever possible;
    /// by default, performance is very poor!
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
        self.ensure_mount_writable()?;
        self.inner_inode.write_sync(offset, buf)
    }

    fn getxattr(&self, name: &str, buf: &mut [u8]) -> Result<usize, SystemError> {
        self.inner_inode.getxattr(name, buf)
    }

    fn setxattr(&self, name: &str, value: &[u8]) -> Result<usize, SystemError> {
        self.ensure_mount_writable()?;
        self.inner_inode.setxattr(name, value)
    }
}

impl FileSystem for MountFS {
    fn support_readahead(&self) -> bool {
        self.inner_filesystem.support_readahead()
    }
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        // A mounted filesystem's root inode is always its own mount root wrapper.
        // Returning the parent mount's root breaks mount-root checks such as pivot_root(2).
        self.mountpoint_root_inode()
    }

    fn info(&self) -> super::FsInfo {
        return self.inner_filesystem.info();
    }

    /// @brief This function is used for dynamic casting.
    /// The simplest implementation for concrete filesystems is to return self directly.
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        self.inner_filesystem.name()
    }
    fn super_block(&self) -> SuperBlock {
        let mut sb = self.inner_filesystem.super_block();
        sb.flags = self.combined_flags().bits() as u64;
        sb
    }

    fn statfs(&self, inode: &Arc<dyn IndexNode>) -> Result<SuperBlock, SystemError> {
        let inner_inode = inode
            .as_any_ref()
            .downcast_ref::<MountFSInode>()
            .map(|mnt| mnt.inner_inode.clone())
            .unwrap_or_else(|| inode.clone());
        let mut sb = self.inner_filesystem.statfs(&inner_inode)?;
        sb.flags = self.combined_flags().bits() as u64;
        Ok(sb)
    }

    fn permission_policy(&self) -> crate::filesystem::vfs::FsPermissionPolicy {
        self.inner_filesystem.permission_policy()
    }

    unsafe fn fault(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        self.inner_filesystem.fault(pfm)
    }

    fn mprotect(&self, old_vm_flags: VmFlags, new_vm_flags: VmFlags) -> Result<(), SystemError> {
        self.inner_filesystem.mprotect(old_vm_flags, new_vm_flags)
    }

    unsafe fn map_pages(
        &self,
        pfm: &mut PageFaultMessage,
        start_pgoff: usize,
        end_pgoff: usize,
    ) -> VmFaultReason {
        self.inner_filesystem.map_pages(pfm, start_pgoff, end_pgoff)
    }

    fn sync_fs(&self, wait: bool) -> Result<(), SystemError> {
        self.inner_filesystem.sync_fs(wait)
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
            // Same depth: sort in reverse order
            // Both the root directory and files directly under root have only one '/' in their absolute path
            other.0.cmp(&self.0)
        } else {
            // Sort by depth (deeper first)
            othe_dep.cmp(&self_dep)
        }
    }
}

impl MountPath {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// Maintain mount point records to support filesystem-specific indexing
pub struct MountList {
    inner: RwSem<InnerMountList>,
}

#[derive(Clone, Debug)]
struct MountRecord {
    fs: Arc<MountFS>,
    ino: Option<InodeId>,
}

struct InnerMountList {
    /// The same path may be mounted multiple times; stored as a stack with the top being the currently visible mount.
    mounts: HashMap<Arc<MountPath>, Vec<MountRecord>>,
    /// Reverse lookup from MountFS to mount point inode.
    mfs2ino: HashMap<Arc<MountFS>, InodeId>,
    /// Reverse lookup from a specific mount to its mount path. The same inode may correspond to multiple propagation replica paths.
    mfs2mp: HashMap<Arc<MountFS>, Arc<MountPath>>,
    /// Mapping from inode to path, used for child mount lookup.
    ino2mp: HashMap<InodeId, Arc<MountPath>>,
}

impl MountList {
    /// # new — Create a new MountList instance
    ///
    /// Creates an empty mount point list.
    ///
    /// ## Returns
    ///
    /// - `Arc<MountList>`: A shared empty mount point list instance.
    pub fn new() -> Arc<Self> {
        Arc::new(MountList {
            inner: RwSem::new(InnerMountList {
                mounts: HashMap::new(),
                ino2mp: HashMap::new(),
                mfs2ino: HashMap::new(),
                mfs2mp: HashMap::new(),
            }),
        })
    }

    /// Inserts a filesystem mount point into the mount list.
    ///
    /// This function records a new mount at `path`. Multiple mounts may exist at
    /// the same path; they are stored as a stack, and the newest entry is the
    /// visible mount returned by lookup helpers.
    ///
    /// # Thread Safety
    /// This function is thread-safe as it uses a RwSem to ensure safe concurrent access.
    ///
    /// # Arguments
    /// * `ino` - Optional inode id of the parent-side mount point.
    /// * `path` - Namespace path where the filesystem is mounted.
    /// * `fs` - MountFS instance mounted at the specified path.
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
        inner.mfs2mp.insert(fs.clone(), path.clone());
        // If ino is None (e.g. root mount), still keep the mounts stack for subsequent pop.
    }

    /// # get_mount_point — Get the mount point path
    ///
    /// This function looks up the mount point for a given path. It searches an internal map
    /// to find a mount point matching the path.
    ///
    /// ## Arguments
    ///
    /// - `path: T`: A reference convertible to a string, representing the path whose mount point to look up.
    ///
    /// ## Returns
    ///
    /// - `Option<(Arc<MountPath>, String, Arc<MountFS>)>`:
    ///   - `Some((mount_point, rest_path, fs))`: If a matching mount point is found, returns the recorded mount path, remaining path, and currently visible mounted filesystem.
    ///   - `None`: If no matching mount point is found.
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

    /// # remove — Remove a mount point
    ///
    /// Removes the currently visible mount at `path`.
    ///
    /// If multiple mounts are stacked on the same path, this function pops only
    /// the top entry. Lower entries remain recorded and become visible again. If
    /// the path has no mount stack, no action is taken.
    ///
    /// ## Arguments
    ///
    /// - `path: T`: `T` implements `Into<MountPath>`, representing the path of the mount point to remove.
    ///
    /// ## Returns
    ///
    /// - `Option<Arc<MountFS>>`: the removed visible mount, or `None` if the
    ///   path does not exist in the mount list.
    #[inline(never)]
    pub fn remove<T: Into<MountPath>>(&self, path: T) -> Option<Arc<MountFS>> {
        let mut inner = self.inner.write();
        let path = Arc::new(path.into());
        if let Some(mut stack) = inner.mounts.remove(&path) {
            if let Some(rec) = stack.pop() {
                let rec_fs = rec.fs.clone();
                if let Some(ino) = inner.mfs2ino.remove(&rec_fs) {
                    inner.ino2mp.remove(&ino);
                }
                inner.mfs2mp.remove(&rec_fs);
                if let Some(ino) = rec.ino {
                    inner.ino2mp.remove(&ino);
                }

                if let Some(visible) = stack.last() {
                    inner.mfs2mp.insert(visible.fs.clone(), path.clone());
                    if let Some(ino) = visible.ino {
                        inner.mfs2ino.insert(visible.fs.clone(), ino);
                        inner.ino2mp.insert(ino, path.clone());
                    }
                    inner.mounts.insert(path.clone(), stack);
                }
                return Some(rec_fs);
            }
        }
        None
    }

    pub fn rewrite_paths<F>(&self, mut rewrite: F)
    where
        F: FnMut(&str) -> Option<String>,
    {
        let mut inner = self.inner.write();
        let old_mounts = mem::take(&mut inner.mounts);
        let mut new_mounts = HashMap::new();
        let mut new_ino2mp = HashMap::new();
        let mut new_mfs2ino = HashMap::new();
        let mut new_mfs2mp = HashMap::new();

        for (old_path, stack) in old_mounts {
            let Some(new_path) = rewrite(old_path.as_str()) else {
                continue;
            };
            let new_path = Arc::new(MountPath::from(new_path));
            let entry = new_mounts.entry(new_path.clone()).or_insert_with(Vec::new);

            for rec in stack {
                if let Some(ino) = rec.ino {
                    new_ino2mp.insert(ino, new_path.clone());
                    new_mfs2ino.insert(rec.fs.clone(), ino);
                }
                new_mfs2mp.insert(rec.fs.clone(), new_path.clone());
                entry.push(rec);
            }
        }

        inner.mounts = new_mounts;
        inner.ino2mp = new_ino2mp;
        inner.mfs2ino = new_mfs2ino;
        inner.mfs2mp = new_mfs2mp;
    }

    /// # clone_inner — Clone the internal mount point list
    pub fn clone_inner(&self) -> HashMap<Arc<MountPath>, Arc<MountFS>> {
        self.inner
            .read()
            .mounts
            .iter()
            .map(|(p, stack)| (p.clone(), stack.last().unwrap().fs.clone()))
            .collect()
    }

    /// Clone every mount record, including lower entries in a same-path mount stack.
    pub fn clone_records(&self) -> Vec<(Arc<MountPath>, Arc<MountFS>)> {
        self.inner
            .read()
            .mounts
            .iter()
            .flat_map(|(path, stack)| {
                stack
                    .iter()
                    .map(|rec| (path.clone(), rec.fs.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub fn get<T: Into<MountPath>>(&self, path: T) -> Option<Arc<MountFS>> {
        let inner = self.inner.read();
        let path: MountPath = path.into();
        inner
            .mounts
            .get(&path)
            .and_then(|stack| stack.last().map(|rec| rec.fs.clone()))
    }

    #[inline(never)]
    pub fn get_mount_path_by_ino(&self, ino: InodeId) -> Option<Arc<MountPath>> {
        self.inner.read().ino2mp.get(&ino).cloned()
    }

    #[inline(never)]
    pub fn get_mount_path_by_mountfs(&self, mountfs: &Arc<MountFS>) -> Option<Arc<MountPath>> {
        self.inner.read().mfs2mp.get(mountfs).cloned()
    }
}

impl Debug for MountList {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let inner = self.inner.read();
        f.debug_map().entries(inner.mounts.iter()).finish()
    }
}

/// Determine whether the given inode is the root inode of its mounted filesystem.
///
/// ## Arguments
///
/// - `inode`: inode to test. Non-`MountFSInode` values are treated as not being
///   a mount root.
///
/// ## Returns
///
/// - `true`: `inode` is a `MountFSInode` whose inner inode is the root inode of
///   its `MountFS`.
/// - `false`: `inode` is not a mount root, is not a `MountFSInode`, or metadata
///   lookup failed.
pub fn is_mountpoint_root(inode: &Arc<dyn IndexNode>) -> bool {
    let mnt_inode = inode.clone().downcast_arc::<MountFSInode>();
    if let Some(mnt) = mnt_inode {
        return mnt.is_mountpoint_root().unwrap_or(false);
    }

    return false;
}

/// # do_mount_mkdir — Create a directory at the specified mount point and mount a filesystem
///
/// Creates `mount_point` with mode `0755`, rejects it if it is already an
/// existing mount point, and mounts `fs` there with the supplied mount flags.
///
/// ## Arguments
///
/// - `fs`: filesystem instance to mount.
/// - `mount_point`: path of the directory to create and use as mount point.
/// - `mount_flags`: per-mount flags for the new mount.
///
/// ## Returns
///
/// - `Ok(Arc<MountFS>)`: returns the newly mounted `MountFS`.
/// - `Err(SystemError)`: On mount failure, returns a system error.
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
