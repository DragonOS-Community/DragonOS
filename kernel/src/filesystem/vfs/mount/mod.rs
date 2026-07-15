use super::{
    file::{File, FileFlags, FileMode, PreopenedFile},
    utils::DName,
    FilePrivateData, FileSystem, FileType, IndexNode, InodeId, InodeMode, InodeRetentionKind,
    PollableInode, SetMetadataMask, SuperBlock, XattrFlags,
};
use crate::{
    driver::base::device::device_number::{DeviceNumber, Major},
    exception::workqueue::{schedule_work, Work},
    filesystem::{
        page_cache::list_page_caches,
        page_cache::PageCache,
        vfs::{fcntl::AtFlags, syscall::RenameFlags, vcore::do_mkdir_at},
    },
    libs::{
        casting::DowncastArc,
        errseq::{ErrSeq, ErrSeqValue},
        mutex::{Mutex, MutexGuard},
        rwsem::{RwSem, RwSemWriteGuard},
        spinlock::SpinLock,
        wait_queue::WaitQueue,
    },
    mm::{fault::PageFaultMessage, VirtRegion, VmFaultReason, VmFlags},
    process::{
        namespace::{
            mnt::MntNamespace,
            propagation::{
                abort_mount_propagation, commit_mount_propagation_locked, detach_mount_propagation,
                ensure_subtree_shared, inherit_bind_mount_propagation,
                prepare_mount_propagation_locked, propagate_umount_sources,
                propagation_umount_busy, MountPropagation,
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
    cell::RefCell,
    fmt::Debug,
    hash::Hash,
    sync::atomic::{compiler_fence, AtomicBool, AtomicUsize, Ordering},
};
use hashbrown::HashMap;
use ida::IdAllocator;
use lazy_static::lazy_static;
use system_error::SystemError;

/// Serializes mount pin admission against multi-mount busy preflight and
/// detach, including propagation peers in other namespaces.
///
/// Mount topology and propagation code acquires locks in this order:
/// lifecycle -> namespace -> dentry mount gate -> parent mountpoints -> peer
/// registry -> one mount's propagation state -> propagation group allocator.
/// A lower layer must never acquire the lifecycle/topology layers in reverse.
pub(crate) static MOUNT_LIFECYCLE_LOCK: Mutex<()> = Mutex::new(());

lazy_static! {
    /// Serializes pathname rendering against alias rename/disconnect. Mount
    /// topology is protected separately by `MOUNT_LIFECYCLE_LOCK`; readers
    /// acquire it first when both are needed.
    static ref DENTRY_TOPOLOGY_LOCK: RwSem<()> = RwSem::new(());
}

/// Capability for one layered directory mutation to acquire the global dentry
/// topology write lock exactly once.  Acquisition is deliberately lazy:
/// layered filesystems may prepare a copy-up before the innermost backing
/// filesystem reaches its namespace commit, avoiding both a copy-up/global-lock
/// inversion and holding a system-wide lock across file-data I/O.
pub struct DentryMutationContext<'a> {
    guard: RefCell<Option<RwSemWriteGuard<'a, ()>>>,
}

impl DentryMutationContext<'static> {
    fn new() -> Self {
        Self {
            guard: RefCell::new(None),
        }
    }
}

impl DentryMutationContext<'_> {
    /// Enter the namespace commit phase.  The guard remains owned by this
    /// context until the outermost mount wrapper has updated every alias.
    pub(crate) fn ensure_locked(&self) {
        let mut guard = self.guard.borrow_mut();
        if guard.is_none() {
            *guard = Some(DENTRY_TOPOLOGY_LOCK.write());
        }
    }
}

pub(crate) fn with_topology_snapshot<T>(f: impl FnOnce() -> T) -> T {
    let _mounts = MOUNT_LIFECYCLE_LOCK.lock();
    let _dentries = DENTRY_TOPOLOGY_LOCK.read();
    f()
}

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

static NEXT_DENTRY_ID: AtomicUsize = AtomicUsize::new(1);

/// Linux `unnamed_dev_ida` 的 DragonOS 等价物。minor 0 保留为“尚未分配”，
/// 上界传入 `MINOR_MASK + 1` 是因为 `IdAllocator` 的 max_id 为开区间。
static UNNAMED_DEV_ID_ALLOCATOR: Mutex<IdAllocator> =
    Mutex::new(IdAllocator::new(1, DeviceNumber::MINOR_MASK as usize + 1).unwrap());

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
    /// Shared alias identity of `root_inner_inode`. Bind mounts and namespace
    /// copies retain this exact object instead of reconstructing an alias from
    /// an inode number or a pathname.
    root_dentry: Arc<VfsDentry>,
    /// Stable VFS wrapper for the root of this mount. Besides avoiding needless
    /// allocations, this keeps the root dentry's child cache shared by all
    /// lookups that enter the mount.
    root_inode: Mutex<Weak<MountFSInode>>,
    /// Per-mount projections of shared dentries. The values are weak because
    /// paths and topology edges own the semantic references.
    wrapper_cache: Mutex<BTreeMap<DentryId, Weak<MountFSInode>>>,
    /// Ordered shadow stack for every exact `(parent mount, dentry)` edge.
    /// The last element is the visible/top mount.
    mountpoints: Mutex<HashMap<DentryId, Vec<Arc<MountFS>>>>,
    /// Marks a covered topper reparented onto a propagated underlay root.
    /// The edge role is copied with the mount object.
    tucked_under: AtomicBool,
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
    /// Internal MNT_LOCKED equivalent; never exposed as a userspace MS_* bit.
    locked: AtomicBool,
    lifecycle: Mutex<MountLifecycle>,
}

/// Capacity reserved for one future mount edge. Creating an empty map entry is
/// topology-neutral; if prepare aborts before the slot is consumed, Drop
/// removes that entry so failed events leave no mountpoint residue.
pub(crate) struct MountEdgeReservation {
    parent: Arc<MountFS>,
    mountpoint: Arc<MountFSInode>,
}

impl Drop for MountEdgeReservation {
    fn drop(&mut self) {
        let mut mountpoints = self.parent.mountpoints.lock();
        let dentry_id = self.mountpoint.dentry.id;
        if mountpoints
            .get(&dentry_id)
            .is_some_and(|stack| stack.is_empty())
        {
            mountpoints.remove(&dentry_id);
        }
    }
}

