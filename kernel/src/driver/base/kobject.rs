use core::{any::Any, fmt::Debug, hash::Hash, ops::Deref};

use alloc::{
    string::String,
    sync::{Arc, Weak},
};
use driver_base_macros::get_weak_or_clear;
use intertrait::CastFromSync;
use log::{debug, error};

use crate::{
    filesystem::{
        kernfs::KernFSInode,
        sysfs::{sysfs_instance, Attribute, AttributeGroup, SysFSOps, SysFSOpsSupport},
    },
    libs::{
        casting::DowncastArc,
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
    },
};

use system_error::SystemError;

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

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>);

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

/// kobject的公共数据
#[derive(Debug, Default)]
pub struct KObjectCommonData {
    pub kern_inode: Option<Arc<KernFSInode>>,
    pub parent: Option<Weak<dyn KObject>>,
    pub kset: Option<Arc<KSet>>,
    pub kobj_type: Option<&'static dyn KObjType>,
}

impl KObjectCommonData {
    pub fn get_parent_or_clear_weak(&mut self) -> Option<Weak<dyn KObject>> {
        get_weak_or_clear!(self.parent)
    }
}

pub trait KObjType: Debug + Send + Sync {
    /// 当指定的kobject被释放时，设备驱动模型会调用此方法
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
    pub fn new(state: Option<KObjectState>) -> LockedKObjectState {
        let state = state.unwrap_or(KObjectState::empty());
        LockedKObjectState(RwLock::new(state))
    }
}

impl Deref for LockedKObjectState {
    type Target = RwLock<KObjectState>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Default for LockedKObjectState {
    fn default() -> Self {
        LockedKObjectState::new(None)
    }
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
            if e == SystemError::ENOSYS {
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
            if e == SystemError::ENOSYS {
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
    pub fn init_and_add_kobj(
        kobj: Arc<dyn KObject>,
        join_kset: Option<Arc<KSet>>,
        kobj_type: Option<&'static dyn KObjType>,
    ) -> Result<(), SystemError> {
        Self::kobj_init(&kobj, kobj_type);
        Self::add_kobj(kobj, join_kset)
    }

    pub fn kobj_init(kobj: &Arc<dyn KObject>, kobj_type: Option<&'static dyn KObjType>) {
        kobj.set_kobj_type(kobj_type);
    }

    pub fn add_kobj(
        kobj: Arc<dyn KObject>,
        join_kset: Option<Arc<KSet>>,
    ) -> Result<(), SystemError> {
        if let Some(kset) = join_kset {
            kset.join(&kobj);
            // 如果kobject没有parent，那么就将这个kset作为parent
            if kobj.parent().is_none() {
                kobj.set_parent(Some(Arc::downgrade(&(kset as Arc<dyn KObject>))));
            }
        }

        let r = Self::create_dir(kobj.clone());

        if let Err(e) = r {
            // https://code.dragonos.org.cn/xref/linux-6.1.9/lib/kobject.c?r=&mo=10426&fi=394#224
            if let Some(kset) = kobj.kset() {
                kset.leave(&kobj);
            }
            kobj.set_parent(None);
            if e == SystemError::EEXIST {
                error!("KObjectManager::add_kobj() failed with error: {e:?}, kobj:{kobj:?}");
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

    /// 从sysfs中移除kobject
    pub fn remove_kobj(kobj: Arc<dyn KObject>) {
        let ktype = kobj.kobj_type();
        if let Some(ktype) = ktype {
            if let Some(groups) = ktype.attribute_groups() {
                sysfs_instance().remove_groups(&kobj, groups);
            }
        }

        // todo: 发送uevent: KOBJ_REMOVE

        sysfs_instance().remove_dir(&kobj);
        kobj.update_kobj_state(None, Some(KObjectState::IN_SYSFS));
        let kset = kobj.kset();
        if let Some(kset) = kset {
            kset.leave(&kobj);
        }
        kobj.set_parent(None);
    }
}

/// 动态创建的kobject对象的ktype
#[derive(Debug)]
pub struct DynamicKObjKType;

impl KObjType for DynamicKObjKType {
    fn release(&self, kobj: Arc<dyn KObject>) {
        debug!("DynamicKObjKType::release() kobj:{:?}", kobj.name());
    }

    fn sysfs_ops(&self) -> Option<&dyn SysFSOps> {
        Some(&KObjectSysFSOps)
    }

    fn attribute_groups(&self) -> Option<&'static [&'static dyn AttributeGroup]> {
        None
    }
}
