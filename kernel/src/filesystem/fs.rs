use core::sync::atomic::{AtomicU32, Ordering};

use alloc::sync::Arc;

use crate::filesystem::vfs::IndexNode;
use crate::filesystem::vfs::InodeMode;
use crate::libs::rwsem::RwSem;
use crate::process::ProcessManager;
#[derive(Debug, Clone)]
struct PathContext {
    root: Arc<dyn IndexNode>,
    pwd: Arc<dyn IndexNode>,
}

impl PathContext {
    pub fn new() -> Self {
        Self {
            root: ProcessManager::current_mntns().root_inode(),
            pwd: ProcessManager::current_mntns().root_inode(),
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
        self.path_context.write().root = inode;
    }

    pub fn set_pwd(&self, inode: Arc<dyn IndexNode>) {
        self.path_context.write().pwd = inode;
    }

    pub fn pwd(&self) -> Arc<dyn IndexNode> {
        self.path_context.read().pwd.clone()
    }

    pub fn root(&self) -> Arc<dyn IndexNode> {
        self.path_context.read().root.clone()
    }
}
