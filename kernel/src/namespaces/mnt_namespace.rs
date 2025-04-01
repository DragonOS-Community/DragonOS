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
    /// namespace 共有的部分
    ns_common: Arc<NsCommon>,
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
    seq: AtomicU64,
    /// 挂载点的数量
    nr_mounts: u32,
    /// 待处理的挂载点
    pending_mounts: u32,
}

impl Default for MntNamespace {
    fn default() -> Self {
        Self::new()
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
        Self::new()
    }
}

impl FsStruct {
    pub fn new() -> Self {
        Self {
            umask: 0o22,
            root: ROOT_INODE(),
            pwd: ROOT_INODE(),
        }
    }
    pub fn set_root(&mut self, inode: Arc<dyn IndexNode>) {
        self.root = inode;
    }
    pub fn set_pwd(&mut self, inode: Arc<dyn IndexNode>) {
        self.pwd = inode;
    }
}

impl Namespace for MntNamespace {
    fn ns_common_to_ns(ns_common: Arc<NsCommon>) -> Arc<Self> {
        let ns_common_ptr = Arc::as_ptr(&ns_common);
        // container_of!(ns_common_ptr, MntNamespace, ns_common)
        panic!("not implemented")
    }
}

impl MntNsOperations {
    pub fn new(name: String) -> Self {
        Self {
            name,
            clone_flags: CloneFlags::CLONE_NEWNS,
        }
    }
}

impl NsOperations for MntNsOperations {
    fn get(&self, pid: crate::process::Pid) -> Option<Arc<NsCommon>> {
        let pcb = ProcessManager::find(pid);
        pcb.map(|pcb| pcb.get_nsproxy().read().mnt_namespace.ns_common.clone())
    }
    // 不存在这个方法
    fn get_parent(&self, _ns_common: Arc<NsCommon>) -> Result<Arc<NsCommon>, SystemError> {
        unreachable!()
    }
    fn install(
        &self,
        nsset: &mut super::NsSet,
        ns_common: Arc<NsCommon>,
    ) -> Result<(), SystemError> {
        let nsproxy = &mut nsset.nsproxy;
        let mnt_ns = MntNamespace::ns_common_to_ns(ns_common);
        if mnt_ns.is_anon_ns() {
            return Err(SystemError::EINVAL);
        }
        nsproxy.mnt_namespace = mnt_ns;

        nsset.fs.lock().set_pwd(ROOT_INODE());
        nsset.fs.lock().set_root(ROOT_INODE());
        Ok(())
    }
    fn owner(&self, ns_common: Arc<NsCommon>) -> Arc<UserNamespace> {
        let mnt_ns = MntNamespace::ns_common_to_ns(ns_common);
        mnt_ns.user_ns.clone()
    }
    fn put(&self, ns_common: Arc<NsCommon>) {
        let pid_ns = MntNamespace::ns_common_to_ns(ns_common);
    }
}
impl MntNamespace {
    pub fn new() -> Self {
        let ns_common = Arc::new(NsCommon::new(Box::new(MntNsOperations::new(
            "mnt".to_string(),
        ))));

        Self {
            ns_common,
            user_ns: Arc::new(UserNamespace::new()),
            ucounts: Arc::new(UCounts::new()),
            root: None,
            mounts: RBTree::new(),
            poll: WaitQueue::default(),
            seq: AtomicU64::new(0),
            nr_mounts: 0,
            pending_mounts: 0,
        }
    }
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
        let ns_common = Arc::new(NsCommon::new(Box::new(MntNsOperations::new(
            "mnt".to_string(),
        ))));
        let seq = AtomicU64::new(0);
        if !anon {
            seq.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        }
        Ok(Self {
            ns_common,
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
