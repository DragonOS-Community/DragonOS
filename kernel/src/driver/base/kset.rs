use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};

use crate::{
    filesystem::{
        kernfs::KernFSInode,
        sysfs::{AttributeGroup, SysFSOps},
    },
    kdebug,
    libs::rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
    syscall::SystemError,
};

use super::kobject::{
    KObjType, KObject, KObjectManager, KObjectState, KObjectSysFSOps, LockedKObjectState,
};

#[derive(Debug)]
pub struct KSet {
    /// 属于当前kset的kobject
    kobjects: RwLock<Vec<Weak<dyn KObject>>>,
    /// 节点的一些信息
    inner: RwLock<InnerKSet>,
    /// kobject的状态
    kobj_state: LockedKObjectState,
    /// 与父节点有关的一些信息
    parent_data: RwLock<KSetParentData>,
    self_ref: Weak<KSet>,
}

impl KSet {
    pub fn new(name: String) -> Arc<Self> {
        let r = Self {
            kobjects: RwLock::new(Vec::new()),
            inner: RwLock::new(InnerKSet::new(name)),
            kobj_state: LockedKObjectState::new(KObjectState::empty()),
            parent_data: RwLock::new(KSetParentData::new(None, None)),
            self_ref: Weak::default(),
        };

        let r = Arc::new(r);

        unsafe {
            let p = r.as_ref() as *const Self as *mut Self;
            (*p).self_ref = Arc::downgrade(&r);
        }

        return r;
    }

    pub fn register(&self, join_kset: Option<Arc<KSet>>) -> Result<(), SystemError> {
        return KObjectManager::add_kobj(self.self_ref.upgrade().unwrap(), join_kset);
        // todo: 引入uevent之后，发送uevent
    }

    /// 把一个kobject加入到当前kset中。
    ///
    /// 该函数不会修改kobj的parent，需要调用者自己视情况修改。
    ///
    /// ## Panic
    ///
    /// 这个kobject的kset必须是None，否则会panic
    pub fn join(&self, kobj: &Arc<dyn KObject>) {
        kdebug!("join kset: kobj.kset() = {:?}", kobj.kset());
        assert!(kobj.kset().is_none());
        kobj.set_kset(self.self_ref.upgrade());
        self.kobjects.write().push(Arc::downgrade(&kobj));
    }

    /// 把一个kobject从当前kset中移除。
    pub fn leave(&self, kobj: &Arc<dyn KObject>) {
        let mut kobjects = self.kobjects.write();
        let index = kobjects.iter().position(|x| {
            if let Some(x) = x.upgrade() {
                return Arc::ptr_eq(&x, kobj);
            }
            return false;
        });
        if let Some(index) = index {
            let x = kobjects.remove(index);
            let x = x.upgrade().unwrap();
            drop(kobjects);
            x.set_kset(None);
        }
    }

    /// 清除所有已经被释放的kobject
    pub fn cleanup_weak(&self) {
        let mut kobjects = self.kobjects.write();
        kobjects.drain_filter(|x| x.upgrade().is_none());
    }

    pub fn parent_kset(&self) -> Option<Arc<KSet>> {
        return self.parent_data.read().kset.clone();
    }
}

impl KObject for KSet {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner.read().kern_inode.clone()
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner.write().kern_inode = inode;
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.parent_data.read().parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.parent_data.write().parent = parent;
    }

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        self.kobj_state.write()
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        Some(&KSetKObjType)
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.parent_data.read().kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.parent_data.write().kset = kset;
    }

    fn name(&self) -> String {
        return self.inner.read().name.clone();
    }

    fn set_name(&self, name: String) {
        self.inner.write().name = name;
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.kobj_state.write() = state;
    }
}

#[derive(Debug)]
struct KSetParentData {
    parent: Option<Weak<dyn KObject>>,
    kset: Option<Arc<KSet>>,
}

impl KSetParentData {
    fn new(parent: Option<Weak<dyn KObject>>, kset: Option<Arc<KSet>>) -> Self {
        Self { parent, kset }
    }
}

#[derive(Debug)]
struct InnerKSet {
    kern_inode: Option<Arc<KernFSInode>>,
    name: String,
}

impl InnerKSet {
    fn new(name: String) -> Self {
        Self {
            kern_inode: None,
            name,
        }
    }
}

#[derive(Debug)]
pub struct KSetKObjType;

impl KObjType for KSetKObjType {
    fn sysfs_ops(&self) -> Option<&dyn SysFSOps> {
        Some(&KObjectSysFSOps)
    }

    fn attribute_groups(&self) -> Option<&'static [&'static dyn AttributeGroup]> {
        None
    }
}
