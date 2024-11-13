#![allow(dead_code, unused_variables, unused_imports)]

use alloc::boxed::Box;

use crate::libs::rwlock::RwLock;
use alloc::string::String;
use alloc::string::ToString;

use alloc::vec::Vec;
use system_error::SystemError;

use crate::namespaces::namespace::NsCommon;
use crate::namespaces::ucount::UCounts;
use crate::process::fork::CloneFlags;
use crate::process::Pid;
use alloc::sync::Arc;

use super::namespace::NsOperations;
use super::ucount::Ucount::Counts;

const UID_GID_MAP_MAX_BASE_EXTENTS: usize = 5;
const UCOUNT_MAX: u32 = 62636;
/// 管理用户ID和组ID的映射
#[allow(dead_code)]
#[derive(Clone, Debug)]
struct UidGidMap {
    nr_extents: u32,
    extent: Vec<UidGidExtent>,
}

///区间映射
#[allow(dead_code)]
#[derive(Clone, Debug)]
struct UidGidExtent {
    first: u32,
    lower_first: u32,
    count: u32,
}
#[derive(Debug)]
pub struct UserNamespace {
    uid_map: UidGidMap,
    gid_map: UidGidMap,
    progid_map: UidGidMap,
    ///项目ID映射
    parent: Option<Arc<UserNamespace>>,
    level: u32,
    owner: usize,
    group: usize,
    ns_common: Arc<NsCommon>,
    flags: u32,
    pid: Arc<RwLock<Pid>>,
    pub ucounts: Option<Arc<UCounts>>,
    pub ucount_max: Vec<u32>, //vec![u32; UCOUNT_COUNTS as usize],
    pub rlimit_max: Vec<u32>, // vec![u32; UCOUNT_RLIMIT_COUNTS as usize],
}

impl Default for UserNamespace {
    fn default() -> Self {
        Self::new()
    }
}
#[derive(Debug)]
struct UserNsOperations {
    name: String,
    clone_flags: CloneFlags,
}
impl UserNsOperations {
    pub fn new(name: String) -> Self {
        Self {
            name,
            clone_flags: CloneFlags::CLONE_NEWUSER,
        }
    }
}
impl NsOperations for UserNsOperations {
    fn get(&self, pid: Pid) -> Option<Arc<NsCommon>> {
        unimplemented!()
    }
    fn get_parent(&self, ns_common: Arc<NsCommon>) -> Result<Arc<NsCommon>, SystemError> {
        unimplemented!()
    }
    fn install(
        &self,
        nsset: &mut super::NsSet,
        ns_common: Arc<NsCommon>,
    ) -> Result<(), SystemError> {
        unimplemented!()
    }
    fn owner(&self, ns_common: Arc<NsCommon>) -> Arc<UserNamespace> {
        unimplemented!()
    }
    fn put(&self, ns_common: Arc<NsCommon>) {
        unimplemented!()
    }
}
impl UidGidMap {
    pub fn new() -> Self {
        Self {
            nr_extents: 1,
            extent: vec![UidGidExtent::new(); UID_GID_MAP_MAX_BASE_EXTENTS],
        }
    }
}

impl UidGidExtent {
    pub fn new() -> Self {
        Self {
            first: 0,
            lower_first: 0,
            count: u32::MAX,
        }
    }
}

impl UserNamespace {
    pub fn new() -> Self {
        Self {
            uid_map: UidGidMap::new(),
            gid_map: UidGidMap::new(),
            progid_map: UidGidMap::new(),
            owner: 0,
            level: 0,
            group: 0,
            flags: 1,
            parent: None,
            ns_common: Arc::new(NsCommon::new(Box::new(UserNsOperations::new(
                "User".to_string(),
            )))),
            pid: Arc::new(RwLock::new(Pid::new(1))),
            ucount_max: vec![UCOUNT_MAX; Counts as usize],
            ucounts: None,
            rlimit_max: vec![65535, 10, 32000, 64 * 1024],
        }
    }
}
