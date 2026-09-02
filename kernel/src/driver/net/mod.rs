use alloc::collections::VecDeque;
use alloc::ffi::CString;
use alloc::sync::Weak;
use alloc::{fmt, vec::Vec};
use alloc::{string::String, sync::Arc};
use core::cell::Cell;
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use net_poll_state::{DueResult, PollDeadline, PublishResult};
use sysfs::{netdev_emit_uevent, netdev_register_kobject, netdev_unregister_kobject};

use crate::driver::net::napi::NapiStruct;
use crate::driver::net::types::{InterfaceFlags, InterfaceType};
use crate::libs::rwsem::{RwSem, RwSemReadGuard};
use crate::libs::spinlock::SpinLock;
use crate::net::routing::RouterEnableDeviceCommon;
use crate::process::namespace::net_namespace::NetNamespace;
use crate::{
    libs::{mutex::Mutex, rwlock::RwLock},
    net::socket::inet::{common::PortManager, InetSocket},
    process::ProcessState,
};
use smoltcp::phy::{
    Device as SmolDevice, DeviceCapabilities, PacketMeta, RxToken, TxToken as SmolTxToken,
};
use system_error::SystemError;

pub mod bridge;
pub mod class;
mod dma;
pub mod e1000e;
pub mod loopback;
pub mod napi;
pub mod sysfs;
pub mod types;
pub mod veth;
pub mod virtio_net;

mod iface;
mod iface_common;
mod local_output;
mod local_queue;

pub use iface::*;
pub use iface_common::IfaceCommon;
pub(crate) use local_output::IfacePollScope;
use local_output::*;
use local_queue::*;
