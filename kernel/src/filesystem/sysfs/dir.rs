#![allow(dead_code)]
use alloc::sync::Arc;

use crate::driver::base::device::KObject;

use super::AttributeGroup;

#[derive(Debug)]
pub struct SysKernDirPriv {
    kobj: Arc<dyn KObject>,
    attribute_group: Option<&'static dyn AttributeGroup>,
}

impl SysKernDirPriv {
    pub fn new(
        kobj: Arc<dyn KObject>,
        attribute_group: Option<&'static dyn AttributeGroup>,
    ) -> Self {
        Self {
            kobj,
            attribute_group,
        }
    }

    pub fn kobj(&self) -> Arc<dyn KObject> {
        self.kobj.clone()
    }

    pub fn attribute_group(&self) -> Option<&'static dyn AttributeGroup> {
        self.attribute_group
    }
}
