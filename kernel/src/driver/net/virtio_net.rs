use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};
use smoltcp::phy;
use virtio_drivers::{device::net::VirtIONet, transport::Transport};

use crate::{
    driver::{generate_nic_id, virtio::virtio_impl::HalImpl, Driver, NET_DRIVERS},
    kdebug, kerror, kinfo,
    libs::rwlock::RwLock,
};

use super::NetDriver;

/// @brief Virtio网络设备驱动(加锁)
pub struct VirtioNICDriver<T: Transport> {
    inner: RwLock<InnerVirtIONet<T>>,
}

impl<T: Transport> VirtioNICDriver<T> {
    pub fn new(driver_net: VirtIONet<HalImpl, T>) -> Arc<Self> {
        let inner = RwLock::new(InnerVirtIONet {
            virtio_net: driver_net,
            self_ref: Weak::new(),
            netface_id: generate_nic_id(),
        });
        let result: Arc<VirtioNICDriver<T>> = Arc::new(Self { inner });
        result.inner.write().self_ref = Arc::downgrade(&result);
        return result;
    }
}

/// @brief Virtio网络设备驱动(不加锁, 仅供内部使用)
struct InnerVirtIONet<T: Transport> {
    /// Virtio网络设备
    virtio_net: VirtIONet<HalImpl, T>,
    /// 自引用
    self_ref: Weak<VirtioNICDriver<T>>,
    /// 网卡ID
    netface_id: usize,
}

pub struct VirtioNetToken<T: Transport> {
    data: Box<[u8]>,
    driver: Arc<VirtioNICDriver<T>>,
}

impl<'a, T: Transport> VirtioNetToken<T> {
    pub fn new(driver: Arc<VirtioNICDriver<T>>) -> Self {
        return Self {
            data: Box::new([0u8; 2000]),
            driver: driver,
        };
    }
}

impl<T: Transport> phy::Device for VirtioNICDriver<T> {
    type RxToken<'a> = VirtioNetToken<T> where Self: 'a;
    type TxToken<'a> = VirtioNetToken<T> where Self: 'a;

    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let driver_net = self.inner.read();
        if driver_net.virtio_net.can_recv() {
            return Some((
                VirtioNetToken::new(driver_net.self_ref.upgrade().unwrap()),
                VirtioNetToken::new(driver_net.self_ref.upgrade().unwrap()),
            ));
        } else {
            return None;
        }
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        let driver_net = self.inner.read();
        if driver_net.virtio_net.can_send() {
            return Some(VirtioNetToken::new(driver_net.self_ref.upgrade().unwrap()));
        } else {
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
    fn consume<R, F>(mut self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let result = f(&mut self.data[..len]);
        // 为了线程安全，这里需要对VirtioNet进行加【写锁】，以保证对设备的互斥访问。
        let mut driver_net = self.driver.inner.write();
        driver_net
            .virtio_net
            .send(&self.data[..len])
            .expect("virtio_net send failed");

        return result;
    }
}

impl<T: Transport> phy::RxToken for VirtioNetToken<T> {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // 为了线程安全，这里需要对VirtioNet进行加【写锁】，以保证对设备的互斥访问。
        let mut driver_net = self.driver.inner.write();
        let len = driver_net
            .virtio_net
            .recv(&mut self.data)
            .expect("virtio_net recv failed");
        return f(&mut self.data[..len]);
    }
}

/// @brief virtio-net 驱动的初始化与测试
pub fn virtio_net<T: Transport + 'static>(transport: T) {
    let driver_net: VirtIONet<HalImpl, T> = match VirtIONet::<HalImpl, T>::new(transport) {
        Ok(net) => {
            net
        }
        Err(_) => {
            kerror!("VirtIONet init failed");
            return;
        }
    };
    let mac = smoltcp::wire::EthernetAddress::from_bytes(&driver_net.mac());
    let driver: Arc<VirtioNICDriver<T>> = VirtioNICDriver::new(driver_net);
    let nic_id = driver.inner.read().netface_id;
    // 将网卡驱动注册到网卡驱动列表中
    NET_DRIVERS.write().insert(nic_id, driver.clone());
    kinfo!(
        "Virtio-net driver init successfully!\tNetface: [{}], MAC: [{}]",
        driver.name(),
        mac
    );
}

impl<T: Transport> Driver for VirtioNICDriver<T> {}
impl<T: Transport> NetDriver for VirtioNICDriver<T> {
    fn mac(&self) -> smoltcp::wire::EthernetAddress {
        let mac: [u8; 6] = self.inner.read().virtio_net.mac();
        return smoltcp::wire::EthernetAddress::from_bytes(&mac);
    }

    #[inline]
    fn nic_id(&self) -> usize {
        return self.inner.read().netface_id;
    }
}

/// 向编译器保证，VirtioNICDriver在线程之间是安全的.
/// 由于smoltcp只会在token内真正操作网卡设备，并且在VirtioNetToken的consume
/// 方法内，会对VirtioNet进行加【写锁】，因此，能够保证对设备操作的的互斥访问，
/// 因此VirtioNICDriver在线程之间是安全的。
unsafe impl<T: Transport> Sync for VirtioNICDriver<T> {}
unsafe impl<T: Transport> Send for VirtioNICDriver<T> {}
