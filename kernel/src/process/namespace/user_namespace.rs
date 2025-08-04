use alloc::sync::{Arc, Weak};
use core::cmp::Ordering;
use core::fmt::Debug;

use crate::libs::spinlock::SpinLock;

use super::nsproxy::NsCommon;
use super::{NamespaceOps, NamespaceType};
use alloc::vec::Vec;

lazy_static! {
    pub static ref INIT_USER_NAMESPACE: Arc<UserNamespace> = UserNamespace::new_root();
}

pub struct UserNamespace {
    parent: Option<Weak<UserNamespace>>,
    nscommon: NsCommon,
    self_ref: Weak<UserNamespace>,
    _inner: SpinLock<InnerUserNamespace>,
}

pub struct InnerUserNamespace {
    _children: Vec<Arc<UserNamespace>>,
}

impl NamespaceOps for UserNamespace {
    fn ns_common(&self) -> &NsCommon {
        &self.nscommon
    }
}

impl UserNamespace {
    /// 创建root user namespace
    fn new_root() -> Arc<Self> {
        Arc::new_cyclic(|self_ref| Self {
            self_ref: self_ref.clone(),
            nscommon: NsCommon::new(0, NamespaceType::User),
            parent: None,
            _inner: SpinLock::new(InnerUserNamespace {
                _children: Vec::new(),
            }),
        })
    }

    /// 获取层级
    pub fn level(&self) -> u32 {
        self.nscommon.level
    }

    /// 检查当前用户命名空间是否是另一个用户命名空间的祖先
    ///
    /// # 参数
    /// * `other` - 要检查的目标用户命名空间
    ///
    /// # 返回值
    /// * `true` - 如果当前命名空间是 `other` 的祖先
    /// * `false` - 如果当前命名空间不是 `other` 的祖先
    ///
    /// # 说明
    /// 该方法通过遍历 `other` 的父命名空间链来判断当前命名空间是否为其祖先。
    /// 如果两个命名空间处于同一层级且指向同一个对象，则认为是祖先关系。
    /// 如果当前命名空间的层级大于目标命名空间，则不可能是祖先关系。
    pub fn is_ancestor_of(&self, other: &Arc<Self>) -> bool {
        let mut current = other.clone();
        let self_level = self.level();
        loop {
            let current_level = current.level();
            match current_level.cmp(&self_level) {
                Ordering::Greater => {
                    if let Some(parent) = current.parent.as_ref().and_then(|p| p.upgrade()) {
                        current = parent;
                        continue;
                    } else {
                        return false;
                    }
                }
                Ordering::Equal => return Arc::ptr_eq(&self.self_ref.upgrade().unwrap(), &current),
                Ordering::Less => return false,
            }
        }
    }
}

impl Debug for UserNamespace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UserNamespace").finish()
    }
}
