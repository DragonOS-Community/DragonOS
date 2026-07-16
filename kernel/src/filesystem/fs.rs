use core::sync::atomic::{AtomicU32, Ordering};

use alloc::sync::Arc;

use crate::filesystem::vfs::InodeMode;
use crate::filesystem::vfs::{
    inode_lifecycle::InodeRetentionGuard, mount::MountFSInode, utils::ResolvedPath, IndexNode,
    InodeRetentionKind,
};
use crate::libs::casting::DowncastArc;
use crate::libs::rwsem::RwSem;
use crate::process::ProcessManager;

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
    pub fn new() -> Self {
        Self {
            root: PinnedPath::new(ProcessManager::current_mntns().root_inode()),
            pwd: PinnedPath::new(ProcessManager::current_mntns().root_inode()),
        }
    }
}

#[derive(Debug)]
pub struct FsStruct {
    umask: AtomicU32, // 文件权限掩码
    path_context: RwSem<PathContext>,
}

impl Clone for FsStruct {
    fn clone(&self) -> Self {
        let current_umask = self.umask.load(Ordering::Relaxed);
        Self {
            umask: AtomicU32::new(current_umask),
            path_context: RwSem::new(self.path_context.read().clone()),
        }
    }
}

impl Default for FsStruct {
    fn default() -> Self {
        Self::new()
    }
}

impl FsStruct {
    pub fn new() -> Self {
        Self {
            // Linux 常见默认 umask：0022（屏蔽 group/other 的写权限）。
            // 这能保证新建文件默认不对组/其他可写，同时不把所有写权限都屏蔽掉。
            umask: AtomicU32::new((InodeMode::S_IWGRP | InodeMode::S_IWOTH).bits()),
            path_context: RwSem::new(PathContext::new()),
        }
    }

    pub fn umask(&self) -> InodeMode {
        InodeMode::from_bits_truncate(self.umask.load(Ordering::SeqCst))
    }

    /// Linux: xchg(&current->fs->umask, mask & S_IRWXUGO)
    pub fn set_umask(&self, mask: InodeMode) -> InodeMode {
        InodeMode::from_bits_truncate(
            self.umask
                .swap(mask.bits() & InodeMode::S_IRWXUGO.bits(), Ordering::SeqCst),
        )
    }

    pub fn set_root(&self, inode: Arc<dyn IndexNode>) {
        self.path_context.write().root = PinnedPath::new(inode);
    }

    pub fn set_root_resolved(&self, path: ResolvedPath) {
        self.path_context.write().root = PinnedPath::from_resolved(path);
    }

    pub fn set_pwd(&self, inode: Arc<dyn IndexNode>) {
        self.path_context.write().pwd = PinnedPath::new(inode);
    }

    pub fn set_pwd_resolved(&self, path: ResolvedPath) {
        self.path_context.write().pwd = PinnedPath::from_resolved(path);
    }

    pub fn pwd(&self) -> Arc<dyn IndexNode> {
        self.path_context.read().pwd.inode.clone()
    }

    pub fn root(&self) -> Arc<dyn IndexNode> {
        self.path_context.read().root.inode.clone()
    }

    pub fn pwd_resolved(&self) -> Result<ResolvedPath, system_error::SystemError> {
        self.path_context.read().pwd.resolved()
    }

    pub fn root_resolved(&self) -> Result<ResolvedPath, system_error::SystemError> {
        self.path_context.read().root.resolved()
    }

    /// Linux `chroot_fs_refs()` equivalent for one fs_struct. Both comparisons
    /// and replacements are performed under one path-context write lock.
    pub fn replace_root_pwd(&self, old: &ResolvedPath, new: &ResolvedPath) -> bool {
        let old_inode = old.inode();
        let mut paths = self.path_context.write();
        let root_hit = same_path_ref(&paths.root.inode, &old_inode);
        let pwd_hit = same_path_ref(&paths.pwd.inode, &old_inode);

        if root_hit {
            paths.root = PinnedPath::from_resolved(new.derive_existing_owner());
        }
        if pwd_hit {
            paths.pwd = PinnedPath::from_resolved(new.derive_existing_owner());
        }
        root_hit || pwd_hit
    }
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
