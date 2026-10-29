use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use alloc::sync::Arc;

use crate::cgroup::memory::{accounting::KernelByteKind, object::ObjectAccount, owned::OwnedBox};
use crate::filesystem::vfs::InodeMode;
use crate::filesystem::vfs::{
    inode_lifecycle::InodeRetentionGuard, mount::MountFSInode, utils::ResolvedPath, IndexNode,
    InodeRetentionKind,
};
use crate::libs::casting::DowncastArc;
use crate::libs::rwsem::RwSem;
use crate::mm::allocation::MemoryAllocationError;
use system_error::SystemError;

#[derive(Debug)]
struct PinnedPath {
    inode: Arc<dyn IndexNode>,
    // Declared before the mount guard so the inode eviction request is
    // published before the final external mount pin can seal its queue.
    _inode_retention: InodeRetentionGuard,
    _mount_guard: Option<crate::filesystem::vfs::mount::MountExternalGuard>,
}

impl PinnedPath {
    fn new(inode: Arc<dyn IndexNode>) -> Self {
        let mount_guard = inode.clone().downcast_arc::<MountFSInode>().map(|inode| {
            inode
                .mount_fs()
                .try_pin_external()
                .expect("live process path must pin its mount")
        });
        let inode_retention = InodeRetentionGuard::new(inode.clone(), InodeRetentionKind::Cache)
            .expect("live process path must retain its inode");
        Self {
            inode,
            _mount_guard: mount_guard,
            _inode_retention: inode_retention,
        }
    }

    fn from_resolved(resolved: ResolvedPath) -> Self {
        let (inode, mount_guard, operation_guard) = resolved.into_parts();
        let inode_retention = InodeRetentionGuard::new(inode.clone(), InodeRetentionKind::Cache)
            .expect("resolved process path must retain its inode");
        drop(operation_guard);
        Self {
            inode,
            _mount_guard: mount_guard,
            _inode_retention: inode_retention,
        }
    }

    fn resolved(&self) -> Result<ResolvedPath, system_error::SystemError> {
        let mount_guard = self
            ._mount_guard
            .as_ref()
            .map(|guard| guard.derive())
            .transpose()?;
        ResolvedPath::from_existing_mount(self.inode.clone(), mount_guard)
    }
}

impl Clone for PinnedPath {
    fn clone(&self) -> Self {
        let inode_retention =
            InodeRetentionGuard::new(self.inode.clone(), InodeRetentionKind::Cache)
                .expect("valid process path must remain retainable");
        Self {
            inode: self.inode.clone(),
            _mount_guard: self._mount_guard.as_ref().map(|guard| {
                guard
                    .derive()
                    .expect("valid process path must remain derivable")
            }),
            _inode_retention: inode_retention,
        }
    }
}

#[derive(Debug, Clone)]
struct PathContext {
    root: PinnedPath,
    pwd: PinnedPath,
}

impl PathContext {
    fn new(root: Arc<dyn IndexNode>) -> Self {
        Self {
            root: PinnedPath::new(root.clone()),
            pwd: PinnedPath::new(root),
        }
    }
}

#[derive(Debug)]
pub struct FsStruct {
    body: OwnedBox<FsStructData>,
}

#[derive(Debug)]
struct FsStructData {
    umask: AtomicU32, // 文件权限掩码
    path_context: RwSem<PathContext>,
    users: AtomicUsize,
}

impl FsStruct {
    pub(crate) fn try_new_for(
        root: Arc<dyn IndexNode>,
        creator: &Arc<ObjectAccount>,
    ) -> Result<Arc<Self>, MemoryAllocationError> {
        let storage = OwnedBox::prepare(creator, KernelByteKind::SlabUnreclaimable)?
            .try_allocate()
            .map_err(|_| SystemError::ENOMEM)?;
        let body = storage.init(FsStructData {
            // Linux 常见默认 umask：0022（屏蔽 group/other 的写权限）。
            // 这能保证新建文件默认不对组/其他可写，同时不把所有写权限都屏蔽掉。
            umask: AtomicU32::new((InodeMode::S_IWGRP | InodeMode::S_IWOTH).bits()),
            path_context: RwSem::new(PathContext::new(root)),
            users: AtomicUsize::new(0),
        });
        Ok(Arc::try_new(Self { body }).map_err(|_| SystemError::ENOMEM)?)
    }

