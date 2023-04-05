use core::{fmt::Debug, ops::DerefMut};

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use smoltcp::{phy, wire};
use virtio_drivers::{device::net::VirtIONet, transport::Transport};

use crate::{
    driver::{generate_nic_id, virtio::virtio_impl::HalImpl, Driver, NET_DRIVERS},
    include::bindings::bindings::usleep,
    kdebug, kerror, kinfo, kwarn,
    libs::rwlock::RwLock,
    net::{Interface, NET_FACES},
    time::{timer::schedule_timeout, Instant}, syscall::SystemError,
};

use super::NetDriver;

/// @brief Virtio网络设备驱动(加锁)
pub struct VirtioNICDriver<T: Transport> {
    pub inner: RwLock<InnerVirtIONet<T>>,
}

impl<T: 'static + Transport> VirtioNICDriver<T> {
    pub fn new(driver_net: VirtIONet<HalImpl, T, 2>) -> Arc<Self> {
        let mut iface_config = smoltcp::iface::Config::new();

        // todo: 随机设定这个值。
        // 参见 https://docs.rs/smoltcp/latest/smoltcp/iface/struct.Config.html#structfield.random_seed
        iface_config.random_seed = 12345;

        iface_config.hardware_addr = Some(wire::HardwareAddress::Ethernet(
            smoltcp::wire::EthernetAddress(driver_net.mac_address()),
        ));

        let inner = RwLock::new(InnerVirtIONet {
            virtio_net: driver_net,
            self_ref: Weak::new(),
            net_device_id: generate_nic_id(),
            ifaces: Vec::new(),
        });

        let mut s: VirtioNICDriver<T> = Self { inner };

        let result: Arc<VirtioNICDriver<T>> = Arc::new(s);
        result.inner.write().self_ref = Arc::downgrade(&result);

        let iface: Arc<Interface> = Arc::new(Interface::new(
            smoltcp::iface::Interface::new::<InnerVirtIONet<T>>(
                iface_config,
                &mut result.inner.write().deref_mut(),
            ),
            Arc::downgrade(&(result.clone() as Arc<dyn NetDriver>)),
        ));

        result.inner.write().ifaces.push(iface.clone());

        let nic_id = result.inner.read().net_device_id;
        // 将网卡驱动注册到网卡驱动列表中
        NET_DRIVERS.write().insert(nic_id, result.clone());
        // 将网络接口注册到iface列表中
        NET_FACES.write().insert(iface.iface_id(), iface.clone());

        return result;
    }
}

impl<T: Transport> Debug for VirtioNICDriver<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        return write!(f, "VirtioNICDriver");
    }
}

/// @brief Virtio网络设备驱动(不加锁, 仅供内部使用)
pub struct InnerVirtIONet<T: Transport> {
    /// Virtio网络设备
    virtio_net: VirtIONet<HalImpl, T, 2>,
    /// 自引用
    self_ref: Weak<VirtioNICDriver<T>>,
    /// 网卡ID
    net_device_id: usize,

    /// 网卡的所有网络接口
    ifaces: Vec<Arc<Interface>>,
}

pub struct VirtioNetToken<T: Transport> {
    driver: Arc<VirtioNICDriver<T>>,
    rx_buffer: Option<virtio_drivers::device::net::RxBuffer>,
}

impl<'a, T: Transport> VirtioNetToken<T> {
    pub fn new(
        driver: Arc<VirtioNICDriver<T>>,
        rx_buffer: Option<virtio_drivers::device::net::RxBuffer>,
    ) -> Self {
        return Self { driver, rx_buffer };
    }
}

impl<T: Transport> phy::Device for InnerVirtIONet<T> {
    type RxToken<'a> = VirtioNetToken<T> where Self: 'a;
    type TxToken<'a> = VirtioNetToken<T> where Self: 'a;

    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // self.notify_rx_queue();
        // self.inner.write().virtio_net.transport.notify(0);
        let mut driver_net = self;
        kdebug!("VirtioNet: receive");
        match driver_net.virtio_net.receive() {
            Ok(buf) => Some((
                VirtioNetToken::new(driver_net.self_ref.upgrade().unwrap(), Some(buf)),
                VirtioNetToken::new(driver_net.self_ref.upgrade().unwrap(), None),
            )),
            Err(virtio_drivers::Error::NotReady) => None,
            Err(err) => panic!("VirtIO receive failed: {}", err),
        }
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        let driver_net = self;
        kdebug!("VirtioNet: transmit");
        if driver_net.virtio_net.can_send() {
            kdebug!("VirtioNet: can send");
            return Some(VirtioNetToken::new(
                driver_net.self_ref.upgrade().unwrap(),
                None,
            ));
        } else {
            kdebug!("VirtioNet: can not send");
            return None;
        }
    }

    fn capabilities(&self) -> phy::DeviceCapabilities {
        let mut caps = phy::DeviceCapabilities::default();
        // 网卡的最大传输单元. 请与IP层的MTU进行区分。这个值应当是网卡的最大传输单元，而不是IP层的MTU。
        caps.max_transmission_unit = 2000;
        /*
           Maximum burst size, in terms of MTU.
           The network device is unable to send or receive bursts large than the value returned by this function.
           If None, there is no fixed limit on burst size, e.g. if network buffers are dynamically allocated.
        */
        caps.max_burst_size = Some(1);
        return caps;
    }
}

