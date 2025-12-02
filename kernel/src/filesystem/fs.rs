use core::sync::atomic::{AtomicU32, Ordering};

use alloc::sync::Arc;

use crate::filesystem::vfs::IndexNode;
use crate::filesystem::vfs::InodeMode;
use crate::libs::rwlock::RwLock;
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
    path_context: RwLock<PathContext>,
}

impl Clone for FsStruct {
    fn clone(&self) -> Self {
        let current_umask = self.umask.load(Ordering::Relaxed);
        Self {
            umask: AtomicU32::new(current_umask),
            path_context: RwLock::new(self.path_context.read().clone()),
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
            umask: AtomicU32::new(InodeMode::S_IWUGO.bits()),
            path_context: RwLock::new(PathContext::new()),
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
