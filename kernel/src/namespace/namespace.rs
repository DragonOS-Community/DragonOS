use crate::namespace::user_namespace::UserNamespace;
use crate::process::Pid;
use alloc::boxed::Box;
use alloc::sync::Arc;

pub trait NsOperations: Send + Sync {
    fn get(&self, pid: Pid) -> Arc<NsCommon>;
    fn put(&self, ns_common: Arc<NsCommon>);
    // fn install(nsset : Nsset, ns_common: Arc<NsCommon>) -> u32;
    fn owner(&self, ns_common: Arc<NsCommon>) -> Arc<UserNamespace>;

    fn get_parent(&self, ns_common: Arc<NsCommon>) -> Arc<NsCommon>;
}

pub struct NsCommon {
    ops: Box<dyn NsOperations>,
    // 相关的文件存储逻辑还需要深入考虑
    //inum: u32, //inode 号用于文件的存储
    //stashed: FATDir,
}

impl NsCommon {
    pub fn new(ops: Box<dyn NsOperations>) -> Self {
        Self { ops }
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
