use core::sync::atomic::AtomicU64;

use alloc::boxed::Box;
use alloc::string::ToString;

use alloc::string::String;

use alloc::sync::Arc;
use system_error::SystemError;

use super::namespace::NsOperations;
use super::ucount::UcountType::UCOUNT_MNT_NAMESPACES;
use super::{namespace::NsCommon, ucount::UCounts, user_namespace::UserNamespace};
use crate::filesystem::vfs::mount::MountFSInode;
use crate::filesystem::vfs::InodeId;
use crate::filesystem::vfs::MountFS;
use crate::libs::rbtree::RBTree;
use crate::libs::wait_queue::WaitQueue;
use crate::process::fork::CloneFlags;
use crate::syscall::Syscall;

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
    poll: WaitQueue,
    ///  挂载序列号
    seq: AtomicU64,
    /// 挂载点的数量
    nr_mounts: u32,
    /// 待处理的挂载点
    pending_mounts: u32,
}

struct MntNsOperations {
    name: String,
    clone_flags: CloneFlags,
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
        unimplemented!()
    }
    fn get_parent(&self, ns_common: Arc<NsCommon>) -> Arc<NsCommon> {
        unimplemented!()
    }
    fn install(&self, nsset: Arc<super::NsSet>, ns_common: Arc<NsCommon>) -> u32 {
        unimplemented!()
    }
    fn owner(&self, ns_common: Arc<NsCommon>) -> Arc<UserNamespace> {
        unimplemented!()
    }
    fn put(&self, ns_common: Arc<NsCommon>) {
        unimplemented!()
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
        let ns_common = Arc::new(NsCommon::new(Box::new(MntNsOperations::new(
            "mnt".to_string(),
        )))?);
        let seq = AtomicU64::new(1);
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
            .inc_ucounts(user_ns, Syscall::geteuid()? as u32, UCOUNT_MNT_NAMESPACES))
    }

    pub fn dec_mnt_namespace(&self, uc: Arc<UCounts>) {
        UCounts::dec_ucount(uc, UCOUNT_MNT_NAMESPACES)
    }
}
