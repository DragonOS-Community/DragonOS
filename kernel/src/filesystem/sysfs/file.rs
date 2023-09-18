#![allow(dead_code)]
use super::Attribute;

#[derive(Debug)]
pub struct SysKernFilePriv {
    attribute: Option<&'static dyn Attribute>,
    // todo: 增加bin attribute,它和attribute二选一，只能有一个为Some
}

impl SysKernFilePriv {
    pub fn new(attribute: Option<&'static dyn Attribute>) -> Self {
        if attribute.is_none() {
            panic!("attribute can't be None");
        }
        return Self { attribute };
    }

    pub fn attribute(&self) -> Option<&'static dyn Attribute> {
        self.attribute
    }
}
