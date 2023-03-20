use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};
use smoltcp::phy;
use virtio_drivers::{device::net::VirtIONet, transport::Transport};

use crate::{
    driver::{virtio::virtio_impl::HalImpl, Driver},
    kdebug, kerror,
    libs::rwlock::RwLock,
};

pub struct VirtioNICDriver<T: Transport> {
    inner: RwLock<InnerVirtIONet<T>>,
}

impl<T: Transport> VirtioNICDriver<T> {
    pub fn new(driver_net: VirtIONet<HalImpl, T>) -> Arc<Self> {
        let inner = RwLock::new(InnerVirtIONet {
            virtio_net: driver_net,
            self_ref: Weak::new(),
        });
        let result: Arc<VirtioNICDriver<T>> = Arc::new(Self {
            inner,
        });
        result.inner.write().self_ref = Arc::downgrade(&result);
        return result;
    }
}

struct InnerVirtIONet<T: Transport> {
    virtio_net: VirtIONet<HalImpl, T>,
    self_ref: Weak<VirtioNICDriver<T>>,
}

pub struct VirtIONetToken<T: Transport> {
    data: Box<[u8]>,
    driver: Arc<VirtioNICDriver<T>>,
}

impl<'a, T: Transport> VirtIONetToken<T> {
    pub fn new(driver: Arc<VirtioNICDriver<T>>) -> Self {
        return Self {
            data: Box::new([0u8; 2000]),
            driver: driver,
        };
    }
}

impl<T: Transport> phy::Device for VirtioNICDriver<T> {
    type RxToken<'a> = VirtIONetToken<T> where Self: 'a;
    type TxToken<'a> = VirtIONetToken<T> where Self: 'a;

    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let driver_net = self.inner.read();
        if driver_net.virtio_net.can_recv() {
            return Some((
                VirtIONetToken::new(driver_net.self_ref.upgrade().unwrap()),
                VirtIONetToken::new(driver_net.self_ref.upgrade().unwrap()),
            ));
        } else {
            return None;
        }
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        let driver_net = self.inner.read();
        if driver_net.virtio_net.can_send() {
            return Some(VirtIONetToken::new(driver_net.self_ref.upgrade().unwrap()));
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

impl<T: Transport> phy::TxToken for VirtIONetToken<T> {
    fn consume<R, F>(mut self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let result = f(&mut self.data[..len]);
        let mut driver_net = self.driver.inner.write();
        driver_net
            .virtio_net
            .send(&self.data[..len])
            .expect("virtio_net send failed");

        return result;
    }
}

impl<T: Transport> phy::RxToken for VirtIONetToken<T> {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut driver_net = self.driver.inner.write();
        let len = driver_net
            .virtio_net
            .recv(&mut self.data)
            .expect("virtio_net recv failed");
        return f(&mut self.data[..len]);
    }
}

/// @brief virtio-net 驱动的初始化与测试
pub fn virtio_net<T: Transport>(transport: T) {
    let driver_net: VirtIONet<HalImpl, T> = match VirtIONet::<HalImpl, T>::new(transport) {
        Ok(net) => {
            kdebug!("Virtio-net driver init successfully.");
            net
        }
        Err(_) => {
            kerror!("VirtIONet init failed");
            return;
        }
    };
    let mac = driver_net.mac();
    kdebug!("virtio_net MAC={:?}", mac);
    let driver: Arc<VirtioNICDriver<T>> = VirtioNICDriver::new(driver_net);


    kdebug!("virtio-net test finished");
}

impl<T:Transport> Driver for VirtioNICDriver<T> {

}