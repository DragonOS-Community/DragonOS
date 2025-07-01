use core::fmt::Debug;

use alloc::sync::{Arc, Weak};

use crate::libs::spinlock::SpinLock;

use super::nsproxy::NsCommon;

lazy_static! {
    pub static ref INIT_USER_NAMESPACE: Arc<UserNamespace> = UserNamespace::new_root();
}

pub struct UserNamespace {
    self_ref: Weak<UserNamespace>,
    inner: SpinLock<InnerUserNamespace>,
}

pub struct InnerUserNamespace {
    nscommon: NsCommon,
}

impl UserNamespace {
    /// 创建root user namespace
    pub fn new_root() -> Arc<Self> {
        Arc::new_cyclic(|self_ref| Self {
            self_ref: self_ref.clone(),
            inner: SpinLock::new(InnerUserNamespace {
                nscommon: NsCommon::default(),
            }),
        })
    }
}

impl Debug for UserNamespace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UserNamespace").finish()
    }
}
