#![allow(dead_code, unused_variables, unused_imports)]
use core::fmt::Debug;

use crate::filesystem::procfs::ProcFSInode;
use crate::filesystem::vfs::{IndexNode, ROOT_INODE};
use crate::libs::rwlock::RwLock;
use crate::namespaces::user_namespace::UserNamespace;
use crate::process::fork::CloneFlags;
use crate::process::{Pid, ProcessControlBlock, ProcessManager};
use alloc::boxed::Box;
use alloc::sync::Arc;
use system_error::SystemError;

// 目前无credit功能，采用全局静态的user_namespace
lazy_static! {
    pub static ref USER_NS: Arc<UserNamespace> = Arc::new(UserNamespace::new());
}
use super::{create_new_namespaces, NsProxy, NsSet};
pub trait NsOperations: Send + Sync + Debug {
    fn get(&self, pid: Pid) -> Option<Arc<NsCommon>>;
    fn put(&self, ns_common: Arc<NsCommon>);
    fn install(&self, nsset: &mut NsSet, ns_common: Arc<NsCommon>) -> Result<(), SystemError>;
    fn owner(&self, ns_common: Arc<NsCommon>) -> Arc<UserNamespace>;
    fn get_parent(&self, ns_common: Arc<NsCommon>) -> Result<Arc<NsCommon>, SystemError>;
}
#[derive(Debug)]
pub struct NsCommon {
    ops: Box<dyn NsOperations>,
    stashed: Arc<dyn IndexNode>,
}

impl NsCommon {
    pub fn new(ops: Box<dyn NsOperations>) -> Self {
        let inode = ROOT_INODE().find("proc").unwrap_or_else(|_| ROOT_INODE());
        Self {
            ops,
            stashed: inode,
        }
    }
}

pub enum NsType {
    Pid,
    User,
    Uts,
    Ipc,
    Net,
    Mnt,
    Cgroup,
    Time,
}

pub trait Namespace {
    fn ns_common_to_ns(ns_common: Arc<NsCommon>) -> Arc<Self>;
}

pub fn check_unshare_flags(unshare_flags: u64) -> Result<usize, SystemError> {
    let valid_flags = CloneFlags::CLONE_THREAD
        | CloneFlags::CLONE_FS
        | CloneFlags::CLONE_NEWNS
        | CloneFlags::CLONE_SIGHAND
        | CloneFlags::CLONE_VM
        | CloneFlags::CLONE_FILES
        | CloneFlags::CLONE_SYSVSEM
        | CloneFlags::CLONE_NEWUTS
        | CloneFlags::CLONE_NEWIPC
        | CloneFlags::CLONE_NEWNET
        | CloneFlags::CLONE_NEWUSER
        | CloneFlags::CLONE_NEWPID
        | CloneFlags::CLONE_NEWCGROUP;

    if unshare_flags & !valid_flags.bits() != 0 {
        return Err(SystemError::EINVAL);
    }
    Ok(0)
}

pub fn unshare_nsproxy_namespaces(unshare_flags: u64) -> Result<Option<NsProxy>, SystemError> {
    if (unshare_flags
        & (CloneFlags::CLONE_NEWNS.bits()
            | CloneFlags::CLONE_NEWUTS.bits()
            | CloneFlags::CLONE_NEWIPC.bits()
            | CloneFlags::CLONE_NEWNET.bits()
            | CloneFlags::CLONE_NEWPID.bits()
            | CloneFlags::CLONE_NEWCGROUP.bits()))
        == 0
    {
        return Ok(None);
    }
    let current = ProcessManager::current_pid();
    let pcb = ProcessManager::find(current).unwrap();
    let new_nsproxy = create_new_namespaces(unshare_flags, &pcb, USER_NS.clone())?;
    Ok(Some(new_nsproxy))
}

pub fn switch_task_namespace(pcb: Arc<ProcessControlBlock>, new_nsproxy: NsProxy) {
    let ns = pcb.get_nsproxy();
    pcb.set_nsproxy(new_nsproxy);
}

pub fn prepare_nsset(flags: u64) -> Result<NsSet, SystemError> {
    let current = ProcessManager::current_pcb();
    Ok(NsSet {
        flags,
        fs: RwLock::new(current.fs_struct()),
        nsproxy: create_new_namespaces(flags, &current, USER_NS.clone())?,
    })
}

pub fn commit_nsset(nsset: NsSet) {
    let flags = CloneFlags::from_bits_truncate(nsset.flags);
    let current = ProcessManager::current_pcb();
    if flags.contains(CloneFlags::CLONE_NEWNS) {
        let nsset_fs = nsset.fs.read();
        let fs = current.fs_struct_mut();
        fs.set_pwd(nsset_fs.pwd());
        fs.set_root(nsset_fs.root());
    }
    switch_task_namespace(current, nsset.nsproxy); // 转移所有权
}