impl<T: Transport> phy::TxToken for VirtioNetToken<T> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // // 为了线程安全，这里需要对VirtioNet进行加【写锁】，以保证对设备的互斥访问。

        let mut driver_net = self.driver.inner.write();
        let mut tx_buf = driver_net.virtio_net.new_tx_buffer(len);
        let result = f(tx_buf.packet_mut());
        driver_net
            .virtio_net
            .send(tx_buf)
            .expect("virtio_net send failed");
        return result;
    }
}

impl<T: Transport> phy::RxToken for VirtioNetToken<T> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // 为了线程安全，这里需要对VirtioNet进行加【写锁】，以保证对设备的互斥访问。
        let mut rx_buf = self.rx_buffer.unwrap();
        let result = f(rx_buf.packet_mut());
        self.driver
            .inner
            .write()
            .virtio_net
            .recycle_rx_buffer(rx_buf)
            .expect("virtio_net recv failed");
        result
    }
}

/// @brief virtio-net 驱动的初始化与测试
pub fn virtio_net<T: Transport + 'static>(transport: T) {
    let driver_net: VirtIONet<HalImpl, T, 2> =
        match VirtIONet::<HalImpl, T, 2>::new(transport, 4096) {
            Ok(net) => net,
            Err(_) => {
                kerror!("VirtIONet init failed");
                return;
            }
        };
    let mac = smoltcp::wire::EthernetAddress::from_bytes(&driver_net.mac_address());
    let driver: Arc<VirtioNICDriver<T>> = VirtioNICDriver::new(driver_net);

    kinfo!(
        "Virtio-net driver init successfully!\tNetDevID: [{}], MAC: [{}]",
        driver.name(),
        mac
    );
}

impl<T: Transport> Driver for VirtioNICDriver<T> {
    fn as_any_ref(&'static self) -> &'static dyn core::any::Any {
        self
    }

    fn as_any_mut(&'static mut self) -> &'static mut dyn core::any::Any {
        self
    }
}

impl<T: Transport> NetDriver for VirtioNICDriver<T> {
    fn mac(&self) -> smoltcp::wire::EthernetAddress {
        let mac: [u8; 6] = self.inner.read().virtio_net.mac_address();
        return smoltcp::wire::EthernetAddress::from_bytes(&mac);
    }

    #[inline]
    fn nic_id(&self) -> usize {
        return self.inner.read().net_device_id;
    }

    fn poll(
        &self,
        iface_id: usize,
        sockets: &mut smoltcp::iface::SocketSet,
    ) -> Result<(), crate::syscall::SystemError> {
        let guard = self.inner.upgradeable_read();
        let mut iface: Option<Arc<Interface>> = None;
        for i in guard.ifaces.iter() {
            if i.iface_id() == iface_id {
               iface = Some(i.clone());
               break;
            }
        }
        kdebug!("found iface: {:?}", iface);
        if let Some(iface) = iface {
            kdebug!("to upgrade");
            let mut guard = guard.upgrade();
            kdebug!("VirtioNet: poll: iface_id: {}", iface_id);
            // !!!!在这里会由于双重锁的问题，导致死锁。
            let poll_res = iface.inner_mut().poll(Instant::now().into(), guard.deref_mut(), sockets);
            kdebug!("VirtioNet: poll: poll_res: {:?}", poll_res);
            return Ok(());
        }
        kdebug!("VirtioNet: poll: iface_id not found");
        return Err(SystemError::EINVAL);
    }
    // fn as_any_ref(&'static self) -> &'static dyn core::any::Any {
    //     return self;
    // }
}

/// 向编译器保证，VirtioNICDriver在线程之间是安全的.
/// 由于smoltcp只会在token内真正操作网卡设备，并且在VirtioNetToken的consume
/// 方法内，会对VirtioNet进行加【写锁】，因此，能够保证对设备操作的的互斥访问，
/// 因此VirtioNICDriver在线程之间是安全的。
unsafe impl<T: Transport> Sync for VirtioNICDriver<T> {}
unsafe impl<T: Transport> Send for VirtioNICDriver<T> {}
