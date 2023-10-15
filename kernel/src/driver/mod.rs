pub mod acpi;
pub mod base;
pub mod disk;
pub mod keyboard;
pub mod net;
pub mod pci;
pub mod timers;
pub mod tty;
pub mod video;
pub mod virtio;

use core::fmt::Debug;

use alloc::{sync::Arc, vec::Vec};

use crate::{filesystem::sysfs::AttributeGroup, syscall::SystemError};

use self::base::{
    device::{
        bus::Bus, driver::DriverProbeType, Device, DevicePrivateData, DeviceResource, IdTable,
    },
    kobject::KObject,
};