    pub(crate) fn try_clone_for(
        source: &Arc<Self>,
        creator: &Arc<ObjectAccount>,
    ) -> Result<Arc<Self>, MemoryAllocationError> {
        let storage = OwnedBox::prepare(creator, KernelByteKind::SlabUnreclaimable)?
            .try_allocate()
            .map_err(|_| SystemError::ENOMEM)?;
        let umask = source.body.umask.load(Ordering::Relaxed);
        let paths = source.body.path_context.read().clone();
        let body = storage.init(FsStructData {
            umask: AtomicU32::new(umask),
            path_context: RwSem::new(paths),
            users: AtomicUsize::new(0),
        });
        Ok(Arc::try_new(Self { body }).map_err(|_| SystemError::ENOMEM)?)
    }

    pub(crate) fn task_users(&self) -> usize {
        self.body.users.load(Ordering::Acquire)
    }

    pub fn umask(&self) -> InodeMode {
        InodeMode::from_bits_truncate(self.body.umask.load(Ordering::SeqCst))
    }

    /// Linux: xchg(&current->fs->umask, mask & S_IRWXUGO)
    pub fn set_umask(&self, mask: InodeMode) -> InodeMode {
        InodeMode::from_bits_truncate(
            self.body
                .umask
                .swap(mask.bits() & InodeMode::S_IRWXUGO.bits(), Ordering::SeqCst),
        )
    }

    pub fn set_root(&self, inode: Arc<dyn IndexNode>) {
        let new = PinnedPath::new(inode);
        let old = core::mem::replace(&mut self.body.path_context.write().root, new);
        drop(old);
    }

    pub fn set_root_resolved(&self, path: ResolvedPath) {
        let new = PinnedPath::from_resolved(path);
        let old = core::mem::replace(&mut self.body.path_context.write().root, new);
        drop(old);
    }

    pub fn set_pwd(&self, inode: Arc<dyn IndexNode>) {
        let new = PinnedPath::new(inode);
        let old = core::mem::replace(&mut self.body.path_context.write().pwd, new);
        drop(old);
    }

    pub fn set_pwd_resolved(&self, path: ResolvedPath) {
        let new = PinnedPath::from_resolved(path);
        let old = core::mem::replace(&mut self.body.path_context.write().pwd, new);
        drop(old);
    }

    pub fn pwd(&self) -> Arc<dyn IndexNode> {
        self.body.path_context.read().pwd.inode.clone()
    }

    pub fn root(&self) -> Arc<dyn IndexNode> {
        self.body.path_context.read().root.inode.clone()
    }

    pub fn pwd_resolved(&self) -> Result<ResolvedPath, system_error::SystemError> {
        self.body.path_context.read().pwd.resolved()
    }

    /// Render the one authoritative cwd from a coherent root/pwd generation.
    /// Keep semantic path owners, but never hold the fs lock while walking
    /// mount/dentry topology or while later copying to user memory.
    pub(crate) fn cwd_path(&self) -> Result<alloc::string::String, SystemError> {
        let paths = self.body.path_context.read().clone();
        let root = paths
            .root
            .inode
            .clone()
            .downcast_arc::<MountFSInode>()
            .ok_or(SystemError::ENOSYS)?;
        let pwd = paths
            .pwd
            .inode
            .clone()
            .downcast_arc::<MountFSInode>()
            .ok_or(SystemError::ENOSYS)?;
        pwd.cwd_path_from(&root)
    }

    pub fn root_resolved(&self) -> Result<ResolvedPath, system_error::SystemError> {
        self.body.path_context.read().root.resolved()
    }

    /// Linux `chroot_fs_refs()` equivalent for one fs_struct. Both comparisons
    /// and replacements are performed under one path-context write lock.
    pub fn replace_root_pwd(&self, old: &ResolvedPath, new: &ResolvedPath) -> bool {
        let old_inode = old.inode();
        let mut paths = self.body.path_context.write();
        let root_hit = same_path_ref(&paths.root.inode, &old_inode);
        let pwd_hit = same_path_ref(&paths.pwd.inode, &old_inode);

        let old_root = root_hit.then(|| {
            core::mem::replace(
                &mut paths.root,
                PinnedPath::from_resolved(new.derive_existing_owner()),
            )
        });
        let old_pwd = pwd_hit.then(|| {
            core::mem::replace(
                &mut paths.pwd,
                PinnedPath::from_resolved(new.derive_existing_owner()),
            )
        });
        drop(paths);
        drop(old_root);
        drop(old_pwd);
        root_hit || pwd_hit
    }
}

