//这个文件的绝大部分内容是copy virtio_net.rs的，考虑到所有的驱动都要用操作系统提供的协议栈，我觉得可以把这些内容抽象出来

use alloc::{sync::Arc, string::String};
use smoltcp::{phy::{RxToken, TxToken, Device}, wire};
use core::{
    cell::UnsafeCell,
    fmt::Debug,
    ops::{Deref, DerefMut},
};
use crate::{libs::spinlock::SpinLock, driver::{Driver, net::NetDriver}, syscall::SystemError, time::Instant, net::{generate_iface_id, NET_DRIVERS}, kinfo, kdebug};

use super::e1000e::{E1000EDevice, E1000EBuffer};


pub struct E1000ERxToken(E1000EBuffer);
pub struct E1000ETxToken{
    driver :E1000EDriver
}
pub struct E1000EDriver{
    pub inner: Arc<SpinLock<E1000EDevice>>,
    // pkt_buffer: 
}

/// @brief 网卡驱动的包裹器，这是为了获取网卡驱动的可变引用而设计的。
/// 参阅virtio_net.rs
struct E1000EDriverWrapper(UnsafeCell<E1000EDriver>);
unsafe impl Send for E1000EDriverWrapper {}
unsafe impl Sync for E1000EDriverWrapper {}

impl Deref for E1000EDriverWrapper {
    type Target = E1000EDriver;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.0.get() }
    }
}
impl DerefMut for E1000EDriverWrapper {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.0.get() }
    }
}

impl E1000EDriverWrapper {
    fn force_get_mut(&self) -> &mut E1000EDriver {
        unsafe { &mut *self.0.get() }
    }
}

impl Debug for E1000EDriverWrapper {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtioNICDriver").finish()
    }
}

pub struct E1000EInterface{
    driver: E1000EDriverWrapper,
    iface_id: usize,
    iface: SpinLock<smoltcp::iface::Interface>,
    name: String,
}
impl RxToken for E1000ERxToken{
    fn consume<R, F>(mut self, f: F) -> R
        where
            F: FnOnce(&mut [u8]) -> R {
        let result = f(&mut self.0.as_mut_slice());
        self.0.free_buffer();
        return result;
    }
}

impl TxToken for E1000ETxToken{
    fn consume<R, F>(self, len: usize, f: F) -> R
        where
            F: FnOnce(&mut [u8]) -> R {
        // let mut buffer = [0u8; 4096];
        // let result = f(&mut buffer[..len]);
        // let mut device = self.driver.inner.lock();
        // device.e1000e_transmit(&mut buffer);
        let mut buffer = E1000EBuffer::new(4096);
        let result = f(buffer.as_mut_slice());
        let mut device = self.driver.inner.lock();
        device.e1000e_transmit(buffer);
        return result;
    }
}

impl E1000EDriver{
    pub fn new(device: E1000EDevice) -> Self{
        let mut iface_config = smoltcp::iface::Config::new();

        // todo: 随机设定这个值。
        // 参见 https://docs.rs/smoltcp/latest/smoltcp/iface/struct.Config.html#structfield.random_seed
        iface_config.random_seed = 12345;

        iface_config.hardware_addr = Some(wire::HardwareAddress::Ethernet(
            smoltcp::wire::EthernetAddress(device.mac_address()),
        ));

        let inner: Arc<SpinLock<E1000EDevice>> = Arc::new(SpinLock::new(device));
        let result = E1000EDriver { inner };
        return result;
    }
}

impl Clone for E1000EDriver{
    fn clone(&self) -> Self {
        return E1000EDriver {
            inner: self.inner.clone(),
        };
    }
}

impl Device for E1000EDriver{
    type RxToken<'a> = E1000ERxToken;
    type TxToken<'a> = E1000ETxToken;

    fn receive(&mut self, _timestamp: smoltcp::time::Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        match self.inner.lock().e1000e_receive2(){
            Some(buffer) => {
                Some((
                E1000ERxToken(buffer), 
                E1000ETxToken{driver: self.clone()}
            ))},
            None => {
                return None;
            }
        }
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        match self.inner.lock().e1000e_can_transmit(){
            true => Some(E1000ETxToken{driver: self.clone()}),
            false => {
                None
            }
        } 
    }

    // 临时测试用
    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        let mut caps = smoltcp::phy::DeviceCapabilities::default();
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

impl E1000EInterface{
    pub fn new(mut driver: E1000EDriver) -> Arc<Self>{
        let iface_id = generate_iface_id();
        let mut iface_config = smoltcp::iface::Config::new();

        // todo: 随机设定这个值。
        // 参见 https://docs.rs/smoltcp/latest/smoltcp/iface/struct.Config.html#structfield.random_seed
        iface_config.random_seed = 12345;

        iface_config.hardware_addr = Some(wire::HardwareAddress::Ethernet(
            smoltcp::wire::EthernetAddress(driver.inner.lock().mac_address()),
        ));
        let iface = smoltcp::iface::Interface::new(iface_config, &mut driver);

        let driver: E1000EDriverWrapper = E1000EDriverWrapper(UnsafeCell::new(driver));
        let result = Arc::new(E1000EInterface {
            driver,
            iface_id,
            iface: SpinLock::new(iface),
            name: format!("eth{}", iface_id),
        });

        return result;
    }
}

impl Debug for E1000EInterface {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("E1000EInterface")
            .field("iface_id", &self.iface_id)
            .field("iface", &"smoltcp::iface::Interface")
            .field("name", &self.name)
            .finish()
    }
}
impl Driver for E1000EInterface {
    fn as_any_ref(&'static self) -> &'static dyn core::any::Any {
        self
    }
}

impl NetDriver for E1000EInterface {
    fn mac(&self) -> smoltcp::wire::EthernetAddress {
        let mac = self.driver.inner.lock().mac_address();
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
        //
        // kdebug!("e1000e Interface poll:{poll_res}");
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

pub fn e1000e_driver_init(device: E1000EDevice){
    let mac = smoltcp::wire::EthernetAddress::from_bytes(&device.mac_address());
    let driver = E1000EDriver::new(device);
    let iface = E1000EInterface::new(driver);
    // 将网卡的接口信息注册到全局的网卡接口信息表中
    NET_DRIVERS.write().insert(iface.nic_id(), iface.clone());
    kinfo!(
        "e1000e driver init successfully!\tNetDevID: [{}], MAC: [{}]",
        iface.name(),
        mac
    );
}