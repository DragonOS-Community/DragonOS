use core::{
    cell::UnsafeCell,
    fmt::Debug,
    ops::{Deref, DerefMut},
};

use alloc::{string::String, sync::Arc};
use smoltcp::{phy, wire};
use virtio_drivers::{device::net::VirtIONet, transport::Transport};

use crate::{
    driver::{virtio::virtio_impl::HalImpl, Driver},
    kerror, kinfo,
    libs::spinlock::SpinLock,
    net::{generate_iface_id, NET_DRIVERS},
    syscall::SystemError,
    time::Instant,
};

use super::NetDriver;

/// @brief Virtio网络设备驱动(加锁)
pub struct VirtioNICDriver<T: Transport> {
    pub inner: Arc<SpinLock<VirtIONet<HalImpl, T, 2>>>,
}

impl<T: Transport> Clone for VirtioNICDriver<T> {
    fn clone(&self) -> Self {
        return VirtioNICDriver {
            inner: self.inner.clone(),
        };
    }
}

/// @brief 网卡驱动的包裹器，这是为了获取网卡驱动的可变引用而设计的。
/// 由于smoltcp的设计，导致需要在poll的时候获取网卡驱动的可变引用，
/// 同时需要在token的consume里面获取可变引用。为了避免双重加锁，所以需要这个包裹器。
struct VirtioNICDriverWrapper<T: Transport>(UnsafeCell<VirtioNICDriver<T>>);
unsafe impl<T: Transport> Send for VirtioNICDriverWrapper<T> {}
unsafe impl<T: Transport> Sync for VirtioNICDriverWrapper<T> {}

impl<T: Transport> Deref for VirtioNICDriverWrapper<T> {
    type Target = VirtioNICDriver<T>;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.0.get() }
    }
}
impl<T: Transport> DerefMut for VirtioNICDriverWrapper<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.0.get() }
    }
}

impl<T: Transport> VirtioNICDriverWrapper<T> {
    fn force_get_mut(&self) -> &mut VirtioNICDriver<T> {
        unsafe { &mut *self.0.get() }
    }
}

impl<T: Transport> Debug for VirtioNICDriver<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtioNICDriver").finish()
    }
}

pub struct VirtioInterface<T: Transport> {
    driver: VirtioNICDriverWrapper<T>,
    iface_id: usize,
    iface: SpinLock<smoltcp::iface::Interface>,
    name: String,
}

impl<T: Transport> Debug for VirtioInterface<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtioInterface")
            .field("driver", self.driver.deref())
            .field("iface_id", &self.iface_id)
            .field("iface", &"smoltcp::iface::Interface")
            .field("name", &self.name)
            .finish()
    }
}

impl<T: Transport> VirtioInterface<T> {
    pub fn new(mut driver: VirtioNICDriver<T>) -> Arc<Self> {
        let iface_id = generate_iface_id();
        let mut iface_config = smoltcp::iface::Config::new();

        // todo: 随机设定这个值。
        // 参见 https://docs.rs/smoltcp/latest/smoltcp/iface/struct.Config.html#structfield.random_seed
        iface_config.random_seed = 12345;

        iface_config.hardware_addr = Some(wire::HardwareAddress::Ethernet(
            smoltcp::wire::EthernetAddress(driver.inner.lock().mac_address()),
        ));
        let iface = smoltcp::iface::Interface::new(iface_config, &mut driver);

        let driver: VirtioNICDriverWrapper<T> = VirtioNICDriverWrapper(UnsafeCell::new(driver));
        let result = Arc::new(VirtioInterface {
            driver,
            iface_id,
            iface: SpinLock::new(iface),
            name: format!("eth{}", iface_id),
        });

        return result;
    }
}

impl<T: 'static + Transport> VirtioNICDriver<T> {
    pub fn new(driver_net: VirtIONet<HalImpl, T, 2>) -> Self {
        let mut iface_config = smoltcp::iface::Config::new();

        // todo: 随机设定这个值。
        // 参见 https://docs.rs/smoltcp/latest/smoltcp/iface/struct.Config.html#structfield.random_seed
        iface_config.random_seed = 12345;

        iface_config.hardware_addr = Some(wire::HardwareAddress::Ethernet(
            smoltcp::wire::EthernetAddress(driver_net.mac_address()),
        ));

        let inner: Arc<SpinLock<VirtIONet<HalImpl, T, 2>>> = Arc::new(SpinLock::new(driver_net));
        let result = VirtioNICDriver { inner };
        return result;
    }
}

