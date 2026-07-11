use core::sync::atomic::{AtomicU32, Ordering};

use alloc::sync::Arc;

use crate::filesystem::vfs::InodeMode;
use crate::filesystem::vfs::{mount::MountFSInode, utils::ResolvedPath, IndexNode};
use crate::libs::casting::DowncastArc;
use crate::libs::rwsem::RwSem;
use crate::process::ProcessManager;

#[derive(Debug)]
struct PinnedPath {
    inode: Arc<dyn IndexNode>,
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
        Self {
            inode,
            _mount_guard: mount_guard,
        }
    }

    fn from_resolved(resolved: ResolvedPath) -> Self {
        let (inode, mount_guard, operation_guard) = resolved.into_parts();
        drop(operation_guard);
        Self {
            inode,
            _mount_guard: mount_guard,
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
        Self {
            inode: self.inode.clone(),
            _mount_guard: self._mount_guard.as_ref().map(|guard| {
                guard
                    .derive()
                    .expect("valid process path must remain derivable")
            }),
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
}