impl MountEdgeReservation {
    pub(crate) fn mountpoint(&self) -> &Arc<MountFSInode> {
        &self.mountpoint
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MountLifecycleState {
    Constructing,
    Live,
    Detaching,
    DetachedConnected,
    Detached,
}

#[derive(Debug)]
struct MountLifecycle {
    state: MountLifecycleState,
    external_pins: usize,
    construction_reserved: bool,
    propagation_attached: bool,
    detached_component: Option<Arc<DetachedMountComponent>>,
}

#[derive(Debug)]
struct DetachedMountComponent {
    root: Weak<MountFS>,
    members: Vec<Weak<MountFS>>,
    pins: AtomicUsize,
}

impl DetachedMountComponent {
    const INITIALIZING: usize = 1usize << (usize::BITS - 1);

    fn new(root: &Arc<MountFS>, members: &[Arc<MountFS>]) -> Arc<Self> {
        Arc::new(Self {
            root: Arc::downgrade(root),
            members: members.iter().map(Arc::downgrade).collect(),
            pins: AtomicUsize::new(Self::INITIALIZING),
        })
    }

    fn add_initial_pins(&self, pins: usize) {
        self.pins.fetch_add(pins, Ordering::Relaxed);
    }

    fn finish_initialization(self: &Arc<Self>) {
        let previous = self.pins.fetch_and(!Self::INITIALIZING, Ordering::AcqRel);
        if previous == Self::INITIALIZING {
            self.schedule_cleanup();
        }
    }

    fn try_pin(&self) -> bool {
        let mut current = self.pins.load(Ordering::Acquire);
        loop {
            if current & !Self::INITIALIZING == 0 && current & Self::INITIALIZING == 0 {
                return false;
            }
            match self.pins.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    fn unpin(self: &Arc<Self>) {
        let previous = self.pins.fetch_sub(1, Ordering::AcqRel);
        debug_assert_ne!(previous & !Self::INITIALIZING, 0);
        if previous == 1 {
            self.schedule_cleanup();
        }
    }

    fn schedule_cleanup(self: &Arc<Self>) {
        let component = self.clone();
        schedule_work(Work::new(move || component.cleanup()));
    }

    fn cleanup(&self) {
        let _topology = MOUNT_LIFECYCLE_LOCK.lock();
        if self.pins.load(Ordering::Acquire) != 0 {
            return;
        }
        let Some(root) = self.root.upgrade() else {
            return;
        };
        for member in self.members.iter().filter_map(Weak::upgrade) {
            let mut lifecycle = member.lifecycle.lock();
            lifecycle.detached_component = None;
        }
        MountFS::deactivate_disconnected_subtree(&root);
    }
}

/// A semantic reference to a path or open file description on one mount.
///
/// Unlike an `Arc<MountFS>`, this reference participates in ordinary umount's
/// busy decision. It is intentionally not `Clone`: every independently owned
/// path must explicitly acquire its own pin.
#[derive(Debug)]
pub struct MountExternalGuard {
    mount: Arc<MountFS>,
}

/// Keeps the superblock backend alive while a topology snapshot is rendered,
/// without making ordinary umount report the mount busy.
#[derive(Debug)]
pub(crate) struct MountSnapshotGuard {
    mount: Arc<MountFS>,
}

unsafe impl Send for MountSnapshotGuard {}
unsafe impl Sync for MountSnapshotGuard {}

// SAFETY: MountExternalGuard only owns an Arc<MountFS>. Every mutable MountFS
// field reachable from the guard is protected by Mutex/RwSem/SpinLock or is
// atomic. These explicit impls break the recursive auto-trait proof cycle
// MountFS -> MountFSInode/File -> MountExternalGuard -> MountFS, which some
// cross-target rustc builds cannot normalize within the default recursion
// limit; they do not weaken the synchronization requirements of MountFS.
unsafe impl Send for MountExternalGuard {}
unsafe impl Sync for MountExternalGuard {}

#[derive(Debug)]
pub struct SuperBlockState {
    flags: RwSem<MountFlags>,
    write_count: AtomicUsize,
    wb_error: ErrSeq,
    umount_lock: RwSem<()>,
    unnamed_dev_minor: Mutex<Option<u32>>,
    /// Shared by all mounts of this superblock, including bind mounts.
    dentry_namespace_lock: RwSem<()>,
    dentry_registry: Mutex<BTreeMap<DentryRegistryKey, Weak<VfsDentry>>>,
    lifecycle: Mutex<SuperBlockLifecycle>,
    shutdown_wait: WaitQueue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SuperBlockLifecycleState {
    Active,
    Dying,
    Dead,
}

#[derive(Debug)]
struct SuperBlockLifecycle {
    active_mounts: usize,
    external_pins: usize,
    state: SuperBlockLifecycleState,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DentryRegistryKey {
    parent: Option<DentryId>,
    child: InodeId,
    child_generation: u64,
    name: Option<DName>,
}

int_like!(DentryId, usize);

impl DentryId {
    fn alloc() -> Self {
        Self(NEXT_DENTRY_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// Minimal superblock-wide dentry identity.
///
/// This deliberately does not implement a complete dcache. It only owns the
/// alias state that must be observed consistently by every bind mount and
/// mount-namespace copy of the same superblock.
#[derive(Debug)]
pub struct VfsDentry {
    id: DentryId,
    inode: Arc<dyn IndexNode>,
    registry_child: InodeId,
    /// Generation captured when this alias entered the registry.  It must not
    /// follow later FUSE invalidations, otherwise the original key could no
    /// longer be removed deterministically.
    registry_generation: u64,
    /// Serializes exact-edge attach/detach against rename/unlink/rmdir of this
    /// alias without holding a global mount lock across filesystem I/O.
    mount_gate: Mutex<()>,
    /// Serializes lookup/registration and namespace mutations of this
    /// directory's children. Layered copy-up may run while this per-directory
    /// gate is held, but unrelated directories and filesystems remain free.
    children_gate: Mutex<()>,
    /// Global fast-path hint; namespace-local checks still inspect topology.
    mount_edges: AtomicUsize,
    automount_gate: Mutex<()>,
    state: Mutex<VfsDentryState>,
}

#[derive(Debug, Default)]
struct VfsDentryState {
    name: Option<DName>,
    parent: Option<Arc<VfsDentry>>,
    disconnected: bool,
}

impl VfsDentry {
    pub fn id(&self) -> DentryId {
        self.id
    }

    fn is_disconnected(&self) -> bool {
        self.state.lock().disconnected
    }

    fn is_local_mountpoint(&self) -> bool {
        if self.mount_edges.load(Ordering::Acquire) == 0 {
            return false;
        }
        let current_namespace = ProcessManager::current_mntns();
        let mounts = {
            let mut registry = MOUNTED_SUPERBLOCKS.lock_irqsave();
            registry.retain(|mount| mount.strong_count() != 0);
            registry.clone()
        };
        mounts.iter().filter_map(Weak::upgrade).any(|mount| {
            mount.is_belongs_to_mntns(&current_namespace)
                && mount
                    .self_mountpoint()
                    .is_some_and(|mountpoint| mountpoint.dentry.id == self.id)
        })
    }
}

fn dentry_is_descendant_of(dentry: &Arc<VfsDentry>, ancestor: &Arc<VfsDentry>) -> bool {
    let mut current = Some(dentry.clone());
    let mut visited = hashbrown::HashSet::new();
    while let Some(dentry) = current {
        if dentry.id == ancestor.id {
            return true;
        }
        if !visited.insert(dentry.id) {
            log::warn!("cycle detected in shared VFS dentry ancestry");
            return false;
        }
        current = dentry.state.lock().parent.clone();
    }
    false
}

fn with_dentry_mount_gates<T>(
    first: Option<&Arc<VfsDentry>>,
    second: Option<&Arc<VfsDentry>>,
    operation: impl FnOnce() -> Result<T, SystemError>,
) -> Result<T, SystemError> {
    match (first, second) {
        (None, None) => operation(),
        (Some(dentry), None) | (None, Some(dentry)) => {
            let _guard = dentry.mount_gate.lock();
            operation()
        }
        (Some(left), Some(right)) if left.id == right.id => {
            let _guard = left.mount_gate.lock();
            operation()
        }
        (Some(left), Some(right)) if left.id < right.id => {
            let _left = left.mount_gate.lock();
            let _right = right.mount_gate.lock();
            operation()
        }
        (Some(left), Some(right)) => {
            let _right = right.mount_gate.lock();
            let _left = left.mount_gate.lock();
            operation()
        }
    }
}

fn with_dentry_children_gates<T>(
    first: &Arc<VfsDentry>,
    second: &Arc<VfsDentry>,
    operation: impl FnOnce() -> Result<T, SystemError>,
) -> Result<T, SystemError> {
    if first.id == second.id {
        let _guard = first.children_gate.lock();
        operation()
    } else if first.id < second.id {
        let _first = first.children_gate.lock();
        let _second = second.children_gate.lock();
        operation()
    } else {
        let _second = second.children_gate.lock();
        let _first = first.children_gate.lock();
        operation()
    }
}

struct MountStateInit {
    super_block_state: Arc<SuperBlockState>,
    mount_source: Option<String>,
    construction_reserved: bool,
}

impl SuperBlockState {
    pub fn new(flags: MountFlags) -> Self {
        Self {
            flags: RwSem::new(flags & MountFlags::SB_SETTABLE_MASK),
            write_count: AtomicUsize::new(0),
            wb_error: ErrSeq::new(),
            umount_lock: RwSem::new(()),
            unnamed_dev_minor: Mutex::new(None),
            dentry_namespace_lock: RwSem::new(()),
            dentry_registry: Mutex::new(BTreeMap::new()),
            lifecycle: Mutex::new(SuperBlockLifecycle {
                active_mounts: 0,
                external_pins: 0,
                state: SuperBlockLifecycleState::Active,
            }),
            shutdown_wait: WaitQueue::default(),
        }
    }

    fn activate_mount(&self, construction_reserved: bool) -> Result<(), SystemError> {
        let mut lifecycle = self.lifecycle.lock();
        if lifecycle.state != SuperBlockLifecycleState::Active {
            return Err(SystemError::ESTALE);
        }
        lifecycle.active_mounts += 1;
        if construction_reserved {
            debug_assert!(lifecycle.external_pins > 0);
            lifecycle.external_pins -= 1;
        }
        Ok(())
    }

    fn try_add_external_pin(&self) -> bool {
        let mut lifecycle = self.lifecycle.lock();
        if lifecycle.state != SuperBlockLifecycleState::Active {
            return false;
        }
        lifecycle.external_pins += 1;
        true
    }

    fn remove_external_pin(&self) -> bool {
        let mut lifecycle = self.lifecycle.lock();
        debug_assert!(lifecycle.external_pins > 0);
        lifecycle.external_pins -= 1;
        Self::try_begin_shutdown(&mut lifecycle)
    }

    fn remove_mount(&self) -> bool {
        let mut lifecycle = self.lifecycle.lock();
        debug_assert!(lifecycle.active_mounts > 0);
        lifecycle.active_mounts -= 1;
        Self::try_begin_shutdown(&mut lifecycle)
    }

    fn try_begin_shutdown(lifecycle: &mut SuperBlockLifecycle) -> bool {
        if lifecycle.state == SuperBlockLifecycleState::Active
            && lifecycle.active_mounts == 0
            && lifecycle.external_pins == 0
        {
            lifecycle.state = SuperBlockLifecycleState::Dying;
            true
        } else {
            false
        }
    }

    fn finish_shutdown(&self) {
        let mut lifecycle = self.lifecycle.lock();
        debug_assert_eq!(lifecycle.state, SuperBlockLifecycleState::Dying);
        lifecycle.state = SuperBlockLifecycleState::Dead;
        drop(lifecycle);
        self.shutdown_wait.wake_all();
    }

    /// Wait only when this unmount started the final superblock shutdown.
    /// A still-active shared superblock needs no shutdown completion wait.
    fn wait_for_shutdown_if_started(&self) {
        self.shutdown_wait.wait_until(|| {
            let state = self.lifecycle.lock().state;
            (state != SuperBlockLifecycleState::Dying).then_some(())
        });
    }

    fn get_or_create_dentry(
        &self,
        parent: Option<&Arc<VfsDentry>>,
        inode: Arc<dyn IndexNode>,
        name: Option<DName>,
    ) -> Result<Arc<VfsDentry>, SystemError> {
        let child = inode.metadata()?.inode_id;
        let child_generation = inode.inode_generation();
        let key = DentryRegistryKey {
            parent: parent.map(|dentry| dentry.id),
            child,
            child_generation,
            name: name.clone(),
        };
        let mut registry = self.dentry_registry.lock();
        if let Some(dentry) = registry.get(&key).and_then(Weak::upgrade) {
            if !dentry.is_disconnected() {
                return Ok(dentry);
            }
        }
        if !registry.is_empty() && registry.len().is_multiple_of(256) {
            registry.retain(|_, dentry| dentry.strong_count() != 0);
        }
        let dentry = Arc::new(VfsDentry {
            id: DentryId::alloc(),
            inode,
            registry_child: child,
            registry_generation: child_generation,
            mount_gate: Mutex::new(()),
            children_gate: Mutex::new(()),
            mount_edges: AtomicUsize::new(0),
            automount_gate: Mutex::new(()),
            state: Mutex::new(VfsDentryState {
                name,
                parent: parent.cloned(),
                disconnected: false,
            }),
        });
        registry.insert(key, Arc::downgrade(&dentry));
        Ok(dentry)
    }

    fn remove_dentry_key(&self, dentry: &Arc<VfsDentry>) {
        let state = dentry.state.lock();
        let key = DentryRegistryKey {
            parent: state.parent.as_ref().map(|parent| parent.id),
            child: dentry.registry_child,
            child_generation: dentry.registry_generation,
            name: state.name.clone(),
        };
        drop(state);
        self.dentry_registry.lock().remove(&key);
    }

    fn get_registered_dentry(
        &self,
        parent: &Arc<VfsDentry>,
        name: &DName,
        inode: &Arc<dyn IndexNode>,
    ) -> Result<Option<Arc<VfsDentry>>, SystemError> {
        let child = inode.metadata()?.inode_id;
        let key = DentryRegistryKey {
            parent: Some(parent.id),
            child,
            child_generation: inode.inode_generation(),
            name: Some(name.clone()),
        };
        let registry = self.dentry_registry.lock();
        Ok(registry.get(&key).and_then(Weak::upgrade))
    }

    fn disconnect_dentry(&self, dentry: &Arc<VfsDentry>) {
        self.remove_dentry_key(dentry);
        dentry.state.lock().disconnected = true;
    }

    /// Update the alias registry after a successful rename while the caller
    /// holds `dentry_namespace_lock` for writing.
    ///
    /// All old keys are removed before any new key is published.  This is
    /// essential for rename-over and RENAME_EXCHANGE: incrementally rekeying
    /// one alias can otherwise overwrite the other alias' key and the second
    /// removal would then delete the freshly published entry.
    #[allow(clippy::too_many_arguments)]
    fn commit_rename_dentries(
        &self,
        source_parent: &Arc<VfsDentry>,
        old_name: DName,
        target_parent: &Arc<VfsDentry>,
        new_name: DName,
        moved: Option<Arc<VfsDentry>>,
        replaced: Option<Arc<VfsDentry>>,
        exchange: bool,
    ) {
        if moved
            .as_ref()
            .zip(replaced.as_ref())
            .is_some_and(|(left, right)| Arc::ptr_eq(left, right))
        {
            return;
        }

        let moved_child = moved.as_ref().map(|dentry| dentry.registry_child);
        let replaced_child = replaced.as_ref().map(|dentry| dentry.registry_child);

        let mut registry = self.dentry_registry.lock();
        if let Some(child) = moved_child {
            registry.remove(&DentryRegistryKey {
                parent: Some(source_parent.id),
                child,
                child_generation: moved
                    .as_ref()
                    .expect("moved generation requires moved dentry")
                    .registry_generation,
                name: Some(old_name.clone()),
            });
        }
        if let Some(child) = replaced_child {
            registry.remove(&DentryRegistryKey {
                parent: Some(target_parent.id),
                child,
                child_generation: replaced
                    .as_ref()
                    .expect("replacement generation requires replacement dentry")
                    .registry_generation,
                name: Some(new_name.clone()),
            });
        }

        if let (Some(dentry), Some(child)) = (moved, moved_child) {
            {
                let mut state = dentry.state.lock();
                state.parent = Some(target_parent.clone());
                state.name = Some(new_name.clone());
                state.disconnected = false;
            }
            registry.insert(
                DentryRegistryKey {
                    parent: Some(target_parent.id),
                    child,
                    child_generation: dentry.registry_generation,
                    name: Some(new_name),
                },
                Arc::downgrade(&dentry),
            );
        }

        if let (Some(dentry), Some(child)) = (replaced, replaced_child) {
            if exchange {
                {
                    let mut state = dentry.state.lock();
                    state.parent = Some(source_parent.clone());
                    state.name = Some(old_name.clone());
                    state.disconnected = false;
                }
                registry.insert(
                    DentryRegistryKey {
                        parent: Some(source_parent.id),
                        child,
                        child_generation: dentry.registry_generation,
                        name: Some(old_name),
                    },
                    Arc::downgrade(&dentry),
                );
            } else {
                dentry.state.lock().disconnected = true;
            }
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

    /// 返回该 superblock 的匿名设备号，首次需要时才分配。
    ///
    /// 锁覆盖“检查并分配”全过程，确保并发 metadata 查询只消耗一个 minor。
    pub fn unnamed_dev(&self) -> Result<DeviceNumber, SystemError> {
        let mut minor = self.unnamed_dev_minor.lock();
        if let Some(minor) = *minor {
            return Ok(DeviceNumber::new(Major::UNNAMED_MAJOR, minor));
        }

        let allocated = UNNAMED_DEV_ID_ALLOCATOR
            .lock()
            .alloc()
            .ok_or(SystemError::EMFILE)? as u32;
        *minor = Some(allocated);
        Ok(DeviceNumber::new(Major::UNNAMED_MAJOR, allocated))
    }
}

impl Drop for SuperBlockState {
    fn drop(&mut self) {
        if let Some(minor) = self.unnamed_dev_minor.lock().take() {
            UNNAMED_DEV_ID_ALLOCATOR.lock().free(minor as usize);
        }
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
    /// Superblock-wide alias identity shared by every mount projection.
    dentry: Arc<VfsDentry>,
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
        let super_block_state = Arc::new(SuperBlockState::new(mount_flags));
        assert!(
            super_block_state.try_add_external_pin(),
            "a fresh superblock accepts its construction reservation"
        );
        Self::new_with_super_block_state(
            inner_filesystem,
            root_inner_inode,
            None,
            self_mountpoint,
            propagation,
            mnt_ns,
            mount_flags,
            MountStateInit {
                super_block_state,
                mount_source,
                construction_reserved: true,
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_super_block_state(
        inner_filesystem: Arc<dyn FileSystem>,
        root_inner_inode: Option<Arc<dyn IndexNode>>,
        root_dentry: Option<Arc<VfsDentry>>,
        self_mountpoint: Option<Arc<MountFSInode>>,
        propagation: Arc<MountPropagation>,
        mnt_ns: Option<&Arc<MntNamespace>>,
        mount_flags: MountFlags,
        state_init: MountStateInit,
    ) -> Arc<Self> {
        let root_inner_inode = root_inner_inode.unwrap_or_else(|| inner_filesystem.root_inode());
        let root_dentry = root_dentry.unwrap_or_else(|| {
            state_init
                .super_block_state
                .get_or_create_dentry(None, root_inner_inode.clone(), None)
                .expect("a mounted filesystem root requires readable inode metadata")
        });
        let result = Arc::new_cyclic(|self_ref| MountFS {
            inner_filesystem,
            root_inner_inode,
            root_dentry,
            root_inode: Mutex::new(Weak::new()),
            wrapper_cache: Mutex::new(BTreeMap::new()),
            mountpoints: Mutex::new(HashMap::new()),
            tucked_under: AtomicBool::new(false),
            self_mountpoint: RwSem::new(self_mountpoint),
            self_ref: self_ref.clone(),
            namespace: RwSem::new(None),
            propagation,
            mount_id: MountId::alloc(),
            mount_flags: RwSem::new(mount_flags),
            super_block_state: state_init.super_block_state,
            mount_source: RwSem::new(state_init.mount_source),
            locked: AtomicBool::new(false),
            lifecycle: Mutex::new(MountLifecycle {
                state: MountLifecycleState::Constructing,
                external_pins: 0,
                construction_reserved: state_init.construction_reserved,
                propagation_attached: true,
                detached_component: None,
            }),
        });

        if let Some(mnt_ns) = mnt_ns {
            result.set_namespace(Arc::downgrade(mnt_ns));
        }

        register_mounted_superblock(&result);
        result
    }

    pub fn deepcopy(
        &self,
        self_mountpoint: Option<Arc<MountFSInode>>,
    ) -> Result<Arc<Self>, SystemError> {
        if !self.super_block_state.try_add_external_pin() {
            return Err(SystemError::ESTALE);
        }
        // Clone propagation state for the new mount copy
        let new_propagation = self.propagation.clone_for_copy();
        let mount_source = self.mount_source();

        let mountfs = Arc::new_cyclic(|self_ref| MountFS {
            inner_filesystem: self.inner_filesystem.clone(),
            root_inner_inode: self.root_inner_inode.clone(),
            root_dentry: self.root_dentry.clone(),
            root_inode: Mutex::new(Weak::new()),
            wrapper_cache: Mutex::new(BTreeMap::new()),
            mountpoints: Mutex::new(HashMap::new()),
            tucked_under: AtomicBool::new(self.tucked_under.load(Ordering::Acquire)),
            self_mountpoint: RwSem::new(self_mountpoint),
            self_ref: self_ref.clone(),
            namespace: RwSem::new(None),
            propagation: new_propagation,
            mount_id: MountId::alloc(),
            mount_flags: RwSem::new(self.mount_flags()),
            super_block_state: self.super_block_state.clone(),
            mount_source: RwSem::new(mount_source),
            locked: AtomicBool::new(self.locked.load(Ordering::Acquire)),
            lifecycle: Mutex::new(MountLifecycle {
                state: MountLifecycleState::Constructing,
                external_pins: 0,
                construction_reserved: true,
                propagation_attached: true,
                detached_component: None,
            }),
        });

        register_mounted_superblock(&mountfs);
        Ok(mountfs)
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

    /// Attach a mount on top of the exact shared dentry. Callers publishing a
    /// live mount must additionally hold `MOUNT_LIFECYCLE_LOCK`; keeping the
    /// primitive here free of namespace policy also allows detached prepare
    /// trees to be assembled before publication.
    pub fn attach_top(
        &self,
        mountpoint: &Arc<MountFSInode>,
        mount_fs: Arc<MountFS>,
    ) -> Result<(), SystemError> {
        self.attach_top_inner(mountpoint, mount_fs, false)
    }

    pub(crate) fn attach_new_top(
        &self,
        mountpoint: &Arc<MountFSInode>,
        mount_fs: Arc<MountFS>,
    ) -> Result<(), SystemError> {
        self.attach_top_inner(mountpoint, mount_fs, true)
    }

    fn attach_top_inner(
        &self,
        mountpoint: &Arc<MountFSInode>,
        mount_fs: Arc<MountFS>,
        require_connected: bool,
    ) -> Result<(), SystemError> {
        if !Arc::ptr_eq(&mountpoint.mount_fs, &self.self_ref()) {
            return Err(SystemError::EINVAL);
        }
        if mount_fs
            .self_mountpoint()
            .as_ref()
            .is_none_or(|child_mp| !Arc::ptr_eq(child_mp, mountpoint))
        {
            return Err(SystemError::EINVAL);
        }
        let _gate = mountpoint.dentry.mount_gate.lock();
        if require_connected && mountpoint.is_disconnected() {
            return Err(SystemError::ENOENT);
        }
        let key = mountpoint.dentry.id;
        let mut mountpoints = self.mountpoints.lock();
        let stack = mountpoints.entry(key).or_default();
        if stack.iter().any(|child| Arc::ptr_eq(child, &mount_fs)) {
            return Err(SystemError::EEXIST);
        }
        stack.push(mount_fs);
        mountpoint
            .dentry
            .mount_edges
            .fetch_add(1, Ordering::Release);
        Ok(())
    }

    /// Insert a shadow mount below the current visible mount at this dentry.
    pub fn attach_beneath(
        &self,
        mountpoint: &Arc<MountFSInode>,
        mount_fs: Arc<MountFS>,
    ) -> Result<(), SystemError> {
        let cover_mountpoint = mount_fs.mountpoint_root_inode();
        self.attach_beneath_prepared(mountpoint, mount_fs, &cover_mountpoint)
    }

    pub(crate) fn attach_beneath_prepared(
        &self,
        mountpoint: &Arc<MountFSInode>,
        mount_fs: Arc<MountFS>,
        cover_mountpoint: &Arc<MountFSInode>,
    ) -> Result<(), SystemError> {
        if !Arc::ptr_eq(&mountpoint.mount_fs, &self.self_ref())
            || !Arc::ptr_eq(&cover_mountpoint.mount_fs, &mount_fs)
            || cover_mountpoint.dentry.id != mount_fs.root_dentry.id
            || mount_fs
                .self_mountpoint()
                .as_ref()
                .is_none_or(|child_mp| !Arc::ptr_eq(child_mp, mountpoint))
        {
            return Err(SystemError::EINVAL);
        }
        let _gate = mountpoint.dentry.mount_gate.lock();
        let mut mountpoints = self.mountpoints.lock();
        let stack = mountpoints.entry(mountpoint.dentry.id).or_default();
        if stack.iter().any(|child| Arc::ptr_eq(child, &mount_fs)) {
            return Err(SystemError::EEXIST);
        }
        let Some(covered) = stack.pop() else {
            stack.push(mount_fs);
            mountpoint
                .dentry
                .mount_edges
                .fetch_add(1, Ordering::Release);
            return Ok(());
        };
        stack.push(mount_fs.clone());
        drop(mountpoints);

        // Linux mnt_set_mountpoint_beneath(): the propagated mount takes the
        // original edge and the previous topper is reparented onto its root.
        covered.relocate_mountpoint(Some(cover_mountpoint.clone()));
        if let Err(error) = mount_fs.attach_top(cover_mountpoint, covered.clone()) {
            covered.relocate_mountpoint(Some(mountpoint.clone()));
            let mut mountpoints = self.mountpoints.lock();
            let stack = mountpoints
                .get_mut(&mountpoint.dentry.id)
                .expect("tuck-under rollback retains the parent stack");
            let removed = stack.pop();
            debug_assert!(removed.is_some_and(|mount| Arc::ptr_eq(&mount, &mount_fs)));
            stack.push(covered);
            return Err(error);
        }
        covered.tucked_under.store(true, Ordering::Release);
        Ok(())
    }

    /// Detach a propagated tuck-under mount and restore the covering mount to
    /// its original parent edge.
    pub(crate) fn detach_exact_restoring_cover(
        &self,
        mount_fs: &Arc<MountFS>,
    ) -> Result<Arc<MountFS>, SystemError> {
        let cover_mountpoint = mount_fs.mountpoint_root_inode();
        let covered = mount_fs
            .mountpoints
            .lock()
            .get(&cover_mountpoint.dentry.id)
            .and_then(|stack| {
                stack
                    .iter()
                    .find(|child| child.tucked_under.load(Ordering::Acquire))
                    .cloned()
            });
        let Some(covered) = covered else {
            return self.detach_exact(mount_fs);
        };
        self.restore_exact_cover(mount_fs, &cover_mountpoint, &covered)
    }

    /// Transaction rollback variant using the exact topper and root wrapper
    /// captured during prepare. It performs no discovery or allocation.
    pub(crate) fn detach_exact_restoring_prepared_cover(
        &self,
        mount_fs: &Arc<MountFS>,
        cover_mountpoint: Option<&Arc<MountFSInode>>,
        covered: Option<&Arc<MountFS>>,
    ) -> Result<Arc<MountFS>, SystemError> {
        match (cover_mountpoint, covered) {
            (None, None) => self.detach_exact(mount_fs),
            (Some(cover_mountpoint), Some(covered)) => {
                if !covered.tucked_under.load(Ordering::Acquire)
                    || !mount_fs
                        .mountpoints
                        .lock()
                        .get(&cover_mountpoint.dentry.id)
                        .is_some_and(|stack| stack.iter().any(|child| Arc::ptr_eq(child, covered)))
                {
                    return Err(SystemError::ENOENT);
                }
                self.restore_exact_cover(mount_fs, cover_mountpoint, covered)
            }
            _ => Err(SystemError::EINVAL),
        }
    }

    /// Remove one exact mount edge while moving every mount stacked on the
    /// removed mount's root back to the removed edge in chronological order.
    ///
    /// This is the `umount_list()` restoration operation used by propagated
    /// umount. Each tuple identifies whether that root child is restored to the
    /// original parent (`true`) or retained below a lazy-detached parent
    /// (`false`). Commit capacity is reserved during preflight.
    pub(crate) fn detach_exact_restoring_root_children(
        &self,
        mount_fs: &Arc<MountFS>,
        root_children: Vec<(Arc<MountFS>, bool)>,
        reservation: &MountEdgeReservation,
    ) -> Result<Arc<MountFS>, SystemError> {
        let original_mountpoint = mount_fs.self_mountpoint().ok_or(SystemError::EINVAL)?;
        if !Arc::ptr_eq(&original_mountpoint.mount_fs, &self.self_ref())
            || !Arc::ptr_eq(&reservation.parent, &self.self_ref())
            || !Arc::ptr_eq(reservation.mountpoint(), &original_mountpoint)
            || Arc::ptr_eq(&self.self_ref(), mount_fs)
        {
            return Err(SystemError::EINVAL);
        }
        let root_mountpoint = mount_fs.mountpoint_root_inode();
        let original_dentry = original_mountpoint.shared_dentry();
        let root_dentry = root_mountpoint.shared_dentry();

        with_dentry_mount_gates(Some(&original_dentry), Some(&root_dentry), || {
            fn replace_edge(
                parent_mountpoints: &mut HashMap<DentryId, Vec<Arc<MountFS>>>,
                child_mountpoints: &mut HashMap<DentryId, Vec<Arc<MountFS>>>,
                original_mountpoint: &Arc<MountFSInode>,
                root_mountpoint: &Arc<MountFSInode>,
                mount_fs: &Arc<MountFS>,
                root_children: &[(Arc<MountFS>, bool)],
            ) -> Result<(), SystemError> {
                let parent_stack = parent_mountpoints
                    .get(&original_mountpoint.dentry_id())
                    .ok_or(SystemError::ENOENT)?;
                let parent_index = parent_stack
                    .iter()
                    .position(|child| Arc::ptr_eq(child, mount_fs))
                    .ok_or(SystemError::ENOENT)?;
                let actual_root_children = child_mountpoints
                    .get(&root_mountpoint.dentry_id())
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                if actual_root_children.len() != root_children.len()
                    || actual_root_children
                        .iter()
                        .zip(root_children)
                        .any(|(actual, (expected, _))| !Arc::ptr_eq(actual, expected))
                {
                    return Err(SystemError::EBUSY);
                }

                let parent_stack = parent_mountpoints
                    .get_mut(&original_mountpoint.dentry_id())
                    .expect("validated propagated-umount parent stack");
                parent_stack.remove(parent_index);
                let mut offset = 0;
                for (child, restore) in root_children {
                    if !restore {
                        continue;
                    }
                    child.relocate_mountpoint(Some(original_mountpoint.clone()));
                    child.tucked_under.store(false, Ordering::Release);
                    parent_stack.insert(parent_index + offset, child.clone());
                    offset += 1;
                }
                let remove_root_key = if root_children.is_empty() {
                    false
                } else {
                    let root_stack = child_mountpoints
                        .get_mut(&root_mountpoint.dentry_id())
                        .expect("validated propagated-umount root stack");
                    let mut index = 0;
                    root_stack.retain(|_| {
                        let retain = !root_children[index].1;
                        index += 1;
                        retain
                    });
                    root_stack.is_empty()
                };
                if remove_root_key {
                    child_mountpoints.remove(&root_mountpoint.dentry_id());
                }
                Ok(())
            }

            if self.mount_id.data() < mount_fs.mount_id.data() {
                let mut parent_mountpoints = self.mountpoints.lock();
                let mut child_mountpoints = mount_fs.mountpoints.lock();
                replace_edge(
                    &mut parent_mountpoints,
                    &mut child_mountpoints,
                    &original_mountpoint,
                    &root_mountpoint,
                    mount_fs,
                    &root_children,
                )?;
            } else {
                let mut child_mountpoints = mount_fs.mountpoints.lock();
                let mut parent_mountpoints = self.mountpoints.lock();
                replace_edge(
                    &mut parent_mountpoints,
                    &mut child_mountpoints,
                    &original_mountpoint,
                    &root_mountpoint,
                    mount_fs,
                    &root_children,
                )?;
            }

            let restored_count = root_children.iter().filter(|(_, restore)| *restore).count();
            root_dentry
                .mount_edges
                .fetch_sub(restored_count, Ordering::Release);
            if restored_count == 0 {
                original_dentry.mount_edges.fetch_sub(1, Ordering::Release);
            } else if restored_count > 1 {
                original_dentry
                    .mount_edges
                    .fetch_add(restored_count - 1, Ordering::Release);
            }
            Ok(mount_fs.clone())
        })
    }

    fn restore_exact_cover(
        &self,
        mount_fs: &Arc<MountFS>,
        cover_mountpoint: &Arc<MountFSInode>,
        covered: &Arc<MountFS>,
    ) -> Result<Arc<MountFS>, SystemError> {
        let original_mountpoint = mount_fs.self_mountpoint().ok_or(SystemError::EINVAL)?;
        // Preserve the clone-root Vec until restoration completes. This is
        // the rollback counterpart of the prepare-time cover reservation.
        let _cover_reservation = mount_fs.reserve_mount_edge(cover_mountpoint, 0)?;
        mount_fs.detach_exact_keep_slot(covered)?;
        covered.relocate_mountpoint(Some(original_mountpoint.clone()));
        let removed = match self.replace_exact_edge(mount_fs, covered.clone()) {
            Ok(removed) => removed,
            Err(error) => {
                // All objects remain owned; reconstruct the tuck-under
                // topology without allocating from the reserved cover slot.
                covered.relocate_mountpoint(Some(cover_mountpoint.clone()));
                mount_fs.attach_top(cover_mountpoint, covered.clone())?;
                covered.restore_tucked_under(true);
                return Err(error);
            }
        };
        covered.tucked_under.store(false, Ordering::Release);
        Ok(removed)
    }

    /// Replace one exact edge in place, preserving the parent stack's key,
    /// capacity, ordering and mount-edge count.
    fn replace_exact_edge(
        &self,
        old: &Arc<MountFS>,
        replacement: Arc<MountFS>,
    ) -> Result<Arc<MountFS>, SystemError> {
        let mountpoint = old.self_mountpoint().ok_or(SystemError::EINVAL)?;
        if !Arc::ptr_eq(&mountpoint.mount_fs, &self.self_ref())
            || replacement
                .self_mountpoint()
                .as_ref()
                .is_none_or(|replacement_mp| !Arc::ptr_eq(replacement_mp, &mountpoint))
        {
            return Err(SystemError::EINVAL);
        }
        let _gate = mountpoint.dentry.mount_gate.lock();
        let mut mountpoints = self.mountpoints.lock();
        let stack = mountpoints
            .get_mut(&mountpoint.dentry.id)
            .ok_or(SystemError::ENOENT)?;
        let index = stack
            .iter()
            .position(|child| Arc::ptr_eq(child, old))
            .ok_or(SystemError::ENOENT)?;
        Ok(core::mem::replace(&mut stack[index], replacement))
    }

    pub(crate) fn detach_exact_keep_slot(
        &self,
        mount_fs: &Arc<MountFS>,
    ) -> Result<Arc<MountFS>, SystemError> {
        let mountpoint = mount_fs.self_mountpoint().ok_or(SystemError::EINVAL)?;
        if !Arc::ptr_eq(&mountpoint.mount_fs, &self.self_ref()) {
            return Err(SystemError::EINVAL);
        }
        let _gate = mountpoint.dentry.mount_gate.lock();
        let key = mountpoint.dentry.id;
        let mut mountpoints = self.mountpoints.lock();
        let stack = mountpoints.get_mut(&key).ok_or(SystemError::ENOENT)?;
        let index = stack
            .iter()
            .position(|child| Arc::ptr_eq(child, mount_fs))
            .ok_or(SystemError::ENOENT)?;
        let removed = stack.remove(index);
        mountpoint
            .dentry
            .mount_edges
            .fetch_sub(1, Ordering::Release);
        Ok(removed)
    }

    pub fn lookup_top(&self, mountpoint: &Arc<MountFSInode>) -> Option<Arc<MountFS>> {
        self.mountpoints
            .lock()
            .get(&mountpoint.dentry.id)
            .and_then(|stack| stack.last().cloned())
    }

    pub fn children_at(&self, mountpoint: &Arc<MountFSInode>) -> Vec<Arc<MountFS>> {
        self.mountpoints
            .lock()
            .get(&mountpoint.dentry.id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn detach_exact(&self, mount_fs: &Arc<MountFS>) -> Result<Arc<MountFS>, SystemError> {
        let mountpoint = mount_fs.self_mountpoint().ok_or(SystemError::EINVAL)?;
        if !Arc::ptr_eq(&mountpoint.mount_fs, &self.self_ref()) {
            return Err(SystemError::EINVAL);
        }
        let _gate = mountpoint.dentry.mount_gate.lock();
        let key = mountpoint.dentry.id;
        let mut mountpoints = self.mountpoints.lock();
        let stack = mountpoints.get_mut(&key).ok_or(SystemError::ENOENT)?;
        let index = stack
            .iter()
            .position(|child| Arc::ptr_eq(child, mount_fs))
            .ok_or(SystemError::ENOENT)?;
        let removed = stack.remove(index);
        mountpoint
            .dentry
            .mount_edges
            .fetch_sub(1, Ordering::Release);
        if stack.is_empty() {
            mountpoints.remove(&key);
        }
        Ok(removed)
    }

    pub fn mount_children(&self) -> Vec<Arc<MountFS>> {
        self.mountpoints
            .lock()
            .values()
            .flat_map(|stack| stack.iter().cloned())
            .collect()
    }

    pub fn mountpoints(&self) -> MutexGuard<'_, HashMap<DentryId, Vec<Arc<MountFS>>>> {
        self.mountpoints.lock()
    }

    pub fn propagation(&self) -> Arc<MountPropagation> {
        self.propagation.clone()
    }

    /// Reserve the HashMap key and stack capacity needed by a future exact
    /// edge publication. Callers hold `MOUNT_LIFECYCLE_LOCK`, so no topology
    /// writer can consume the reserved slot before commit.
    pub(crate) fn reserve_mount_edge(
        &self,
        mountpoint: &Arc<MountFSInode>,
        additional: usize,
    ) -> Result<MountEdgeReservation, SystemError> {
        if !Arc::ptr_eq(&mountpoint.mount_fs, &self.self_ref()) {
            return Err(SystemError::EINVAL);
        }
        let key = mountpoint.dentry.id;
        let mut mountpoints = self.mountpoints.lock();
        if let Some(stack) = mountpoints.get_mut(&key) {
            stack
                .try_reserve(additional)
                .map_err(|_| SystemError::ENOMEM)?;
        } else {
            mountpoints
                .try_reserve(1)
                .map_err(|_| SystemError::ENOMEM)?;
            let mut stack = Vec::new();
            stack
                .try_reserve(additional)
                .map_err(|_| SystemError::ENOMEM)?;
            mountpoints.insert(key, stack);
        }
        Ok(MountEdgeReservation {
            parent: self.self_ref(),
            mountpoint: mountpoint.clone(),
        })
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

    pub(crate) fn lock_mount(&self) {
        self.locked.store(true, Ordering::Release);
    }

    pub(crate) fn unlock_mount(&self) {
        self.locked.store(false, Ordering::Release);
    }

    pub(crate) fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Acquire)
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

    /// Relocate the backlink of a live mount under the topology lock.
    pub(crate) fn relocate_mountpoint(&self, mountpoint: Option<Arc<MountFSInode>>) {
        // tucked_under describes the current parent edge, not the mount
        // object. A relocation starts a normal edge; attach_beneath marks the
        // role again only after its special topology is committed.
        self.tucked_under.store(false, Ordering::Release);
        *self.self_mountpoint.write() = mountpoint;
    }

    pub(crate) fn is_tucked_under(&self) -> bool {
        self.tucked_under.load(Ordering::Acquire)
    }

    pub(crate) fn restore_tucked_under(&self, tucked: bool) {
        self.tucked_under.store(tucked, Ordering::Release);
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
        let mut root_inode = self.root_inode.lock();
        if let Some(inode) = root_inode.upgrade() {
            return inode;
        }

        let inode =
            MountFSInode::from_dentry(self.root_dentry.clone(), self.self_ref.upgrade().unwrap());
        *root_inode = Arc::downgrade(&inode);
        inode
    }

    pub fn inner_filesystem(&self) -> Arc<dyn FileSystem> {
        return self.inner_filesystem.clone();
    }

    pub fn root_inner_inode(&self) -> Arc<dyn IndexNode> {
        self.root_inner_inode.clone()
    }

    pub fn root_dentry(&self) -> Arc<VfsDentry> {
        self.root_dentry.clone()
    }

    /// Return the mount root as a path relative to the underlying superblock.
    /// This is the fourth field of `/proc/*/mountinfo`; unlike an ordinary
    /// absolute path it must not cross this mount's parent edge.
    pub fn root_path(&self) -> Result<String, SystemError> {
        let _path_snapshot = DENTRY_TOPOLOGY_LOCK.read();
        let _namespace_guard = self.super_block_state.dentry_namespace_lock.read();
        self.root_path_from_snapshot()
    }

    /// Snapshot-only variant of [`Self::root_path`].  The caller must hold
    /// `DENTRY_TOPOLOGY_LOCK` and the mount lifecycle snapshot.
    pub(crate) fn root_path_from_snapshot(&self) -> Result<String, SystemError> {
        let mut current = self.root_dentry.clone();
        let mut parts: Vec<Arc<String>> = Vec::new();
        let mut deleted = false;
        loop {
            let state = current.state.lock();
            deleted |= state.disconnected;
            let parent = state.parent.clone();
            if let Some(name) = state.name.as_ref() {
                parts.push(name.0.clone());
            }
            drop(state);
            let Some(parent) = parent else {
                break;
            };
            current = parent;
            if parts.len() > 4096 {
                return Err(SystemError::ELOOP);
            }
        }
        parts.reverse();
        let mut path = String::new();
        for part in parts {
            path.push('/');
            path.push_str(&part);
        }
        if path.is_empty() {
            path.push('/');
        }
        if deleted {
            path.push_str("//deleted");
        }
        Ok(path)
    }

    pub fn wrapper_for_dentry(
        &self,
        dentry: Arc<VfsDentry>,
    ) -> Result<Arc<MountFSInode>, SystemError> {
        let _namespace_guard = self.super_block_state.dentry_namespace_lock.read();
        if !dentry_is_descendant_of(&dentry, &self.root_dentry) {
            return Err(SystemError::EXDEV);
        }
        Ok(MountFSInode::from_dentry(dentry, self.self_ref()))
    }

    /// Project an already-existing topology edge while copying a namespace.
    /// Existing hidden edges must be retained even if a cross-namespace rename
    /// moved their dentry outside this bind mount's currently reachable root.
    pub(crate) fn wrapper_for_existing_edge(&self, dentry: Arc<VfsDentry>) -> Arc<MountFSInode> {
        MountFSInode::from_dentry(dentry, self.self_ref())
    }

    pub fn self_ref(&self) -> Arc<Self> {
        self.self_ref.upgrade().unwrap()
    }

    /// Publish a fully attached mount into the shared-superblock lifecycle.
    /// Construction failures before this point require no counter rollback.
    pub(crate) fn activate(&self) -> Result<(), SystemError> {
        let mut lifecycle = self.lifecycle.lock();
        if lifecycle.state != MountLifecycleState::Constructing {
            return Err(SystemError::EINVAL);
        }
        self.super_block_state
            .activate_mount(lifecycle.construction_reserved)?;
        lifecycle.construction_reserved = false;
        lifecycle.state = MountLifecycleState::Live;
        Ok(())
    }

    /// Remove one published mount from superblock activity exactly once.
    /// This performs no filesystem I/O; final shutdown is delegated to workqueue.
    pub(crate) fn deactivate(&self) {
        let (should_remove, release_construction, should_leave_propagation) = {
            let mut lifecycle = self.lifecycle.lock();
            let should_leave_propagation = lifecycle.propagation_attached;
            lifecycle.propagation_attached = false;
            let (should_remove, release_construction) = match lifecycle.state {
                MountLifecycleState::Constructing => {
                    lifecycle.state = MountLifecycleState::Detached;
                    let reserved = lifecycle.construction_reserved;
                    lifecycle.construction_reserved = false;
                    (false, reserved)
                }
                MountLifecycleState::Live
                | MountLifecycleState::Detaching
                | MountLifecycleState::DetachedConnected => {
                    lifecycle.state = MountLifecycleState::Detached;
                    lifecycle.detached_component = None;
                    (true, false)
                }
                MountLifecycleState::Detached => (false, false),
            };
            (
                should_remove,
                release_construction,
                should_leave_propagation,
            )
        };
        if should_leave_propagation {
            detach_mount_propagation(&self.self_ref());
        }
        if release_construction && self.super_block_state.remove_external_pin() {
            Self::schedule_final_shutdown(self.self_ref());
        }
        if should_remove && self.super_block_state.remove_mount() {
            Self::schedule_final_shutdown(self.self_ref());
        }
    }

    /// Record that a prepared batch graph commit already removed this mount's
    /// propagation relationships. Later lifecycle teardown must not repeat it.
    pub(crate) fn mark_propagation_detached(&self) {
        self.lifecycle.lock().propagation_attached = false;
    }

    fn detach_propagation_once(&self) {
        let should_detach = {
            let mut lifecycle = self.lifecycle.lock();
            core::mem::replace(&mut lifecycle.propagation_attached, false)
        };
        if should_detach {
            detach_mount_propagation(&self.self_ref());
        }
    }

    pub(crate) fn has_external_pins(&self) -> bool {
        self.lifecycle.lock().external_pins != 0
    }

    pub(crate) fn subtree_has_external_pins(&self) -> bool {
        let mut pending = vec![self.self_ref()];
        while let Some(mount) = pending.pop() {
            if mount.has_external_pins() {
                return true;
            }
            pending.extend(mount.mount_children());
        }
        false
    }

    pub(crate) fn is_live(&self) -> bool {
        self.lifecycle.lock().state == MountLifecycleState::Live
    }

    /// Whether this mount may receive topology edges while serialized by the
    /// lifecycle lock. Detached trees are assembled in `Constructing` before
    /// they are published, so both constructing and live parents are valid.
    pub(crate) fn accepts_topology_edges(&self) -> bool {
        matches!(
            self.lifecycle.lock().state,
            MountLifecycleState::Constructing | MountLifecycleState::Live
        )
    }

    pub(crate) fn try_pin_snapshot(&self) -> Result<MountSnapshotGuard, SystemError> {
        if !self.super_block_state.try_add_external_pin() {
            return Err(SystemError::ESTALE);
        }
        Ok(MountSnapshotGuard {
            mount: self.self_ref(),
        })
    }

    /// Finish lifecycle teardown for a subtree already disconnected from its
    /// parent topology (propagation and namespace destruction paths).
    pub(crate) fn deactivate_disconnected_subtree(root: &Arc<Self>) {
        let mut mounts = Vec::new();
        let mut pending = vec![root.clone()];
        while let Some(mount) = pending.pop() {
            pending.extend(mount.mount_children());
            mounts.push(mount);
        }

        // Drain parent edges before clearing any child's backlink.  Doing this
        // children-first would lose the dentry needed to balance mount_edges.
        for mount in &mounts {
            let edges: Vec<Arc<MountFS>> = mount
                .mountpoints
                .lock()
                .drain()
                .flat_map(|(_, stack)| stack)
                .collect();
            for child in edges {
                if let Some(mountpoint) = child.self_mountpoint() {
                    mountpoint
                        .dentry
                        .mount_edges
                        .fetch_sub(1, Ordering::Release);
                }
            }
        }

        for mount in mounts.into_iter().rev() {
            if let Some(namespace) = mount.namespace() {
                namespace.remove_mount_exact(&mount);
            }
            mount.set_self_mountpoint(None);
            mount.clear_namespace();
            mount.deactivate();
        }
    }

    /// Complete an umount after the root edge has been removed. Lazy detach
    /// keeps locked descendants connected to their detached parent, matching
    /// Linux `disconnect_mount()`. Such retained Arc components are reaped by
    /// workqueue after their last external path/file pin disappears.
    pub(crate) fn finish_disconnected_umount(
        root: &Arc<Self>,
        lazy: bool,
    ) -> Result<(), SystemError> {
        if !lazy {
            Self::deactivate_disconnected_subtree(root);
            return Ok(());
        }

        fn collect(root: &Arc<MountFS>) -> Vec<Arc<MountFS>> {
            let mut mounts = Vec::new();
            let mut pending = vec![root.clone()];
            while let Some(mount) = pending.pop() {
                pending.extend(mount.mount_children());
                mounts.push(mount);
            }
            mounts
        }

        let mounts = collect(root);
        for mount in &mounts {
            let mut lifecycle = mount.lifecycle.lock();
            if lifecycle.state == MountLifecycleState::Live {
                lifecycle.state = MountLifecycleState::Detaching;
            }
        }
        for mount in &mounts {
            if let Some(namespace) = mount.namespace() {
                namespace.remove_mount_exact(mount);
            }
            mount.clear_namespace();
            mount.detach_propagation_once();
        }

        let mut component_roots = vec![root.clone()];
        for mount in mounts.iter().filter(|mount| !Arc::ptr_eq(mount, root)) {
            if mount.is_locked() {
                continue;
            }
            let parent = mount.parent_mount().ok_or(SystemError::EINVAL)?;
            parent.detach_exact_restoring_cover(mount)?;
            mount.set_self_mountpoint(None);
            component_roots.push(mount.clone());
        }

        for component_root in component_roots {
            let members = collect(&component_root);
            if members.len() == 1 {
                component_root.deactivate();
                continue;
            }
            let component = DetachedMountComponent::new(&component_root, &members);
            for member in &members {
                let mut lifecycle = member.lifecycle.lock();
                component.add_initial_pins(lifecycle.external_pins);
                lifecycle.state = MountLifecycleState::DetachedConnected;
                lifecycle.detached_component = Some(component.clone());
            }
            component.finish_initialization();
        }
        Ok(())
    }

    /// Acquire an external semantic reference used by ordinary umount's busy
    /// check. Existing paths may derive another reference after lazy detach;
    /// acquisition stops once final superblock shutdown has begun.
    pub fn try_pin_external(&self) -> Result<MountExternalGuard, SystemError> {
        let _topology = MOUNT_LIFECYCLE_LOCK.lock();
        let mut lifecycle = self.lifecycle.lock();
        let component = match lifecycle.state {
            MountLifecycleState::Constructing | MountLifecycleState::Detaching => {
                return Err(SystemError::EBUSY);
            }
            MountLifecycleState::Detached if lifecycle.external_pins == 0 => {
                return Err(SystemError::ESTALE);
            }
            MountLifecycleState::DetachedConnected => lifecycle.detached_component.clone(),
            MountLifecycleState::Live | MountLifecycleState::Detached => None,
        };
        if component
            .as_ref()
            .is_some_and(|component| !component.try_pin())
        {
            return Err(SystemError::ESTALE);
        }
        if !self.super_block_state.try_add_external_pin() {
            if let Some(component) = component {
                component.unpin();
            }
            return Err(SystemError::ESTALE);
        }
        lifecycle.external_pins += 1;
        Ok(MountExternalGuard {
            mount: self.self_ref(),
        })
    }

    fn derive_external_pin(&self) -> Result<MountExternalGuard, SystemError> {
        let _topology = MOUNT_LIFECYCLE_LOCK.lock();
        let mut lifecycle = self.lifecycle.lock();
        let component = match lifecycle.state {
            MountLifecycleState::Constructing | MountLifecycleState::Detaching => {
                return Err(SystemError::EBUSY);
            }
            MountLifecycleState::Detached if lifecycle.external_pins == 0 => {
                return Err(SystemError::ESTALE);
            }
            MountLifecycleState::DetachedConnected => lifecycle.detached_component.clone(),
            MountLifecycleState::Live | MountLifecycleState::Detached => None,
        };
        if component
            .as_ref()
            .is_some_and(|component| !component.try_pin())
        {
            return Err(SystemError::ESTALE);
        }
        if !self.super_block_state.try_add_external_pin() {
            if let Some(component) = component {
                component.unpin();
            }
            return Err(SystemError::ESTALE);
        }
        lifecycle.external_pins += 1;
        Ok(MountExternalGuard {
            mount: self.self_ref(),
        })
    }

    fn schedule_final_shutdown(mount: Arc<Self>) {
        schedule_work(Work::new(move || mount.finish_final_shutdown()));
    }

    fn finish_final_shutdown(&self) {
        let sb_state = self.super_block_state();
        let _umount_guard = sb_state.umount_write();
        self.inner_filesystem.close_external_inode_admission();
        if let Err(err) = self.sync_filesystem_locked() {
            log::warn!("final superblock sync failed during umount: {:?}", err);
            sb_state.record_wb_error(err);
        }
        if let Err(err) = self.inner_filesystem.shrink_inode_cache_for_shutdown() {
            log::warn!("final inode cache shrink failed: {:?}", err);
            sb_state.record_wb_error(err);
        }
        if let Err(err) = self.inner_filesystem.quiesce_async_inode_work() {
            log::warn!("final asynchronous inode quiesce failed: {:?}", err);
            sb_state.record_wb_error(err);
        }
        let epoch = self.inner_filesystem.seal_eviction_queue();
        if let Err(err) = self.inner_filesystem.drain_evictions_through(epoch) {
            log::warn!("final superblock eviction drain failed: {:?}", err);
            sb_state.record_wb_error(err);
        }
        self.inner_filesystem.on_umount();
        sb_state.finish_shutdown();
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
        Self::umount_subtree_with_mode(&self.self_ref(), false)
    }

    /// Detach a complete mount subtree, deepest mounts first. Ordinary umount
    /// performs one preflight busy check so it cannot partially detach merely
    /// because a descendant owns an external path/file reference.
    pub fn umount_subtree_with_mode(
        root: &Arc<MountFS>,
        lazy: bool,
    ) -> Result<Arc<MountFS>, SystemError> {
        fn collect(root: &Arc<MountFS>) -> Vec<Arc<MountFS>> {
            let mut mounts = Vec::new();
            let mut stack = vec![root.clone()];
            while let Some(mount) = stack.pop() {
                stack.extend(mount.mount_children());
                mounts.push(mount);
            }
            mounts.reverse();
            mounts
        }

        // Potentially sleeping metadata reads happen without the topology lock.
        let mounts = {
            let _topology = MOUNT_LIFECYCLE_LOCK.lock();
            let mounts = collect(root);
            // Linux's ordinary do_umount() leaves non-shrinkable submounts in
            // place and reports EBUSY; only MNT_DETACH disconnects a complete
            // subtree. DragonOS currently has no transient shrinkable mount
            // class, so every visible child is busy here.
            if !lazy && mounts.len() != 1 {
                return Err(SystemError::EBUSY);
            }
            mounts
        };
        let mut super_blocks = Vec::new();
        for mount in &mounts {
            let state = mount.super_block_state();
            if !super_blocks
                .iter()
                .any(|existing| Arc::ptr_eq(existing, &state))
            {
                super_blocks.push(state);
            }
        }
        let mut propagation_keys = Vec::with_capacity(mounts.len());
        for mount in &mounts {
            let mountpoint = mount.self_mountpoint().ok_or(SystemError::EINVAL)?;
            propagation_keys.push((mountpoint.mount_fs.clone(), mountpoint));
        }

        let _topology = MOUNT_LIFECYCLE_LOCK.lock();
        let current = collect(root);
        if current.len() != mounts.len()
            || current
                .iter()
                .zip(mounts.iter())
                .any(|(left, right)| left.mount_id() != right.mount_id())
        {
            return Err(SystemError::EBUSY);
        }
        if root.is_locked() {
            return Err(SystemError::EINVAL);
        }
        if !lazy && root.subtree_has_external_pins() {
            return Err(SystemError::EBUSY);
        }
        for (mount, (parent, mountpoint)) in mounts.iter().zip(propagation_keys.iter()) {
            if !mount.is_live() {
                return Err(SystemError::EINVAL);
            }
            if !lazy && propagation_umount_busy(parent, mountpoint) {
                return Err(SystemError::EBUSY);
            }
        }
        for mount in &mounts {
            mount.lifecycle.lock().state = MountLifecycleState::Detaching;
        }

        let root_mountpoint = root.self_mountpoint().ok_or(SystemError::EINVAL)?;
        if let Err(error) = propagate_umount_sources(&mounts, lazy) {
            for mount in &mounts {
                let mut lifecycle = mount.lifecycle.lock();
                if lifecycle.state == MountLifecycleState::Detaching {
                    lifecycle.state = MountLifecycleState::Live;
                }
            }
            return Err(error);
        }
        root.commit_umount_at(&root_mountpoint, lazy)
            .expect("prepared local umount edge commit cannot fail");
        // Final filesystem shutdown may sleep and may itself need pathname
        // operations. Never wait while holding the topology/admission lock.
        drop(_topology);
        if !lazy {
            for super_block in super_blocks {
                super_block.wait_for_shutdown_if_started();
            }
        }
        Ok(root.clone())
    }

    /// Detach this mount. Lazy detach skips the external-pin busy check and
    /// pre-detach sync; final shared-superblock shutdown remains deferred until
    /// every mount and external pin is gone.
    pub fn umount_with_mode(&self, lazy: bool) -> Result<Arc<MountFS>, SystemError> {
        let _topology = MOUNT_LIFECYCLE_LOCK.lock();
        let mountpoint = self.self_mountpoint().ok_or(SystemError::EINVAL)?;

        if !lazy && propagation_umount_busy(&mountpoint.mount_fs, &mountpoint) {
            return Err(SystemError::EBUSY);
        }

        {
            let mut lifecycle = self.lifecycle.lock();
            if lifecycle.state != MountLifecycleState::Live {
                return Err(SystemError::EINVAL);
            }
            if !lazy && lifecycle.external_pins != 0 {
                return Err(SystemError::EBUSY);
            }
            lifecycle.state = MountLifecycleState::Detaching;
        }
        if let Err(error) = propagate_umount_sources(core::slice::from_ref(&self.self_ref()), lazy)
        {
            self.lifecycle.lock().state = MountLifecycleState::Live;
            return Err(error);
        }
        Ok(self
            .commit_umount_at(&mountpoint, lazy)
            .expect("prepared local umount edge commit cannot fail"))
    }

    fn commit_umount_at(
        &self,
        mountpoint: &Arc<MountFSInode>,
        lazy: bool,
    ) -> Result<Arc<MountFS>, SystemError> {
        let parent = mountpoint.mount_fs();
        let this_mount = self.self_ref();

        let result = parent.detach_exact(&this_mount);

        if result.is_ok() {
            self.self_mountpoint.write().take();
            Self::finish_disconnected_umount(&this_mount, lazy)?;
        } else {
            self.lifecycle.lock().state = MountLifecycleState::Live;
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

        for child_mfs in root.mount_children() {
            stack.push(child_mfs);
        }

        while let Some(mfs) = stack.pop() {
            for child_mfs in mfs.mount_children() {
                stack.push(child_mfs);
            }
            all_descendants.push(mfs);
        }

        // 2. Process in reverse order (deepest child mounts first), ensuring child mounts are cleaned up before parent mounts
        all_descendants.reverse();

        for child_mfs in &all_descendants {
            mntns.remove_mount_exact(child_mfs);
            let _ = child_mfs.umount();
        }

        // 3. Finally unmount the root mount itself
        mntns.remove_mount_exact(root);
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

impl MountExternalGuard {
    pub fn mount(&self) -> Arc<MountFS> {
        self.mount.clone()
    }

    /// Derive another owner from an already valid path. This remains legal
    /// while lazy detach is in progress, matching Linux path_get semantics.
    pub fn derive(&self) -> Result<Self, SystemError> {
        self.mount.derive_external_pin()
    }
}

impl Drop for MountExternalGuard {
    fn drop(&mut self) {
        let component = {
            let mut lifecycle = self.mount.lifecycle.lock();
            debug_assert!(lifecycle.external_pins > 0);
            lifecycle.external_pins -= 1;
            lifecycle.detached_component.clone()
        };
        if self.mount.super_block_state.remove_external_pin() {
            MountFS::schedule_final_shutdown(self.mount.clone());
        }
        if let Some(component) = component {
            component.unpin();
        }
    }
}

impl Drop for MountSnapshotGuard {
    fn drop(&mut self) {
        if self.mount.super_block_state.remove_external_pin() {
            MountFS::schedule_final_shutdown(self.mount.clone());
        }
    }
}

impl MountFSInode {
    pub(crate) fn same_path_ref(&self, other: &MountFSInode) -> bool {
        Arc::ptr_eq(&self.mount_fs, &other.mount_fs) && self.dentry.id == other.dentry.id
    }

    pub(crate) fn is_disconnected(&self) -> bool {
        self.dentry.is_disconnected()
    }

    /// Render this path relative to an exact `(mount, dentry)` root. Returns
    /// `None` when the path is outside that root's mount-tree view.
    pub(crate) fn relative_path_from_snapshot(
        &self,
        root: &Arc<MountFSInode>,
    ) -> Result<Option<String>, SystemError> {
        let mut current = self.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
        let mut parts: Vec<Arc<String>> = Vec::new();
        for _ in 0..=4096 {
            if current.same_path_ref(root) {
                parts.reverse();
                let mut result = String::from("/");
                for (index, part) in parts.iter().enumerate() {
                    if index != 0 {
                        result.push('/');
                    }
                    result.push_str(part.as_str());
                }
                return Ok(Some(result));
            }
            if current.dentry.id == current.mount_fs.root_dentry.id {
                let Some(mountpoint) = current.mount_fs.self_mountpoint() else {
                    return Ok(None);
                };
                current = mountpoint;
                continue;
            }
            let state = current.dentry.state.lock();
            if state.disconnected {
                return Ok(None);
            }
            let name = state.name.as_ref().ok_or(SystemError::ENOENT)?.0.clone();
            let parent = state.parent.clone().ok_or(SystemError::ENOENT)?;
            drop(state);
            parts.push(name);
            current = current.mount_fs.wrapper_for_existing_edge(parent);
        }
        Err(SystemError::ELOOP)
    }

    fn from_dentry(dentry: Arc<VfsDentry>, mount_fs: Arc<MountFS>) -> Arc<Self> {
        let mut cache = mount_fs.wrapper_cache.lock();
        if let Some(cached) = cache.get(&dentry.id).and_then(Weak::upgrade) {
            return cached;
        }
        let inode = Arc::new_cyclic(|self_ref| Self {
            dentry: dentry.clone(),
            mount_fs: mount_fs.clone(),
            self_ref: self_ref.clone(),
        });
        cache.insert(dentry.id, Arc::downgrade(&inode));
        inode
    }

    fn new_child(
        inner_inode: Arc<dyn IndexNode>,
        mount_fs: Arc<MountFS>,
        parent: &Arc<MountFSInode>,
        name: DName,
    ) -> Result<Arc<Self>, SystemError> {
        let dentry = mount_fs.super_block_state.get_or_create_dentry(
            Some(&parent.dentry),
            inner_inode,
            Some(name),
        )?;
        Ok(Self::from_dentry(dentry, mount_fs))
    }

    fn update_move_dentries(
        source: &Arc<MountFSInode>,
        old_name: DName,
        target: &Arc<MountFSInode>,
        new_name: DName,
        exchange: bool,
        old: Option<Arc<VfsDentry>>,
        replaced: Option<Arc<VfsDentry>>,
    ) {
        let sb = &source.mount_fs.super_block_state;
        sb.commit_rename_dentries(
            &source.dentry,
            old_name,
            &target.dentry,
            new_name,
            old,
            replaced,
            exchange,
        );
    }

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
        self.mount_subtree_with_state(inner_fs, root_inner_inode, mount_flags, None, None)
    }

    pub(crate) fn mount_subtree_with_state(
        &self,
        inner_fs: Arc<dyn FileSystem>,
        root_inner_inode: Arc<dyn IndexNode>,
        mount_flags: MountFlags,
        super_block_state: Option<Arc<SuperBlockState>>,
        bind_source: Option<&Arc<MountFS>>,
    ) -> Result<Arc<MountFS>, SystemError> {
        self.mount_subtree_with_root_dentry(
            inner_fs,
            root_inner_inode,
            None,
            mount_flags,
            super_block_state,
            bind_source,
        )
    }

    /// Attach an automatically discovered submount only if the mountpoint is
    /// still uncovered at the publication linearization point.
    pub(crate) fn mount_subtree_with_state_if_vacant(
        &self,
        inner_fs: Arc<dyn FileSystem>,
        root_inner_inode: Arc<dyn IndexNode>,
        mount_flags: MountFlags,
        super_block_state: Option<Arc<SuperBlockState>>,
        bind_source: Option<&Arc<MountFS>>,
    ) -> Result<Arc<MountFS>, SystemError> {
        let prepared = self.prepare_subtree_with_root_dentry(
            inner_fs,
            root_inner_inode,
            None,
            mount_flags,
            super_block_state,
            bind_source,
        )?;
        if let Err(error) = self.publish_prepared_subtree_inner(&prepared, true) {
            MountFS::deactivate_disconnected_subtree(&prepared);
            return Err(error);
        }
        Ok(prepared)
    }

    pub(crate) fn mount_subtree_with_root_dentry(
        &self,
        inner_fs: Arc<dyn FileSystem>,
        root_inner_inode: Arc<dyn IndexNode>,
        root_dentry: Option<Arc<VfsDentry>>,
        mount_flags: MountFlags,
        super_block_state: Option<Arc<SuperBlockState>>,
        bind_source: Option<&Arc<MountFS>>,
    ) -> Result<Arc<MountFS>, SystemError> {
        let prepared = self.prepare_subtree_with_root_dentry(
            inner_fs,
            root_inner_inode,
            root_dentry,
            mount_flags,
            super_block_state,
            bind_source,
        )?;
        if let Err(error) = self.publish_prepared_subtree(&prepared) {
            MountFS::deactivate_disconnected_subtree(&prepared);
            return Err(error);
        }
        Ok(prepared)
    }

    pub(crate) fn prepare_subtree_with_root_dentry(
        &self,
        inner_fs: Arc<dyn FileSystem>,
        root_inner_inode: Arc<dyn IndexNode>,
        root_dentry: Option<Arc<VfsDentry>>,
        mount_flags: MountFlags,
        super_block_state: Option<Arc<SuperBlockState>>,
        bind_source: Option<&Arc<MountFS>>,
    ) -> Result<Arc<MountFS>, SystemError> {
        // Preserve do_add_mount validation order before invoking a filesystem.
        let current_mntns = ProcessManager::current_mntns();
        if !self.mount_fs.is_belongs_to_mntns(&current_mntns) {
            return Err(SystemError::EINVAL);
        }

        let metadata = self.dentry.inode.metadata()?;
        let root_metadata = root_inner_inode.metadata()?;
        let is_dir = metadata.file_type == FileType::Dir;
        let root_is_dir = root_metadata.file_type == FileType::Dir;
        if is_dir != root_is_dir {
            return Err(SystemError::ENOTDIR);
        }

        self.prepare_subtree_with_root_dentry_prevalidated(
            inner_fs,
            root_inner_inode,
            root_dentry,
            mount_flags,
            super_block_state,
            bind_source,
        )
    }

    /// Prepare a detached mount after the caller has validated that the source
    /// root and destination mountpoint have compatible, immutable inode types.
    ///
    /// Unlike [`Self::prepare_subtree_with_root_dentry`], this entry point does
    /// not call into the underlying filesystem. It is suitable for bind-mount
    /// topology snapshot sections, where a FUSE `metadata()` request could
    /// otherwise wait on a daemon that needs the dentry topology write lock.
    pub(crate) fn prepare_subtree_with_root_dentry_prevalidated(
        &self,
        inner_fs: Arc<dyn FileSystem>,
        root_inner_inode: Arc<dyn IndexNode>,
        root_dentry: Option<Arc<VfsDentry>>,
        mount_flags: MountFlags,
        super_block_state: Option<Arc<SuperBlockState>>,
        bind_source: Option<&Arc<MountFS>>,
    ) -> Result<Arc<MountFS>, SystemError> {
        // Linux do_add_mount: the parent mount point must belong to the current mount namespace.
        let current_mntns = ProcessManager::current_mntns();
        if !self.mount_fs.is_belongs_to_mntns(&current_mntns) {
            return Err(SystemError::EINVAL);
        }

        // Keep detached construction private. The destination parent's shared
        // state is revalidated and any new group is allocated atomically at
        // publication; a bind source may install an existing group below.
        let new_propagation = MountPropagation::new_private();

        let (super_block_state, construction_reserved) = match super_block_state {
            Some(super_block_state) => {
                if !super_block_state.try_add_external_pin() {
                    return Err(SystemError::ESTALE);
                }
                (super_block_state, true)
            }
            None => {
                let super_block_state = Arc::new(SuperBlockState::new(mount_flags));
                assert!(
                    super_block_state.try_add_external_pin(),
                    "a fresh superblock accepts its construction reservation"
                );
                (super_block_state, true)
            }
        };
        let new_mount_fs = MountFS::new_with_super_block_state(
            inner_fs,
            Some(root_inner_inode),
            root_dentry,
            Some(self.self_ref.upgrade().unwrap()),
            new_propagation,
            None,
            mount_flags,
            MountStateInit {
                super_block_state,
                mount_source: None,
                construction_reserved,
            },
        );

        if let Some(source) = bind_source {
            inherit_bind_mount_propagation(source, &new_mount_fs);
        }

        Ok(new_mount_fs)
    }

    pub(crate) fn publish_prepared_subtree(
        &self,
        new_mount_fs: &Arc<MountFS>,
    ) -> Result<(), SystemError> {
        self.publish_prepared_subtree_inner(new_mount_fs, false)
    }

    fn publish_prepared_subtree_inner(
        &self,
        new_mount_fs: &Arc<MountFS>,
        require_vacant: bool,
    ) -> Result<(), SystemError> {
        let current_mntns = ProcessManager::current_mntns();
        let mountpoint = self.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
        let _topology = MOUNT_LIFECYCLE_LOCK.lock();
        if !self.mount_fs.is_live() {
            return Err(SystemError::EBUSY);
        }
        if require_vacant && self.mount_fs.lookup_top(&mountpoint).is_some() {
            return Err(SystemError::EEXIST);
        }
        if self.mount_fs.propagation().is_shared() {
            ensure_subtree_shared(new_mount_fs)?;
        }
        let propagation =
            prepare_mount_propagation_locked(&self.mount_fs, &mountpoint, new_mount_fs)?;
        if let Err(error) =
            current_mntns.add_mount_tree(&self.mount_fs, &mountpoint, new_mount_fs.clone())
        {
            abort_mount_propagation(propagation);
            return Err(error);
        }
        if let Err(error) = commit_mount_propagation_locked(propagation) {
            self.mount_fs.detach_exact(new_mount_fs)?;
            MountFS::deactivate_disconnected_subtree(new_mount_fs);
            return Err(error);
        }

        Ok(())
    }

    /// Return the underlying inode wrapped by the mount wrapper.
    #[inline]
    pub(crate) fn underlying_inode(&self) -> Arc<dyn IndexNode> {
        self.dentry.inode.clone()
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
        Ok(self.dentry.id == self.mount_fs.root_dentry.id)
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
            let Some(sub_mountfs) = current.mount_fs.lookup_top(&current) else {
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
        let (inner_inode, mount_inode) = {
            let _children_guard = base.dentry.children_gate.lock();
            let _namespace_guard = base.mount_fs.super_block_state.dentry_namespace_lock.read();
            // Since downward lookups may cross filesystem boundaries, wrap the
            // exact alias before releasing rename serialization.
            let inner_inode = base.dentry.inode.find(name)?;
            let dname = DName::from(name);
            let mount_inode =
                MountFSInode::new_child(inner_inode.clone(), base.mount_fs.clone(), &base, dname)?;
            (inner_inode, mount_inode)
        };
        // FUSE automount may acquire the global mount topology lock; never hold
        // the dentry namespace read lock across that operation.
        if let Some(fuse_node) =
            inner_inode.downcast_arc::<crate::filesystem::fuse::inode::FuseNode>()
        {
            crate::filesystem::fuse::fs::fuse_try_automount_submount(&fuse_node, &mount_inode)?;
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
        }
        if let Some(parent) = self.dentry.state.lock().parent.clone() {
            return self.mount_fs.wrapper_for_dentry(parent);
        }
        Ok(self.self_ref.upgrade().unwrap())
    }

    fn do_absolute_path(&self) -> Result<String, SystemError> {
        self.do_absolute_path_impl(false)
    }

    pub(crate) fn procfs_path(&self) -> Result<String, SystemError> {
        let mut path = self.do_absolute_path_impl(true)?;
        if self.dentry.is_disconnected() {
            path.push_str(" (deleted)");
        }
        Ok(path)
    }

    #[inline(never)]
    fn do_absolute_path_impl(&self, allow_disconnected: bool) -> Result<String, SystemError> {
        // Mount moves and dentry rename take this lock before changing either
        // half of the object chain. Rendering under the same lock prevents a
        // mixed old-parent/new-name namespace path.
        let _topology = MOUNT_LIFECYCLE_LOCK.lock();
        let _path_snapshot = DENTRY_TOPOLOGY_LOCK.read();
        let mut current = self.self_ref.upgrade().unwrap();

        let current_state = current.dentry.state.lock();
        if current_state.disconnected && !allow_disconnected {
            return Err(SystemError::ENOENT);
        }
        drop(current_state);

        let mut path_parts = Vec::new();

        // Note: different filesystems may have independent inode_id spaces, so “global root inode_id” cannot be used as a termination condition.
        // The correct approach is to walk up the mount tree until reaching the “namespace root” (i.e., the rootfs mount where self_mountpoint is None).
        loop {
            // Reached the current namespace root: stop.
            if current.is_mountpoint_root()?
                && current.mount_fs.namespace().is_some_and(|ns| {
                    let ns_root = ns.root_mntfs();
                    Arc::ptr_eq(&current.mount_fs, &ns_root)
                })
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
        MountFSInode::from_dentry(self.dentry.clone(), mount_fs)
    }

    pub fn mount_fs(&self) -> Arc<MountFS> {
        self.mount_fs.clone()
    }

    pub fn dentry_id(&self) -> DentryId {
        self.dentry.id
    }

    pub fn shared_dentry(&self) -> Arc<VfsDentry> {
        self.dentry.clone()
    }

    pub(crate) fn serialize_automount<T>(
        &self,
        operation: impl FnOnce() -> Result<T, SystemError>,
    ) -> Result<T, SystemError> {
        let _guard = self.dentry.automount_gate.lock();
        operation()
    }

    pub fn inode_id(&self) -> Result<InodeId, SystemError> {
        Ok(self.dentry.inode.metadata()?.inode_id)
    }
}

impl IndexNode for MountFSInode {
    fn retain(&self, kind: InodeRetentionKind) -> Result<(), SystemError> {
        self.dentry.inode.retain(kind)
    }

    fn release(&self, kind: InodeRetentionKind) {
        self.dentry.inode.release(kind);
    }

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
        return self.dentry.inode.open(data, flags);
    }

    fn adjust_file_mode_after_open(&self, data: &FilePrivateData, mode: &mut FileMode) {
        self.dentry.inode.adjust_file_mode_after_open(data, mode)
    }

    fn mmap(&self, start: usize, len: usize, offset: usize) -> Result<(), SystemError> {
        return self.dentry.inode.mmap(start, len, offset);
    }

    fn check_mmap_file(
        &self,
        file: &Arc<super::file::File>,
        len: usize,
        offset: usize,
        vm_flags: VmFlags,
    ) -> Result<(), SystemError> {
        self.dentry
            .inode
            .check_mmap_file(file, len, offset, vm_flags)
    }

    fn mmap_vm_flags(&self, file: &Arc<File>, vm_flags: VmFlags) -> Result<VmFlags, SystemError> {
        self.dentry.inode.mmap_vm_flags(file, vm_flags)
    }

    fn mmap_effective_file(
        &self,
        file: &Arc<super::file::File>,
    ) -> Result<Arc<super::file::File>, SystemError> {
        self.dentry.inode.mmap_effective_file(file)
    }

    fn mmap_file(
        &self,
        file: &Arc<super::file::File>,
        start: usize,
        len: usize,
        offset: usize,
        vm_flags: VmFlags,
    ) -> Result<(), SystemError> {
        self.dentry
            .inode
            .mmap_file(file, start, len, offset, vm_flags)
    }

    fn truncate_before_open(&self, flags: &FileFlags) -> bool {
        self.dentry.inode.truncate_before_open(flags)
    }

    fn sync(&self) -> Result<(), SystemError> {
        return self.dentry.inode.sync();
    }

    fn sync_file(
        &self,
        datasync: bool,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        self.dentry.inode.sync_file(datasync, data)
    }

    fn sync_file_range(
        &self,
        start: usize,
        end: usize,
        datasync: bool,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        self.dentry
            .inode
            .sync_file_range(start, end, datasync, data)
    }

    fn write_inode(&self, wbc: &super::WritebackControl) -> Result<(), SystemError> {
        self.dentry.inode.write_inode(wbc)
    }

    fn fadvise(
        &self,
        file: &Arc<super::file::File>,
        offset: i64,
        len: i64,
        advise: i32,
    ) -> Result<usize, SystemError> {
        return self.dentry.inode.fadvise(file, offset, len, advise);
    }

    fn flush_file(
        &self,
        data: MutexGuard<FilePrivateData>,
        lock_owner: u64,
    ) -> Result<(), SystemError> {
        self.dentry.inode.flush_file(data, lock_owner)
    }

    fn close(&self, data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        self.dentry.inode.close(data)
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.ensure_mount_writable()?;
        let _children_guard = self.dentry.children_gate.lock();
        let inner_inode = self
            .dentry
            .inode
            .create_with_data(name, file_type, mode, data)?;
        let _namespace_guard = self
            .mount_fs
            .super_block_state
            .dentry_namespace_lock
            .write();
        let parent = self.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
        return Ok(MountFSInode::new_child(
            inner_inode,
            self.mount_fs.clone(),
            &parent,
            DName::from(name),
        )?);
    }

    fn truncate(&self, len: usize) -> Result<(), SystemError> {
        self.ensure_mount_writable()?;
        return self.dentry.inode.truncate(len);
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        return self.dentry.inode.read_at(offset, len, buf, data);
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.ensure_mount_writable()?;
        return self.dentry.inode.write_at(offset, len, buf, data);
    }

    fn read_direct(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.dentry.inode.read_direct(offset, len, buf, data)
    }

    fn write_direct(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.ensure_mount_writable()?;
        self.dentry.inode.write_direct(offset, len, buf, data)
    }

    #[inline]
    fn fs(&self) -> Arc<dyn FileSystem> {
        // This inode records the mount selected by path lookup.  Operations on
        // the resolved inode (including bind-mount source validation) must not
        // re-resolve a later overmount; pathname lookup itself performs the
        // overmount traversal before returning an inode.
        self.mount_fs.clone()
    }

    #[inline]
    fn mount_flags(&self) -> MountFlags {
        self.mount_fs.mount_flags()
    }

    #[inline]
    fn as_any_ref(&self) -> &dyn core::any::Any {
        return self.dentry.inode.as_any_ref();
    }

    #[inline]
    fn metadata(&self) -> Result<super::Metadata, SystemError> {
        let mut md = self.dentry.inode.metadata()?;

        // Filesystems without a real device share one lazily allocated st_dev across all
        // views of the same superblock (including bind mounts and namespace copies).
        if md.dev_id == 0 {
            md.dev_id = self.mount_fs.super_block_state.unnamed_dev()?.data() as usize;
        }

        Ok(md)
    }

    fn inode_generation(&self) -> u64 {
        self.dentry.registry_generation
    }

    #[inline]
    fn set_metadata(&self, metadata: &super::Metadata) -> Result<(), SystemError> {
        self.ensure_mount_writable()?;
        return self.dentry.inode.set_metadata(metadata);
    }

    #[inline]
    fn set_metadata_masked(
        &self,
        metadata: &super::Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), SystemError> {
        self.ensure_mount_writable()?;
        self.dentry.inode.set_metadata_masked(metadata, mask)
    }

    #[inline]
    fn resize(&self, len: usize) -> Result<(), SystemError> {
        self.ensure_mount_writable()?;
        return self.dentry.inode.resize(len);
    }

    #[inline]
    fn resize_with_lock_owner(&self, len: usize, lock_owner: u64) -> Result<(), SystemError> {
        self.ensure_mount_writable()?;
        return self.dentry.inode.resize_with_lock_owner(len, lock_owner);
    }

    #[inline]
    fn resize_file(
        &self,
        len: usize,
        lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        self.ensure_mount_writable()?;
        return self.dentry.inode.resize_file(len, lock_owner, data);
    }

    #[inline]
    fn resize_with_metadata(
        &self,
        len: usize,
        lock_owner: u64,
        metadata: &super::Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), SystemError> {
        self.ensure_mount_writable()?;
        self.dentry
            .inode
            .resize_with_metadata(len, lock_owner, metadata, mask)
    }

    #[inline]
    fn resize_file_with_metadata(
        &self,
        len: usize,
        lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
        metadata: &super::Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), SystemError> {
        self.ensure_mount_writable()?;
        self.dentry
            .inode
            .resize_file_with_metadata(len, lock_owner, data, metadata, mask)
    }

    #[inline]
    fn fallocate_file(
        &self,
        mode: i32,
        offset: usize,
        len: usize,
        lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        self.ensure_mount_writable()?;
        return self
            .dentry
            .inode
            .fallocate_file(mode, offset, len, lock_owner, data);
    }

    #[inline]
    fn create(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.ensure_mount_writable()?;
        let _children_guard = self.dentry.children_gate.lock();
        let inner_inode = self.dentry.inode.create(name, file_type, mode)?;
        let _namespace_guard = self
            .mount_fs
            .super_block_state
            .dentry_namespace_lock
            .write();
        let parent = self.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
        return Ok(MountFSInode::new_child(
            inner_inode,
            self.mount_fs.clone(),
            &parent,
            DName::from(name),
        )?);
    }

    fn create_and_open(
        &self,
        name: &str,
        mode: InodeMode,
        flags: &FileFlags,
    ) -> Result<PreopenedFile, SystemError> {
        self.ensure_mount_writable()?;
        let children_guard = self.dentry.children_gate.lock();
        let mut preopened = self.dentry.inode.create_and_open(name, mode, flags)?;
        let wrapped = {
            let _namespace_guard = self
                .mount_fs
                .super_block_state
                .dentry_namespace_lock
                .write();
            self.self_ref
                .upgrade()
                .ok_or(SystemError::ENOENT)
                .and_then(|parent| {
                    MountFSInode::new_child(
                        preopened.inode(),
                        self.mount_fs.clone(),
                        &parent,
                        DName::from(name),
                    )
                })
        };
        drop(children_guard);
        preopened.replace_inode(wrapped?);
        Ok(preopened)
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        self.ensure_mount_writable()?;
        let _children_guard = self.dentry.children_gate.lock();
        // Filesystem implementations expect `other` to be an inode of the same concrete filesystem (e.g. LockedExt4Inode).
        // When VFS mount wrapping is enabled, `other` is typically a `MountFSInode`, which causes
        // filesystem-level downcasts to fail and incorrectly return EINVAL.
        //
        // Therefore, before linking, we need to unwrap the mount wrapper (same as move_to).
        let other_inner: Arc<dyn IndexNode> = other
            .clone()
            .downcast_arc::<MountFSInode>()
            .map(|mnt| mnt.dentry.inode.clone())
            .unwrap_or_else(|| other.clone());

        return self.dentry.inode.link(name, &other_inner);
    }

    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.ensure_mount_writable()?;
        let _children_guard = self.dentry.children_gate.lock();
        let inner_inode = self.dentry.inode.symlink(name, target)?;
        let _namespace_guard = self
            .mount_fs
            .super_block_state
            .dentry_namespace_lock
            .write();
        let parent = self.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
        Ok(MountFSInode::new_child(
            inner_inode,
            self.mount_fs.clone(),
            &parent,
            DName::from(name),
        )?)
    }

    /// @brief Delete a file/directory in the mounted filesystem
    #[inline]
    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        let context = DentryMutationContext::new();
        self.unlink_with_context(name, &context)
    }

    fn unlink_with_context(
        &self,
        name: &str,
        context: &DentryMutationContext<'_>,
    ) -> Result<(), SystemError> {
        self.ensure_mount_writable()?;
        let _children_guard = self.dentry.children_gate.lock();
        let _namespace_guard = self
            .mount_fs
            .super_block_state
            .dentry_namespace_lock
            .write();
        let inner = self.dentry.inode.find(name)?;
        let dname = DName::from(name);
        let child =
            self.mount_fs
                .super_block_state
                .get_registered_dentry(&self.dentry, &dname, &inner)?;
        let _mount_gate = child.as_ref().map(|dentry| dentry.mount_gate.lock());
        drop(_namespace_guard);

        // First check if this inode is a mount point; if so, it cannot be deleted
        if child
            .as_ref()
            .is_some_and(|dentry| dentry.is_local_mountpoint())
        {
            return Err(SystemError::EBUSY);
        }
        // Delegate to the inner inode's unlink method to delete this inode
        self.dentry.inode.unlink_with_context(name, context)?;
        context.ensure_locked();
        let _namespace_guard = self
            .mount_fs
            .super_block_state
            .dentry_namespace_lock
            .write();
        drop(inner);
        if let Some(child) = child.as_ref() {
            self.mount_fs.super_block_state.disconnect_dentry(child);
        }
        return Ok(());
    }

    #[inline]
    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        let context = DentryMutationContext::new();
        self.rmdir_with_context(name, &context)
    }

    fn rmdir_with_context(
        &self,
        name: &str,
        context: &DentryMutationContext<'_>,
    ) -> Result<(), SystemError> {
        self.ensure_mount_writable()?;
        let _children_guard = self.dentry.children_gate.lock();
        let _namespace_guard = self
            .mount_fs
            .super_block_state
            .dentry_namespace_lock
            .write();
        let inner = self.dentry.inode.find(name)?;
        let dname = DName::from(name);
        let child =
            self.mount_fs
                .super_block_state
                .get_registered_dentry(&self.dentry, &dname, &inner)?;
        let _mount_gate = child.as_ref().map(|dentry| dentry.mount_gate.lock());
        drop(_namespace_guard);

        // First check if this inode is a mount point; if so, it cannot be deleted
        if child
            .as_ref()
            .is_some_and(|dentry| dentry.is_local_mountpoint())
        {
            return Err(SystemError::EBUSY);
        }
        // Delegate to the inner inode's rmdir method to delete this inode
        self.dentry.inode.rmdir_with_context(name, context)?;
        context.ensure_locked();
        let _namespace_guard = self
            .mount_fs
            .super_block_state
            .dentry_namespace_lock
            .write();
        drop(inner);
        if let Some(child) = child.as_ref() {
            self.mount_fs.super_block_state.disconnect_dentry(child);
        }
        return Ok(());
    }

    #[inline]
    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
        flags: RenameFlags,
    ) -> Result<(), SystemError> {
        let context = DentryMutationContext::new();
        self.move_to_with_context(old_name, target, new_name, flags, &context)
    }

    fn move_to_with_context(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
        flags: RenameFlags,
        context: &DentryMutationContext<'_>,
    ) -> Result<(), SystemError> {
        self.ensure_mount_writable()?;
        // Filesystem implementations generally expect `target` to be an inode
        // of the same concrete FS (e.g. tmpfs' LockedTmpfsInode). When VFS
        // mount wrapping is enabled, `target` is often a `MountFSInode`, which
        // would make FS-level downcasts fail and incorrectly return EINVAL.
        //
        // So we unwrap the mount wrapper before delegating.
        let target_mount = target.clone().downcast_arc::<MountFSInode>();
        let target_inner: Arc<dyn IndexNode> = target_mount
            .as_ref()
            .map(|mnt| mnt.dentry.inode.clone())
            .unwrap_or_else(|| target.clone());
        let target_parent = target_mount
            .as_ref()
            .map(|mount| mount.dentry.clone())
            .unwrap_or_else(|| self.dentry.clone());

        with_dentry_children_gates(&self.dentry, &target_parent, || {
            let namespace_guard = self
                .mount_fs
                .super_block_state
                .dentry_namespace_lock
                .write();
            let (source_dentry, target_dentry) = if let Some(target_mount) = target_mount.as_ref() {
                let sb = &self.mount_fs.super_block_state;
                let source_inner = self.dentry.inode.find(old_name)?;
                let source_dentry =
                    sb.get_registered_dentry(&self.dentry, &DName::from(old_name), &source_inner)?;
                let target_dentry = match target_inner.find(new_name) {
                    Ok(target_child) => sb.get_registered_dentry(
                        &target_mount.dentry,
                        &DName::from(new_name),
                        &target_child,
                    )?,
                    Err(SystemError::ENOENT) => None,
                    Err(error) => return Err(error),
                };
                (source_dentry, target_dentry)
            } else {
                (None, None)
            };
            with_dentry_mount_gates(source_dentry.as_ref(), target_dentry.as_ref(), || {
                // The parent directory gates keep both positive and negative
                // names stable while the per-superblock registry lock is
                // released for potentially long layered copy-up I/O.
                drop(namespace_guard);
                if source_dentry
                    .as_ref()
                    .is_some_and(|dentry| dentry.is_local_mountpoint())
                    || target_dentry
                        .as_ref()
                        .is_some_and(|dentry| dentry.is_local_mountpoint())
                {
                    return Err(SystemError::EBUSY);
                }
                self.dentry.inode.move_to_with_context(
                    old_name,
                    &target_inner,
                    new_name,
                    flags,
                    context,
                )?;
                context.ensure_locked();
                let _namespace_guard = self
                    .mount_fs
                    .super_block_state
                    .dentry_namespace_lock
                    .write();
                if let (Some(source), Some(target)) =
                    (self.self_ref.upgrade(), target_mount.clone())
                {
                    Self::update_move_dentries(
                        &source,
                        DName::from(old_name),
                        &target,
                        DName::from(new_name),
                        flags.contains(RenameFlags::EXCHANGE),
                        source_dentry.clone(),
                        target_dentry.clone(),
                    );
                }
                Ok(())
            })
        })
    }

    fn check_access(
        &self,
        mask: crate::filesystem::vfs::permission::PermissionMask,
    ) -> Result<(), SystemError> {
        self.dentry.inode.check_access(mask)
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
        return self.dentry.inode.get_entry_name(ino);
    }

    #[inline]
    fn get_entry_name_and_metadata(
        &self,
        ino: InodeId,
    ) -> Result<(alloc::string::String, super::Metadata), SystemError> {
        return self.dentry.inode.get_entry_name_and_metadata(ino);
    }

    #[inline]
    fn ioctl(
        &self,
        cmd: u32,
        data: usize,
        private_data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        return self.dentry.inode.ioctl(cmd, data, private_data);
    }

    #[inline]
    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, SystemError> {
        return self.dentry.inode.list();
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
        mntns.move_mount(&from_mfs, &target_mountpoint)?;

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
        let _children_guard = self.dentry.children_gate.lock();
        let inner_inode = self.dentry.inode.mknod(filename, mode, dev_t)?;
        let _namespace_guard = self
            .mount_fs
            .super_block_state
            .dentry_namespace_lock
            .write();
        let parent = self.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
        return Ok(MountFSInode::new_child(
            inner_inode,
            self.mount_fs.clone(),
            &parent,
            DName::from(filename),
        )?);
    }

    #[inline]
    fn special_node(&self) -> Option<super::SpecialNodeData> {
        match self.dentry.inode.special_node() {
            // proc namespace magic links use a self-reference. Preserve the
            // resolved struct-path projection by returning this mount wrapper,
            // not the raw procfs inode. References to any other inode (notably
            // /proc/<pid>/fd/<n>) must remain the original target.
            Some(super::SpecialNodeData::Reference(target))
                if Arc::ptr_eq(&target, &self.dentry.inode) =>
            {
                self.self_ref
                    .upgrade()
                    .map(|inode| super::SpecialNodeData::Reference(inode as Arc<dyn IndexNode>))
            }
            other => other,
        }
    }

    /// If not supported, fall back to getting the filename from the parent directory.
    /// # Performance
    /// DName should be introduced wherever possible;
    /// by default, performance is very poor!
    fn dname(&self) -> Result<DName, SystemError> {
        if self.is_mountpoint_root()? {
            if let Some(inode) = self.mount_fs.self_mountpoint() {
                if let Some(name) = inode.dentry.state.lock().name.clone() {
                    return Ok(name);
                }
                return inode.dentry.inode.dname();
            }
        }
        if let Some(name) = self.dentry.state.lock().name.clone() {
            return Ok(name);
        }
        return self.dentry.inode.dname();
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        return self.do_parent().map(|inode| inode as Arc<dyn IndexNode>);
    }

    fn page_cache(&self) -> Option<Arc<PageCache>> {
        self.dentry.inode.page_cache()
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        self.dentry.inode.as_pollable_inode()
    }

    fn read_sync(&self, offset: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        self.dentry.inode.read_sync(offset, buf)
    }

    fn write_sync(&self, offset: usize, buf: &[u8]) -> Result<usize, SystemError> {
        self.ensure_mount_writable()?;
        self.dentry.inode.write_sync(offset, buf)
    }

    fn getxattr(&self, name: &str, buf: &mut [u8]) -> Result<usize, SystemError> {
        self.dentry.inode.getxattr(name, buf)
    }

    fn setxattr(&self, name: &str, value: &[u8], flags: XattrFlags) -> Result<usize, SystemError> {
        self.ensure_mount_writable()?;
        self.dentry.inode.setxattr(name, value, flags)
    }

    fn listxattr(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        self.dentry.inode.listxattr(buf)
    }

    fn removexattr(&self, name: &str) -> Result<usize, SystemError> {
        self.ensure_mount_writable()?;
        self.dentry.inode.removexattr(name)
    }
}

impl FileSystem for MountFS {
    fn supports_reliable_flush(&self) -> bool {
        self.inner_filesystem.supports_reliable_flush()
    }

    fn support_readahead(&self) -> bool {
        self.inner_filesystem.support_readahead()
    }

    fn fault_before_map_pages(&self) -> bool {
        self.inner_filesystem.fault_before_map_pages()
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
            .map(|mnt| mnt.dentry.inode.clone())
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

    unsafe fn page_mkwrite(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        self.inner_filesystem.page_mkwrite(pfm)
    }

    fn mprotect(&self, old_vm_flags: VmFlags, new_vm_flags: VmFlags) -> Result<(), SystemError> {
        self.inner_filesystem.mprotect(old_vm_flags, new_vm_flags)
    }

    fn vma_close(&self, file: &Arc<super::file::File>, region: VirtRegion, vm_flags: VmFlags) {
        self.inner_filesystem.vma_close(file, region, vm_flags)
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

/// Determine whether the given inode is the root inode of its mounted filesystem.
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
    if let Some(mount_inode) = inode.clone().downcast_arc::<MountFSInode>() {
        if mount_inode.is_mountpoint_root()? && mount_inode.mount_fs().self_mountpoint().is_some() {
            return Err(SystemError::EBUSY);
        }
    }
    return inode.mount(fs, mount_flags);
}
