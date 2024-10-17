#![allow(dead_code, unused_variables, unused_imports)]
use core::fmt::Debug;

use crate::filesystem::vfs::{IndexNode, ROOT_INODE};
use crate::namespaces::user_namespace::UserNamespace;
use crate::process::Pid;
use alloc::boxed::Box;
use alloc::sync::Arc;
use system_error::SystemError;

// 目前无credit功能，采用全局静态的user_namespace
lazy_static! {
    pub static ref USER_NS: Arc<UserNamespace> = Arc::new(UserNamespace::new().unwrap());
}
use super::NsSet;
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
    pub fn new(ops: Box<dyn NsOperations>) -> Result<Self, SystemError> {
        Ok(Self {
            ops,
            stashed: ROOT_INODE().find("proc")?,
        })
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
