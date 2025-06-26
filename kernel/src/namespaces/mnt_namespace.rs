#![allow(dead_code, unused_variables, unused_imports)]
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;

use alloc::boxed::Box;
use alloc::string::ToString;

use alloc::string::String;

use alloc::sync::Arc;
use system_error::SystemError;

use super::namespace::Namespace;
use super::namespace::NsOperations;
use super::ucount::Ucount::MntNamespaces;
use super::{namespace::NsCommon, ucount::UCounts, user_namespace::UserNamespace};
use crate::container_of;
use crate::filesystem::vfs::mount::MountFSInode;
use crate::filesystem::vfs::syscall::ModeType;
use crate::filesystem::vfs::IndexNode;
use crate::filesystem::vfs::InodeId;
use crate::filesystem::vfs::MountFS;
use crate::filesystem::vfs::ROOT_INODE;
use crate::libs::rbtree::RBTree;
use crate::libs::rwlock::RwLock;
use crate::libs::wait_queue::WaitQueue;
use crate::process::fork::CloneFlags;
use crate::process::geteuid::do_geteuid;
use crate::process::ProcessManager;

#[derive(Debug, Clone)]
struct PathContext {
    root: Arc<dyn IndexNode>,
    pwd: Arc<dyn IndexNode>,
}

impl PathContext {
    pub fn new() -> Self {
        Self {
            root: ROOT_INODE(),
            pwd: ROOT_INODE(),
        }
    }
}

#[derive(Debug)]
pub struct FsStruct {
    umask: ModeType, //文件权限掩码
    path_context: RwLock<PathContext>,
}

impl Clone for FsStruct {
    fn clone(&self) -> Self {
        Self {
            umask: self.umask,
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
            umask: ModeType::S_IWUGO,
            path_context: RwLock::new(PathContext::new()),
        }
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
