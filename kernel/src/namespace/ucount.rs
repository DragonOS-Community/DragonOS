use core::{
    hash::Hash,
    sync::atomic::{AtomicU32, AtomicUsize},
};

use alloc::sync::Arc;
use hashbrown::HashMap;
use log::warn;

use super::user_namespace::UserNamespace;
use crate::{include::bindings::bindings::uid_t, libs::mutex::Mutex};
use crate::{libs::spinlock, namespace::ucount::rlimit_type::UCOUNT_RLIMIT_COUNTS};
use crate::{libs::spinlock::SpinLock, namespace::ucount::UcountType::UCOUNT_COUNTS};

#[derive(Clone, Copy)]
pub enum UcountType {
    UCOUNT_USER_NAMESPACES = 1,
    UCOUNT_PID_NAMESPACES = 2,
    UCOUNT_UTS_NAMESPACES = 3,
    UCOUNT_IPC_NAMESPACES = 4,
    UCOUNT_NET_NAMESPACES = 5,
    UCOUNT_MNT_NAMESPACES = 6,
    UCOUNT_CGROUP_NAMESPACES = 7,
    UCOUNT_TIME_NAMESPACES = 8,
    UCOUNT_COUNTS = 9,
}

pub enum rlimit_type {
    UCOUNT_RLIMIT_NPROC = 1,
    UCOUNT_RLIMIT_MSGQUEUE = 2,
    UCOUNT_RLIMIT_SIGPENDING = 3,
    UCOUNT_RLIMIT_MEMLOCK = 4,
    UCOUNT_RLIMIT_COUNTS = 5,
}

lazy_static! {
    static ref COUNT_MANAGER: Arc<CountManager> = Arc::new(CountManager::new());
}

pub struct UCounts {
    /// 对应的user_namespace
    ns: Arc<UserNamespace>,
    /// 用户标识符
    uid: uid_t,
    count: AtomicU32,
    ucount: [AtomicU32; UCOUNT_COUNTS as usize],
    rlimit: [AtomicU32; UCOUNT_RLIMIT_COUNTS as usize],
}

impl UCounts {
    fn alloc_ucounts(&self, ns: Arc<UserNamespace>, uid: uid_t) -> Arc<Self> {
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
                ucount: Default::default(),
                rlimit: Default::default(),
            })
        };
        counts.insert(key, uc.clone());
        uc
    }

    pub fn inc_ucounts(
        &self,
        user_ns: Arc<UserNamespace>,
        uid: uid_t,
        ucount_type: UcountType,
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

    fn find_ucounts(user_ns: Arc<UserNamespace>, uid: uid_t) -> Option<Arc<UCounts>> {
        let counts = COUNT_MANAGER.counts.lock();
        let key = UKey { user_ns, uid };
        if let Some(uc) = counts.get(&key) {
            Some(uc.clone())
        } else {
            None
        }
    }

    fn get_ucounts(uc: Arc<UCounts>) {
        let mut counts = COUNT_MANAGER.counts.lock();
        let ukey = UKey {
            user_ns: uc.ns.clone(),
            uid: uc.uid,
        };
        counts.insert(ukey, uc);
    }

    pub fn dec_ucount(uc: Arc<UCounts>, ucount_type: UcountType) {
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
    uid: uid_t,
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
