use smoltcp::wire::EthernetAddress;

use crate::syscall::SystemError;

use super::Driver;

pub mod virtio_net;

pub trait NetDriver: Driver {
    /// @brief 获取网卡的MAC地址
    fn mac(&self) -> EthernetAddress;

    fn send(&self, data:&[u8])->Result<usize,SystemError>;
}
