use core::{any::Any, fmt::Debug, hash::Hash, ops::Deref};

use alloc::{
    string::String,
    sync::{Arc, Weak},
};
use intertrait::CastFromSync;

use crate::{
    filesystem::{
        kernfs::KernFSInode,
        sysfs::{sysfs_instance, Attribute, AttributeGroup, SysFSOps, SysFSOpsSupport},
    },
    kerror,
    libs::{
        casting::DowncastArc,
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
    },
    syscall::SystemError,
};

use super::kset::KSet;

pub trait KObject: Any + Send + Sync + Debug + CastFromSync {
    fn as_any_ref(&self) -> &dyn core::any::Any;

    /// 设置当前kobject对应的sysfs inode(类型为KernFSInode)
    fn set_inode(&self, inode: Option<Arc<KernFSInode>>);

    /// 获取当前kobject对应的sysfs inode(类型为KernFSInode)
    fn inode(&self) -> Option<Arc<KernFSInode>>;

    fn parent(&self) -> Option<Weak<dyn KObject>>;

    /// 设置当前kobject的parent kobject（不一定与kset相同）
    fn set_parent(&self, parent: Option<Weak<dyn KObject>>);

    /// 当前kobject属于哪个kset
    fn kset(&self) -> Option<Arc<KSet>>;

    /// 设置当前kobject所属的kset
    fn set_kset(&self, kset: Option<Arc<KSet>>);

    fn kobj_type(&self) -> Option<&'static dyn KObjType>;

    fn name(&self) -> String;

    fn set_name(&self, name: String);

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState>;

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState>;

    fn set_kobj_state(&self, state: KObjectState);
}

impl dyn KObject {
    /// 更新kobject的状态
    pub fn update_kobj_state(&self, insert: Option<KObjectState>, remove: Option<KObjectState>) {
        let insert = insert.unwrap_or(KObjectState::empty());
        let remove = remove.unwrap_or(KObjectState::empty());
        let mut state = self.kobj_state_mut();
        *state = (*state | insert) & !remove;
    }
}

impl DowncastArc for dyn KObject {
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any> {
        self
    }
}

pub trait KObjType: Debug {
    fn release(&self, _kobj: Arc<dyn KObject>) {}
    fn sysfs_ops(&self) -> Option<&dyn SysFSOps>;

    fn attribute_groups(&self) -> Option<&'static [&'static dyn AttributeGroup]>;
}

bitflags! {
    pub struct KObjectState: u32 {
        const IN_SYSFS = 1 << 0;
        const ADD_UEVENT_SENT = 1 << 1;
        const REMOVE_UEVENT_SENT = 1 << 2;
        const INITIALIZED = 1 << 3;
    }

}

#[derive(Debug)]
pub struct LockedKObjectState(RwLock<KObjectState>);

impl LockedKObjectState {
    pub const fn new(state: KObjectState) -> LockedKObjectState {
        LockedKObjectState(RwLock::new(state))
    }
}

impl Deref for LockedKObjectState {
    type Target = RwLock<KObjectState>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub trait KObjectAttribute: Attribute {
    fn support(&self) -> SysFSOpsSupport;

    fn show(&self, kobj: &dyn KObject, buf: &mut [u8]) -> Result<usize, SystemError>;
    fn store(&self, kobj: &dyn KObject, buf: &[u8]) -> Result<usize, SystemError>;
}

#[derive(Debug)]
pub struct KObjectSysFSOps;

impl SysFSOps for KObjectSysFSOps {
    fn support(&self, attr: &dyn Attribute) -> SysFSOpsSupport {
        return attr.support();
    }

    fn show(
        &self,
        kobj: Arc<dyn KObject>,
        attr: &dyn Attribute,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        let r = attr.show(kobj, buf).map_err(|e| {
            if e == SystemError::EOPNOTSUPP_OR_ENOTSUP {
                SystemError::EIO
            } else {
                e
            }
        });

        return r;
    }

    fn store(
        &self,
        kobj: Arc<dyn KObject>,
        attr: &dyn Attribute,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        let r = attr.store(kobj, buf).map_err(|e| {
            if e == SystemError::EOPNOTSUPP_OR_ENOTSUP {
                SystemError::EIO
            } else {
                e
            }
        });

        return r;
    }
}

#[derive(Debug)]
pub struct KObjectManager;

impl KObjectManager {
    pub fn add_kobj(
        kobj: Arc<dyn KObject>,
        join_kset: Option<Arc<KSet>>,
    ) -> Result<(), SystemError> {
        if join_kset.is_some() {
            let kset = join_kset.unwrap();
            kset.join(&kobj);
            // 如果kobject没有parent，那么就将这个kset作为parent
            if kobj.parent().is_none() {
                kobj.set_parent(Some(Arc::downgrade(&(kset as Arc<dyn KObject>))));
            }
        }

        let r = Self::create_dir(kobj.clone());

        if let Err(e) = r {
            // https://opengrok.ringotek.cn/xref/linux-6.1.9/lib/kobject.c?r=&mo=10426&fi=394#224
            if let Some(kset) = kobj.kset() {
                kset.leave(&kobj);
            }
            kobj.set_parent(None);
            if e == SystemError::EEXIST {
                kerror!("KObjectManager::add_kobj() failed with error: {e:?}, kobj:{kobj:?}");
            }

            return Err(e);
        }

        kobj.update_kobj_state(Some(KObjectState::IN_SYSFS), None);
        return Ok(());
    }

    fn create_dir(kobj: Arc<dyn KObject>) -> Result<(), SystemError> {
        // create dir in sysfs
        sysfs_instance().create_dir(kobj.clone())?;

        // create default attributes in sysfs
        if let Some(ktype) = kobj.kobj_type() {
            let groups = ktype.attribute_groups();
            if let Some(groups) = groups {
                let r = sysfs_instance().create_groups(&kobj, groups);
                if let Err(e) = r {
                    sysfs_instance().remove_dir(&kobj);
                    return Err(e);
                }
            }
        }

        return Ok(());
    }
}
