use alloc::string::String;
use smoltcp::{
    iface,
    wire::{self, EthernetAddress},
};

use crate::{libs::spinlock::SpinLock, syscall::SystemError};

use super::Driver;

pub mod virtio_net;

pub trait NetDriver: Driver {
    /// @brief 获取网卡的MAC地址
    fn mac(&self) -> EthernetAddress;

    fn name(&self) -> String;

    /// @brief 获取网卡的id
    fn nic_id(&self) -> usize;

    fn poll(&self, sockets: &mut iface::SocketSet) -> Result<(), SystemError>;

    fn update_ip_addrs(&self, ip_addrs: &[wire::IpCidr]) -> Result<(), SystemError>;

    /// @brief 获取smoltcp的网卡接口类型
    fn inner_iface(&self) -> &SpinLock<smoltcp::iface::Interface>;
    // fn as_any_ref(&'static self) -> &'static dyn core::any::Any;
}
