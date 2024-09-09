use alloc::{fmt, string::String, sync::Arc, vec::Vec};

use crate::{
    libs::{rwlock::RwLock, spinlock::SpinLock},
    net::socket::inet::{common::PortManager, InetSocket},
};
use smoltcp;
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
    fn poll(&self);

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
    fn smol_iface(&self) -> &SpinLock<smoltcp::iface::Interface> {
        &self.common().smol_iface
    }
    // fn as_any_ref(&'static self) -> &'static dyn core::any::Any;

    /// # `sockets`
    /// 获取网卡的套接字集
    fn sockets(&self) -> &SpinLock<smoltcp::iface::SocketSet<'static>> {
        &self.common().sockets
    }

    /// # `port_manager`
    /// 用于管理网卡的端口
    fn port_manager(&self) -> &PortManager {
        &self.common().port_manager
    }
}

pub struct IfaceCommon {
    iface_id: usize,
    smol_iface: SpinLock<smoltcp::iface::Interface>,
    /// 存smoltcp网卡的套接字集
    sockets: SpinLock<smoltcp::iface::SocketSet<'static>>,
    /// 存 kernel wrap smoltcp socket 的集合
    bounds: RwLock<Vec<Arc<dyn InetSocket>>>,
    /// 端口管理器
    port_manager: PortManager,
    /// 下次轮询的时间
    poll_at_ms: core::sync::atomic::AtomicU64,
}

impl fmt::Debug for IfaceCommon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IfaceCommon")
            .field("iface_id", &self.iface_id)
            .field("sockets", &self.sockets)
            .field("bounds", &self.bounds)
            .field("port_manager", &self.port_manager)
            .field("poll_at_ms", &self.poll_at_ms)
            .finish()
    }
}

impl IfaceCommon {
    pub fn new(iface_id: usize, iface: smoltcp::iface::Interface) -> Self {
        IfaceCommon {
            iface_id,
            smol_iface: SpinLock::new(iface),
            sockets: SpinLock::new(smoltcp::iface::SocketSet::new(Vec::new())),
            bounds: RwLock::new(Vec::new()),
            port_manager: PortManager::new(),
            poll_at_ms: core::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn poll<D>(&self, device: &mut D)
    where
        D: smoltcp::phy::Device + ?Sized,
    {
        let timestamp = crate::time::Instant::now().into();
        let mut sockets = self.sockets.lock_no_preempt();
        let mut interface = self.smol_iface.lock_no_preempt();

        let (has_events, poll_at) = {
            let mut has_events = false;
            let mut poll_at;
            loop {
                has_events |= interface.poll(timestamp, device, &mut sockets);
                poll_at = interface.poll_at(timestamp, &sockets);
                let Some(instant) = poll_at else {
                    break;
                };
                if instant > timestamp {
                    break;
                }
            }
            (has_events, poll_at)
        };

        // drop sockets here to avoid deadlock
        drop(interface);
        drop(sockets);

        use core::sync::atomic::Ordering;
        if let Some(instant) = poll_at {
            let _old_instant = self.poll_at_ms.load(Ordering::Relaxed);
            let new_instant = instant.total_millis() as u64;
            self.poll_at_ms.store(new_instant, Ordering::Relaxed);

            // if old_instant == 0 || new_instant < old_instant {
            //     self.polling_wait_queue.wake_all();
            // }
        } else {
            self.poll_at_ms.store(0, Ordering::Relaxed);
        }

        if has_events {
            // We never try to hold the write lock in the IRQ context, and we disable IRQ when
            // holding the write lock. So we don't need to disable IRQ when holding the read lock.
            self.bounds.read().iter().for_each(|bound_socket| {
                bound_socket.on_iface_events();
            });

            // let closed_sockets = self
            //     .closing_sockets
            //     .lock_irq_disabled()
            //     .extract_if(|closing_socket| closing_socket.is_closed())
            //     .collect::<Vec<_>>();
            // drop(closed_sockets);
        }
    }

    pub fn update_ip_addrs(&self, ip_addrs: &[smoltcp::wire::IpCidr]) -> Result<(), SystemError> {
        if ip_addrs.len() != 1 {
            return Err(SystemError::EINVAL);
        }

        self.smol_iface.lock().update_ip_addrs(|addrs| {
            let dest = addrs.iter_mut().next();

            if let Some(dest) = dest {
                *dest = ip_addrs[0];
            } else {
                addrs.push(ip_addrs[0]).expect("Push ipCidr failed: full");
            }
        });
        return Ok(());
    }

    // 需要bounds储存具体的Inet Socket信息，以提供不同种类inet socket的事件分发
    pub fn bind_socket(&self, socket: Arc<dyn InetSocket>) {
        self.bounds.write().push(socket);
    }
}
