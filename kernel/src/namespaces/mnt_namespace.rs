#![allow(dead_code, unused_variables, unused_imports)]
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;

use alloc::boxed::Box;
use alloc::string::ToString;

use alloc::string::String;

use alloc::sync::Arc;
use system_error::SystemError;

use super::namespace::Namespace;
use super::ucount::Ucount::MntNamespaces;
use super::{ucount::UCounts, user_namespace::UserNamespace};
use crate::filesystem::vfs::mount::MountFSInode;
use crate::filesystem::vfs::IndexNode;
use crate::filesystem::vfs::InodeId;
use crate::filesystem::vfs::MountFS;
use crate::filesystem::vfs::ROOT_INODE;
use crate::libs::rbtree::RBTree;
use crate::libs::wait_queue::WaitQueue;
use crate::process::fork::CloneFlags;
use crate::process::ProcessManager;
use crate::syscall::Syscall;
#[allow(dead_code)]
#[derive(Debug)]
pub struct MntNamespace {
    /// 关联的用户名字空间
    user_ns: Arc<UserNamespace>,
    /// 资源计数器
    ucounts: Arc<UCounts>,
    /// 根文件系统
    root: Option<Arc<MountFS>>,
    /// 红黑树用于挂载所有挂载点
    mounts: RBTree<InodeId, MountFSInode>,
    /// 等待队列
    poll: WaitQueue,
    ///  挂载序列号
    seq: Arc<AtomicU64>,
    /// 挂载点的数量
    nr_mounts: u32,
    /// 待处理的挂载点
    pending_mounts: u32,
}

impl Default for MntNamespace {
    fn default() -> Self {
        Self {
            user_ns: Arc::new(UserNamespace::default()),
            ucounts: Arc::new(UCounts::default()),
            root: None,
            mounts: RBTree::new(),
            poll: WaitQueue::default(),
            seq: Arc::new(AtomicU64::new(0)),
            nr_mounts: 0,
            pending_mounts: 0,
        }
    }
}

#[derive(Debug)]
struct MntNsOperations {
    name: String,
    clone_flags: CloneFlags,
}

/// 使用该结构体的时候加spinlock
#[derive(Clone, Debug)]
pub struct FsStruct {
    umask: u32, //文件权限掩码
    pub root: Arc<dyn IndexNode>,
    pub pwd: Arc<dyn IndexNode>,
}
impl Default for FsStruct {
    fn default() -> Self {
        Self {
            umask: 0o22,
            root: ROOT_INODE(),
            pwd: ROOT_INODE(),
        }
    }
}

impl FsStruct {
    pub fn set_root(&mut self, inode: Arc<dyn IndexNode>) {
        self.root = inode;
    }
    pub fn set_pwd(&mut self, inode: Arc<dyn IndexNode>) {
        self.pwd = inode;
    }
}

impl Namespace for MntNamespace {
    fn name(&self) -> String {
        "mnt".to_string()
    }
    fn get(&self, pid: crate::process::Pid) -> Option<Arc<Self>> {
        ProcessManager::find(pid).map(|pcb| pcb.get_nsproxy().read().mnt_namespace.clone())
    }

    fn clone_flags(&self) -> CloneFlags {
        CloneFlags::CLONE_NEWNS
    }

    fn put(&self) {}

    fn install(nsset: &mut super::NsSet, ns: Arc<Self>) -> Result<(), SystemError> {
        let nsproxy = &mut nsset.nsproxy;
        if ns.is_anon_ns() {
            return Err(SystemError::EINVAL);
        }
        nsproxy.mnt_namespace = ns.clone();

        nsset.fs.lock().set_pwd(ROOT_INODE());
        nsset.fs.lock().set_root(ROOT_INODE());
        Ok(())
    }

    fn owner(&self) -> Arc<UserNamespace> {
        self.user_ns.clone()
    }

    // 不存在这个方法
    fn get_parent(&self) -> Result<Arc<Self>, SystemError> {
        unreachable!()
    }
}
impl MntNamespace {
    /// anon 用来判断是否是匿名的.匿名函数的问题还需要考虑
    pub fn create_mnt_namespace(
        &self,
        user_ns: Arc<UserNamespace>,
        anon: bool,
    ) -> Result<Self, SystemError> {
        let ucounts = self.inc_mnt_namespace(user_ns.clone())?;
        if ucounts.is_none() {
            return Err(SystemError::ENOSPC);
        }
        let ucounts = ucounts.unwrap();
        let seq = Arc::new(AtomicU64::new(0));
        if !anon {
            seq.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        }
        Ok(Self {
            user_ns,
            ucounts,
            root: None,
            mounts: RBTree::new(),
            poll: WaitQueue::default(),
            seq,
            nr_mounts: 0,
            pending_mounts: 0,
        })
    }

    pub fn inc_mnt_namespace(
        &self,
        user_ns: Arc<UserNamespace>,
    ) -> Result<Option<Arc<UCounts>>, SystemError> {
        Ok(self
            .ucounts
            .inc_ucounts(user_ns, Syscall::geteuid()?, MntNamespaces))
    }

    pub fn dec_mnt_namespace(&self, uc: Arc<UCounts>) {
        UCounts::dec_ucount(uc, super::ucount::Ucount::MntNamespaces)
    }
    //判断是不是匿名空间
    pub fn is_anon_ns(&self) -> bool {
        self.seq.load(Ordering::SeqCst) == 0
    }
}
