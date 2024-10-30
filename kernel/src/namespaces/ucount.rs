#![allow(dead_code, unused_variables, unused_imports)]
use alloc::vec::Vec;
use core::{hash::Hash, sync::atomic::AtomicU32};
use system_error::SystemError;

use alloc::sync::Arc;
use hashbrown::HashMap;
use log::warn;

use super::user_namespace::UserNamespace;
use crate::libs::mutex::Mutex;

#[derive(Clone, Copy)]
pub enum Ucount {
    UserNamespaces = 1,
    PidNamespaces = 2,
    UtsNamespaces = 3,
    IpcNamespaces = 4,
    NetNamespaces = 5,
    MntNamespaces = 6,
    CgroupNamespaces = 7,
    TimeNamespaces = 8,
    Counts = 9,
}

pub enum UcountRlimit {
    Nproc = 1,
    Msgqueue = 2,
    Sigpending = 3,
    Memlock = 4,
    Counts = 5,
}

lazy_static! {
    static ref COUNT_MANAGER: Arc<CountManager> = Arc::new(CountManager::new());
}

#[derive(Debug)]
pub struct UCounts {
    /// 对应的user_namespace
    ns: Arc<UserNamespace>,
    /// 用户标识符
    uid: usize,
    count: AtomicU32,
    ucount: Vec<AtomicU32>, //[AtomicU32; UCOUNT_COUNTS as usize],
    rlimit: Vec<AtomicU32>, //[AtomicU32; UCOUNT_RLIMIT_COUNTS as usize],
}

impl Default for UCounts {
    fn default() -> Self {
        Self::new()
    }
}
impl UCounts {
    pub fn new() -> Self {
        Self {
            ns: Arc::new(UserNamespace::new()),
            uid: 0,
            count: AtomicU32::new(1),
            ucount: (0..Ucount::Counts as usize)
                .map(|_| AtomicU32::new(0))
                .collect(),
            rlimit: (0..UcountRlimit::Counts as usize)
                .map(|_| AtomicU32::new(0))
                .collect(),
        }
    }

    fn alloc_ucounts(&self, ns: Arc<UserNamespace>, uid: usize) -> Arc<Self> {
        let mut counts = COUNT_MANAGER.counts.lock();
        let key = UKey {
            user_ns: ns.clone(),
            uid,
        };
        let uc = if let Some(uc) = counts.get(&key) {
            self.count
                .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
            uc.clone()
        } else {
            Arc::new(Self {
                ns,
                uid,
                count: AtomicU32::new(1),
                ucount: (0..Ucount::Counts as usize)
                    .map(|_| AtomicU32::new(0))
                    .collect(),
                rlimit: (0..UcountRlimit::Counts as usize)
                    .map(|_| AtomicU32::new(0))
                    .collect(),
            })
        };
        counts.insert(key, uc.clone());
        uc
    }

    pub fn inc_ucounts(
        &self,
        user_ns: Arc<UserNamespace>,
        uid: usize,
        ucount_type: Ucount,
    ) -> Option<Arc<UCounts>> {
        let uc_type = ucount_type as usize;
        let uc = self.alloc_ucounts(user_ns, uid);
        let mut uc_iter = Some(uc.clone());
        let mut ucounts_add = vec![];
        while let Some(iter) = uc_iter {
            let num = iter.ucount[uc_type].fetch_add(1, core::sync::atomic::Ordering::SeqCst);
            ucounts_add.push(iter.clone());
            // 分配失败回滚
            if num > iter.ns.ucount_max[uc_type] {
                for add_iter in &ucounts_add {
                    add_iter.ucount[uc_type].fetch_sub(1, core::sync::atomic::Ordering::SeqCst);
                }
                return None;
            }
            uc_iter = iter.ns.ucounts.clone();
        }
        return Some(uc);
    }

    fn find_ucounts(user_ns: Arc<UserNamespace>, uid: usize) -> Option<Arc<UCounts>> {
        let counts = COUNT_MANAGER.counts.lock();
        let key = UKey { user_ns, uid };
        counts.get(&key).cloned()
    }

    fn get_ucounts(uc: Arc<UCounts>) {
        let mut counts = COUNT_MANAGER.counts.lock();
        let ukey = UKey {
            user_ns: uc.ns.clone(),
            uid: uc.uid,
        };
        counts.insert(ukey, uc);
    }

    pub fn dec_ucount(uc: Arc<UCounts>, ucount_type: Ucount) {
        let mut uc_iter = Some(uc.clone());
        let uc_type = ucount_type as usize;
        while let Some(iter) = uc_iter {
            let num = iter.ucount[uc_type].fetch_sub(1, core::sync::atomic::Ordering::SeqCst);
            if num == 0 {
                warn!("count has reached zero");
            }
            uc_iter = iter.ns.ucounts.clone();
        }
        Self::put_ucounts(uc);
    }

    fn put_ucounts(uc: Arc<UCounts>) {
        let mut counts = COUNT_MANAGER.counts.lock();
        let key = UKey {
            user_ns: uc.ns.clone(),
            uid: uc.uid,
        };
        counts.remove(&key);
    }
}
struct UKey {
    user_ns: Arc<UserNamespace>,
    uid: usize,
}

impl Hash for UKey {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        let user_ns_ptr = Arc::as_ptr(&self.user_ns);
        user_ns_ptr.hash(state);
        self.uid.hash(state)
    }
}
impl Eq for UKey {}
impl PartialEq for UKey {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.user_ns, &other.user_ns) && self.uid == other.uid
    }
}

struct CountManager {
    counts: Mutex<HashMap<UKey, Arc<UCounts>>>,
}

impl CountManager {
    fn new() -> Self {
        Self {
            counts: Mutex::new(HashMap::new()),
        }
    }
}
