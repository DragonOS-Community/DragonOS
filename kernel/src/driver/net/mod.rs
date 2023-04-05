use core::any::Any;

use alloc::string::String;
use smoltcp::{wire::EthernetAddress, iface};

use crate::syscall::SystemError;

use super::Driver;

pub mod virtio_net;

pub trait NetDriver: Driver{
    /// @brief 获取网卡的MAC地址
    fn mac(&self) -> EthernetAddress;

    fn name(&self) -> String {
        return format!("eth{}", self.nic_id());
    }

    /// @brief 获取网卡的id
    fn nic_id(&self) -> usize;

    fn poll(&self, iface_id:usize, sockets: &mut iface::SocketSet) -> Result<(), SystemError>;
    // fn as_any_ref(&'static self) -> &'static dyn core::any::Any;
}