pub struct VirtioNetToken<T: Transport> {
    driver: VirtioNICDriver<T>,
    rx_buffer: Option<virtio_drivers::device::net::RxBuffer>,
}

impl<'a, T: Transport> VirtioNetToken<T> {
    pub fn new(
        driver: VirtioNICDriver<T>,
        rx_buffer: Option<virtio_drivers::device::net::RxBuffer>,
    ) -> Self {
        return Self { driver, rx_buffer };
    }
}

impl<T: Transport> phy::Device for VirtioNICDriver<T> {
    type RxToken<'a> = VirtioNetToken<T> where Self: 'a;
    type TxToken<'a> = VirtioNetToken<T> where Self: 'a;

    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        match self.inner.lock().receive() {
            Ok(buf) => Some((
                VirtioNetToken::new(self.clone(), Some(buf)),
                VirtioNetToken::new(self.clone(), None),
            )),
            Err(virtio_drivers::Error::NotReady) => None,
            Err(err) => panic!("VirtIO receive failed: {}", err),
        }
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        // kdebug!("VirtioNet: transmit");
        if self.inner.lock().can_send() {
            // kdebug!("VirtioNet: can send");
            return Some(VirtioNetToken::new(self.clone(), None));
        } else {
            // kdebug!("VirtioNet: can not send");
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

        let mut driver_net = self.driver.inner.lock();
        let mut tx_buf = driver_net.new_tx_buffer(len);
        let result = f(tx_buf.packet_mut());
        driver_net.send(tx_buf).expect("virtio_net send failed");
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
            .lock()
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
    let driver: VirtioNICDriver<T> = VirtioNICDriver::new(driver_net);
    let iface = VirtioInterface::new(driver);
    // 将网卡的接口信息注册到全局的网卡接口信息表中
    NET_DRIVERS.write().insert(iface.nic_id(), iface.clone());
    kinfo!(
        "Virtio-net driver init successfully!\tNetDevID: [{}], MAC: [{}]",
        iface.name(),
        mac
    );
}

impl<T: Transport> Driver for VirtioInterface<T> {
    fn as_any_ref(&'static self) -> &'static dyn core::any::Any {
        self
    }
}

impl<T: Transport> NetDriver for VirtioInterface<T> {
    fn mac(&self) -> smoltcp::wire::EthernetAddress {
        let mac: [u8; 6] = self.driver.inner.lock().mac_address();
        return smoltcp::wire::EthernetAddress::from_bytes(&mac);
    }

    #[inline]
    fn nic_id(&self) -> usize {
        return self.iface_id;
    }

    #[inline]
    fn name(&self) -> String {
        return self.name.clone();
    }

    fn update_ip_addrs(&self, ip_addrs: &[wire::IpCidr]) -> Result<(), SystemError> {
        if ip_addrs.len() != 1 {
            return Err(SystemError::EINVAL);
        }

        self.iface.lock().update_ip_addrs(|addrs| {
            let dest = addrs.iter_mut().next();
            if let None = dest {
                addrs.push(ip_addrs[0]).expect("Push ipCidr failed: full");
            } else {
                let dest = dest.unwrap();
                *dest = ip_addrs[0];
            }
        });
        return Ok(());
    }

    fn poll(
        &self,
        sockets: &mut smoltcp::iface::SocketSet,
    ) -> Result<(), crate::syscall::SystemError> {
        let timestamp: smoltcp::time::Instant = Instant::now().into();
        let mut guard = self.iface.lock();
        let poll_res = guard.poll(timestamp, self.driver.force_get_mut(), sockets);
        // todo: notify!!!
        // kdebug!("Virtio Interface poll:{poll_res}");
        if poll_res {
            return Ok(());
        }
        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
    }

    #[inline(always)]
    fn inner_iface(&self) -> &SpinLock<smoltcp::iface::Interface> {
        return &self.iface;
    }
    // fn as_any_ref(&'static self) -> &'static dyn core::any::Any {
    //     return self;
    // }
}

// 向编译器保证，VirtioNICDriver在线程之间是安全的.
// 由于smoltcp只会在token内真正操作网卡设备，并且在VirtioNetToken的consume
// 方法内，会对VirtioNet进行加【写锁】，因此，能够保证对设备操作的的互斥访问，
// 因此VirtioNICDriver在线程之间是安全的。
// unsafe impl<T: Transport> Sync for VirtioNICDriver<T> {}
// unsafe impl<T: Transport> Send for VirtioNICDriver<T> {}
