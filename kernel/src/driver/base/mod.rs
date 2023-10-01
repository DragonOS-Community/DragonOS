use alloc::sync::Arc;

use crate::syscall::SystemError;

use self::{device::init::devices_init, kobject::KObject, kset::KSet};

pub mod block;
pub mod c_adapter;
pub mod char;
pub mod class;
pub mod device;
pub mod firmware;
pub mod hypervisor;
pub mod init;
pub mod kobject;
pub mod kset;
pub mod map;
pub mod platform;
