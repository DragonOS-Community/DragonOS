#![allow(unused)]

use crate::prelude::*;

/// Mount point descriptor
#[derive(Clone)]
pub struct MountPoint {
    /// Mount done flag
    pub mounted: bool,
    ///  Mount point name
    pub mount_name: String,
}

impl MountPoint {
    pub fn new(name: &str) -> Self {
        Self {
            mounted: false,
            mount_name: name.to_owned(),
        }
    }
}

impl Debug for MountPoint {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(f, "Ext4MountPoint {{ mount_name: {:?} }}", self.mount_name)
    }
}
