#![allow(dead_code, unused_variables, unused_imports)]

use alloc::boxed::Box;

use crate::libs::rwlock::RwLock;
use alloc::string::String;
use alloc::string::ToString;

use alloc::vec::Vec;
use system_error::SystemError;

use crate::namespaces::ucount::UCounts;
use crate::process::fork::CloneFlags;
use crate::process::Pid;
use alloc::sync::Arc;

use super::namespace::Namespace;
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
    flags: u32,
    pid: Arc<RwLock<Pid>>,
    pub ucounts: Option<Arc<UCounts>>,
    pub ucount_max: Vec<u32>,
    pub rlimit_max: Vec<u32>,
}

impl Default for UserNamespace {
    fn default() -> Self {
        Self {
            uid_map: UidGidMap::new(),
            gid_map: UidGidMap::new(),
            progid_map: UidGidMap::new(),
            owner: 0,
            level: 0,
            group: 0,
            flags: 1,
            parent: None,
            pid: Arc::new(RwLock::new(Pid::new(1))),
            ucount_max: vec![UCOUNT_MAX; Counts as usize],
            ucounts: None,
            rlimit_max: vec![65535, 10, 32000, 64 * 1024],
        }
    }
}
impl Namespace for UserNamespace {
    fn name(&self) -> String {
        "user".to_string()
    }

    fn clone_flags(&self) -> CloneFlags {
        CloneFlags::CLONE_NEWUSER
    }

    fn get(&self, pid: Pid) -> Option<Arc<Self>> {
        unimplemented!()
    }

    fn put(&self) {
        unimplemented!()
    }

    fn install(nsset: &mut super::NsSet, ns: Arc<Self>) -> Result<(), SystemError> {
        unimplemented!()
    }

    fn owner(&self) -> Arc<UserNamespace> {
        unimplemented!()
    }

    fn get_parent(&self) -> Result<Arc<Self>, SystemError> {
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
