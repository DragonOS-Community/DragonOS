use crate::{
    driver::base::{
        kobject::{KObjType, KObject, KObjectState},
        kset::KSet,
    },
    filesystem::{kernfs::KernFSInode, sysfs::BinAttribute},
    libs::rwlock::{RwLockReadGuard, RwLockWriteGuard},
    libs::spinlock::SpinLock,
};
use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::fmt::Debug;

/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/of.h#51
#[allow(dead_code)]
#[derive(Debug)]
pub struct DeviceNode {
    full_name: Option<&'static str>,
    full_name_allocated: Option<String>,
    inner: SpinLock<InnerDeviceNode>,
}

#[allow(dead_code)]
#[derive(Debug)]
struct InnerDeviceNode {
    properties: Vec<Property>,
    parent: Weak<DeviceNode>,
    children: Vec<Arc<DeviceNode>>,
    sibling: Option<Weak<DeviceNode>>,
    private_data: Option<Arc<dyn DeviceNodePrivateData>>,
}

#[allow(dead_code)]
impl DeviceNode {
    pub fn new(
        full_name: Option<&'static str>,
        full_name_allocated: Option<String>,
    ) -> Option<Arc<Self>> {
        if full_name.is_none() && full_name_allocated.is_none() {
            return None;
        }

        let x = DeviceNode {
            full_name,
            full_name_allocated,
            inner: SpinLock::new(InnerDeviceNode {
                properties: Vec::new(),
                parent: Weak::new(),
                children: Vec::new(),
                sibling: None,
                private_data: None,
            }),
        };

        return Some(Arc::new(x));
    }

    pub fn add_property(&self, prop: Property) {
        self.inner.lock().properties.push(prop);
    }

    pub fn properties(&self) -> Vec<Property> {
        self.inner.lock().properties.clone()
    }

    pub fn parent(&self) -> Option<Arc<DeviceNode>> {
        self.inner.lock().parent.upgrade()
    }

    pub fn set_parent(&self, parent: Arc<DeviceNode>) {
        self.inner.lock().parent = Arc::downgrade(&parent);
    }

    pub fn children(&self) -> Vec<Arc<DeviceNode>> {
        self.inner.lock().children.clone()
    }

    pub fn add_child(&self, child: Arc<DeviceNode>) {
        self.inner.lock().children.push(child);
    }

    pub fn sibling(&self) -> Option<Arc<DeviceNode>> {
        self.inner.lock().sibling.as_ref().and_then(|s| s.upgrade())
    }

    pub fn set_sibling(&self, sibling: Arc<DeviceNode>) {
        self.inner.lock().sibling = Some(Arc::downgrade(&sibling));
    }

    pub fn private_data(&self) -> Option<Arc<dyn DeviceNodePrivateData>> {
        self.inner.lock().private_data.clone()
    }

    pub fn set_private_data(&self, data: Arc<dyn DeviceNodePrivateData>) {
        self.inner.lock().private_data = Some(data);
    }
}

pub trait DeviceNodePrivateData: Send + Sync + Debug {}

impl KObject for DeviceNode {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn set_inode(&self, _inode: Option<Arc<KernFSInode>>) {
        todo!()
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        todo!()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        todo!()
    }

    fn set_parent(&self, _parent: Option<Weak<dyn KObject>>) {
        todo!()
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        todo!()
    }

    fn set_kset(&self, _kset: Option<Arc<KSet>>) {
        todo!()
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        todo!()
    }

    fn set_kobj_type(&self, _ktype: Option<&'static dyn KObjType>) {
        todo!()
    }

    fn name(&self) -> String {
        todo!()
    }

    fn set_name(&self, _name: String) {}

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        todo!()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        todo!()
    }

    fn set_kobj_state(&self, _state: KObjectState) {
        todo!()
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Property {
    name: String,
    value: Vec<u8>,
    bin_attr: Option<Arc<dyn BinAttribute>>,
}

impl Property {
    #[allow(dead_code)]
    pub const fn new(name: String, value: Vec<u8>, battr: Option<Arc<dyn BinAttribute>>) -> Self {
        Property {
            name,
            value,
            bin_attr: battr,
        }
    }
}