/// Exactly one logical fs user for a task slot; observers only hold Arc.
#[derive(Debug)]
pub(crate) struct FsAttachment {
    fs: Option<Arc<FsStruct>>,
}

impl FsAttachment {
    pub(crate) fn new(fs: Arc<FsStruct>) -> Self {
        fs.body
            .users
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |users| {
                users.checked_add(1)
            })
            .expect("fs task-user overflow");
        Self { fs: Some(fs) }
    }
    pub(crate) fn observer(&self) -> Arc<FsStruct> {
        self.fs.as_ref().expect("retired fs attachment").clone()
    }
    pub(crate) fn retire(mut self) -> Arc<FsStruct> {
        let fs = self.fs.take().expect("retired fs attachment");
        Self::detach(&fs);
        fs
    }
    fn detach(fs: &FsStruct) {
        fs.body
            .users
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |users| {
                users.checked_sub(1)
            })
            .expect("fs task-user underflow");
    }
}

impl Drop for FsAttachment {
    fn drop(&mut self) {
        if let Some(fs) = self.fs.as_ref() {
            Self::detach(fs);
        }
    }
}

/// Real body/users/path owners, without publishing a fixture task or mount.
pub(crate) fn run_accounting_selftest() -> Result<bool, MemoryAllocationError> {
    use crate::cgroup::memory::accounting::{MemoryAccount, MemoryPolicy, KERNEL_BYTE_KIND_COUNT};
    let root = MemoryAccount::new(0x776641, None)?;
    let leaf = MemoryAccount::new(0x776642, Some(root.clone()))?;
    let creator = ObjectAccount::new(&leaf)?;
    let parent = crate::process::ProcessManager::current_pcb().fs_struct();
    let users = parent.task_users();
    let refs = crate::process::lock_fs_refs_copy();
    leaf.set_policy(MemoryPolicy {
        max: Some(0),
        ..MemoryPolicy::default()
    });
    let observer = parent.clone();
    if parent.task_users() != users {
        return Ok(false);
    }
    if !matches!(
        FsStruct::try_clone_for(&parent, &creator),
        Err(MemoryAllocationError::Charge(_))
    ) || leaf.snapshot().current != 0
        || parent.task_users() != users
    {
        return Ok(false);
    }
    drop(observer);
    leaf.set_policy(MemoryPolicy::default());
    let private = FsStruct::try_clone_for(&parent, &creator)?;
    drop(refs);
    if private.task_users() != 0
        || private.umask() != parent.umask()
        || !same_path_ref(&private.root(), &parent.root())
        || !same_path_ref(&private.pwd(), &parent.pwd())
        || leaf.snapshot().current == 0
    {
        return Ok(false);
    }
    let attachment = FsAttachment::new(private.clone());
    let second = FsAttachment::new(private.clone());
    let observer = private.clone();
    if private.task_users() != 2 {
        return Ok(false);
    }
    drop(second);
    if private.task_users() != 1 {
        return Ok(false);
    }
    let prior = parent.umask();
    private.set_umask(InodeMode::S_IRWXUGO);
    if parent.umask() != prior {
        return Ok(false);
    }
    drop(attachment);
    if private.task_users() != 0 {
        return Ok(false);
    }
    let weak = Arc::downgrade(&private);
    drop(observer);
    drop(private);
    let result = weak.upgrade().is_none()
        && leaf.snapshot().current == 0
        && root.snapshot().current == 0
        && leaf.snapshot().kernel_bytes == [0; KERNEL_BYTE_KIND_COUNT]
        && parent.task_users() == users;
    drop(weak);
    Ok(result)
}

fn same_path_ref(left: &Arc<dyn IndexNode>, right: &Arc<dyn IndexNode>) -> bool {
    let Some(left_mount) = left.clone().downcast_arc::<MountFSInode>() else {
        return Arc::ptr_eq(left, right);
    };
    let Some(right_mount) = right.clone().downcast_arc::<MountFSInode>() else {
        return false;
    };
    left_mount.same_path_ref(&right_mount)
}
