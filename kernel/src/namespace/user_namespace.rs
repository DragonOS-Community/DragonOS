use crate::include::bindings::bindings::{gid_t, uid_t};
use crate::namespace::namespace::NsCommon;
use crate::namespace::ucount::rlimit_type::UCOUNT_RLIMIT_COUNTS;
use crate::namespace::ucount::UCounts;
use crate::namespace::ucount::UcountType::UCOUNT_COUNTS;
use alloc::sync::Arc;

const UID_GID_MAP_MAX_BASE_EXTENTS: usize = 5;

/// 管理用户ID和组ID的映射
struct UidGidMap {
    nr_extents: u32,
    uid_gid_extent: [UidGidExtent; UID_GID_MAP_MAX_BASE_EXTENTS],
}

///区间映射
struct UidGidExtent {
    first: u32,
    lower_first: u32,
    count: u32,
}
pub struct UserNamespace {
    uid_map: UidGidMap,
    gid_mao: UidGidMap,
    progid_map: UidGidMap,
    ///项目ID映射
    parent: Arc<UserNamespace>,
    level: u32,
    owner: uid_t,
    group: gid_t,
    ns_common: Arc<NsCommon>,
    flags: u32,
    ///剩下一个work_struct
    pub ucounts: Option<Arc<UCounts>>,
    pub ucount_max: [u32; UCOUNT_COUNTS as usize],
    pub rlimit_max: [u32; UCOUNT_RLIMIT_COUNTS as usize],
}
