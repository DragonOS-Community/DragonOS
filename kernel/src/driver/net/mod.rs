use alloc::{string::String, fmt};

use smoltcp;
use crate::libs::spinlock::SpinLock;
use system_error::SystemError;

mod dma;
pub mod e1000e;
pub mod irq_handle;
pub mod loopback;
pub mod virtio_net;

#[allow(dead_code)]
pub trait Iface: crate::driver::base::device::Device {
    /// # `common`
    /// 获取网卡的公共信息
    fn common(&self) -> &IfaceCommon;

    /// # `mac` 
    /// 获取网卡的MAC地址
    fn mac(&self) -> smoltcp::wire::EthernetAddress;

    /// # `name`
    /// 获取网卡名
    fn name(&self) -> String;

    /// # `nic_id` 
    /// 获取网卡id
    fn nic_id(&self) -> usize {
        self.common().iface_id
    }

    /// # `poll` 
    /// 用于轮询接口的状态。
    /// ## 参数
    /// - `sockets` ：一个可变引用到 `smoltcp::iface::SocketSet`，表示要轮询的套接字集
    /// ## 返回值
    /// - 成功返回 `Ok(())`
    /// - 如果轮询失败，返回 `Err(SystemError::EAGAIN_OR_EWOULDBLOCK)`，表示需要再次尝试或者操作会阻塞
    fn poll(&self) -> Result<(), SystemError>;

    /// # `update_ip_addrs` 
    /// 用于更新接口的 IP 地址
    /// ## 参数
    /// - `ip_addrs` ：一个包含 `smoltcp::wire::IpCidr` 的切片，表示要设置的 IP 地址和子网掩码
    /// ## 返回值
    /// - 如果 `ip_addrs` 的长度不为 1，返回 `Err(SystemError::EINVAL)`，表示输入参数无效
    fn update_ip_addrs(&self, ip_addrs: &[smoltcp::wire::IpCidr]) -> Result<(), SystemError> {
        self.common().update_ip_addrs(ip_addrs)
    }

    /// @brief 获取smoltcp的网卡接口类型
    #[inline(always)]
    fn inner_iface(&self) -> &SpinLock<smoltcp::iface::Interface> {
        &self.common().iface
    }
    // fn as_any_ref(&'static self) -> &'static dyn core::any::Any;

    /// # `sockets`
    /// 获取网卡的套接字集
    fn sockets(&self) -> &SpinLock<smoltcp::iface::SocketSet<'static>> {
        &self.common().sockets
    }

    /// # `port_manager`
    /// 用于管理网卡的端口
    fn port_manager(&self) -> &crate::net::socket::PortManager {
        &self.common().port_manager
    }
}

pub struct IfaceCommon {
    iface_id: usize,
    iface: SpinLock<smoltcp::iface::Interface>,
    sockets: SpinLock<smoltcp::iface::SocketSet<'static>>,
    port_manager: crate::net::socket::PortManager,
    _poll_at_ms: core::sync::atomic::AtomicU64,
}

impl fmt::Debug for IfaceCommon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IfaceCommon")
            .field("iface_id", &self.iface_id)
            .field("sockets", &self.sockets)
            .field("port_manager", &self.port_manager)
            .field("_poll_at_ms", &self._poll_at_ms)
            .finish()
    }
}

impl IfaceCommon {
    pub fn new(iface_id: usize, iface: smoltcp::iface::Interface) -> Self {
        IfaceCommon {
            iface_id,
            iface: SpinLock::new(iface),
            sockets: SpinLock::new(smoltcp::iface::SocketSet::new(vec![])),
            port_manager: crate::net::socket::PortManager::new(),
            _poll_at_ms: core::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn poll<D>(&self, device: &mut D) -> Result<(), SystemError>
    where
        D: smoltcp::phy::Device + ?Sized,
    {
        let timestamp = crate::time::Instant::now().into();
        let mut sockets = self.sockets.lock();
        if self.iface.lock().poll(timestamp, device, &mut sockets) {
            return Ok(());
        } else {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
    }

    pub fn update_ip_addrs(&self, ip_addrs: &[smoltcp::wire::IpCidr]) -> Result<(), SystemError> {
        if ip_addrs.len() != 1 {
            return Err(SystemError::EINVAL);
        }

        self.iface.lock().update_ip_addrs(|addrs| {
            let dest = addrs.iter_mut().next();

            if let Some(dest) = dest {
                *dest = ip_addrs[0];
            } else {
                addrs.push(ip_addrs[0]).expect("Push ipCidr failed: full");
            }
        });
        return Ok(());
    }
}
