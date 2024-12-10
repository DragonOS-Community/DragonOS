use alloc::{string::String, sync::Arc};
use smoltcp::{
    iface,
    wire::{self, EthernetAddress},
};
use sysfs::netdev_register_kobject;

use super::base::device::Device;
use crate::libs::spinlock::SpinLock;
use system_error::SystemError;

pub mod class;
mod dma;
pub mod e1000e;
pub mod irq_handle;
pub mod loopback;
pub mod sysfs;
pub mod virtio_net;

bitflags! {
    pub struct NetDeivceState: u16 {
        /// 表示网络设备已经启动
        const __LINK_STATE_START = 1 << 0;
        /// 表示网络设备在系统中存在，即注册到sysfs中
        const __LINK_STATE_PRESENT = 1 << 1;
        /// 表示网络设备没有检测到载波信号
        const __LINK_STATE_NOCARRIER = 1 << 2;
        /// 表示设备的链路监视操作处于挂起状态
        const __LINK_STATE_LINKWATCH_PENDING = 1 << 3;
        /// 表示设备处于休眠状态
        const __LINK_STATE_DORMANT = 1 << 4;
    }
}

#[derive(Debug, Copy, Clone)]
#[allow(dead_code, non_camel_case_types)]
pub enum Operstate {
    /// 网络接口的状态未知
    IF_OPER_UNKNOWN = 0,
    /// 网络接口不存在
    IF_OPER_NOTPRESENT = 1,
    /// 网络接口已禁用或未连接
    IF_OPER_DOWN = 2,
    /// 网络接口的下层接口已关闭
    IF_OPER_LOWERLAYERDOWN = 3,
    /// 网络接口正在测试
    IF_OPER_TESTING = 4,
    /// 网络接口处于休眠状态
    IF_OPER_DORMANT = 5,
    /// 网络接口已启用
    IF_OPER_UP = 6,
}

#[allow(dead_code)]
pub trait NetDevice: Device {
    /// @brief 获取网卡的MAC地址
    fn mac(&self) -> EthernetAddress;

    fn iface_name(&self) -> String;

    /// @brief 获取网卡的id
    fn nic_id(&self) -> usize;

    fn poll(&self, sockets: &mut iface::SocketSet) -> Result<(), SystemError>;

    fn update_ip_addrs(&self, ip_addrs: &[wire::IpCidr]) -> Result<(), SystemError>;

    /// @brief 获取smoltcp的网卡接口类型
    fn inner_iface(&self) -> &SpinLock<smoltcp::iface::Interface>;
    // fn as_any_ref(&'static self) -> &'static dyn core::any::Any;

    fn addr_assign_type(&self) -> u8;

    fn net_device_type(&self) -> u16;

    fn net_state(&self) -> NetDeivceState;

    fn set_net_state(&self, state: NetDeivceState);

    fn operstate(&self) -> Operstate;

    fn set_operstate(&self, state: Operstate);
}

/// 网络设备的公共数据
#[derive(Debug)]
pub struct NetDeviceCommonData {
    /// 表示网络接口的地址分配类型
    pub addr_assign_type: u8,
    /// 表示网络接口的类型
    pub net_device_type: u16,
    /// 表示网络接口的状态
    pub state: NetDeivceState,
    /// 表示网络接口的操作状态
    pub operstate: Operstate,
}

impl Default for NetDeviceCommonData {
    fn default() -> Self {
        Self {
            addr_assign_type: 0,
            net_device_type: 1,
            state: NetDeivceState::empty(),
            operstate: Operstate::IF_OPER_UNKNOWN,
        }
    }
}

/// 将网络设备注册到sysfs中
/// 参考：https://code.dragonos.org.cn/xref/linux-2.6.39/net/core/dev.c?fi=register_netdev#5373
fn register_netdevice(dev: Arc<dyn NetDevice>) -> Result<(), SystemError> {
    // 在sysfs中注册设备
    netdev_register_kobject(dev.clone())?;

    // 标识网络设备在系统中存在
    dev.set_net_state(NetDeivceState::__LINK_STATE_PRESENT);

    return Ok(());
}
