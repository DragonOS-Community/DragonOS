use crate::filesystem::vfs::{IndexNode, ROOT_INODE};
use crate::namespace::user_namespace::UserNamespace;
use crate::process::Pid;
use alloc::boxed::Box;
use alloc::sync::Arc;
use system_error::SystemError;

use super::NsSet;

pub trait NsOperations: Send + Sync {
    fn get(&self, pid: Pid) -> Option<Arc<NsCommon>>;
    fn put(&self, ns_common: Arc<NsCommon>);
    fn install(&self, nsset: Arc<NsSet>, ns_common: Arc<NsCommon>) -> u32;
    fn owner(&self, ns_common: Arc<NsCommon>) -> Arc<UserNamespace>;

    fn get_parent(&self, ns_common: Arc<NsCommon>) -> Arc<NsCommon>;
}

pub struct NsCommon {
    ops: Box<dyn NsOperations>,
    stashed: Arc<dyn IndexNode>,
}

impl NsCommon {
    pub fn new(ops: Box<dyn NsOperations>) -> Result<Self, SystemError> {
        Ok(Self {
            ops,
            stashed: ROOT_INODE().find("proc")?,
        })
    }
}

enum NsType {
    PidNamespace,
    UserNamespace,
    UtsNamespace,
    IpcNamespace,
    NetNamespace,
    MntNamespace,
    CgroupNamespace,
    TimeNamespace,
}
