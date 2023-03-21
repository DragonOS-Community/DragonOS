use alloc::string::String;
use smoltcp::wire::EthernetAddress;

use super::Driver;

pub mod virtio_net;

pub trait NetDriver: Driver {
    /// @brief 获取网卡的MAC地址
    fn mac(&self) -> EthernetAddress;

    fn name(&self) -> String {
        return format!("eth{}", self.nic_id());
    }

    /// @brief 获取网卡的id
    fn nic_id(&self) -> usize;
    
}
