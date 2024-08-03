//这个文件的绝大部分内容是copy virtio_net.rs的，考虑到所有的驱动都要用操作系统提供的协议栈，我觉得可以把这些内容抽象出来

use crate::{
    arch::rand::rand,
    driver::{
        base::{
            class::Class,
            device::{bus::Bus, driver::Driver, Device, DeviceType, IdTable},
            kobject::{KObjType, KObject, KObjectState},
        },
        net::{Iface, IfaceCommon},
    },
    libs::spinlock::SpinLock,
    net::{generate_iface_id, NET_DEVICES},
    time::Instant,
};
use alloc::{
    string::String,
    sync::{Arc, Weak},
};
use core::{
    cell::UnsafeCell,
    fmt::Debug,
    ops::{Deref, DerefMut},
};
use log::info;
use smoltcp::{
    phy,
    wire::HardwareAddress,
};
use system_error::SystemError;

use super::e1000e::{E1000EBuffer, E1000EDevice};

pub struct E1000ERxToken(E1000EBuffer);
pub struct E1000ETxToken {
    driver: E1000EDriver,
}
pub struct E1000EDriver {
    pub inner: Arc<SpinLock<E1000EDevice>>,
}
unsafe impl Send for E1000EDriver {}
unsafe impl Sync for E1000EDriver {}

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
    #[allow(clippy::mut_from_ref)]
    fn force_get_mut(&self) -> &mut E1000EDriver {
        unsafe { &mut *self.0.get() }
    }
}

impl Debug for E1000EDriverWrapper {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("E1000ENICDriver").finish()
    }
}

#[derive(Debug)]
pub struct E1000EInterface {
    driver: E1000EDriverWrapper,
    common: IfaceCommon,
    name: String,
}
impl phy::RxToken for E1000ERxToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let result = f(self.0.as_mut_slice());
        self.0.free_buffer();
        return result;
    }
}

impl phy::TxToken for E1000ETxToken {
    fn consume<R, F>(self, _len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = E1000EBuffer::new(4096);
        let result = f(buffer.as_mut_slice());
        let mut device = self.driver.inner.lock();
        device.e1000e_transmit(buffer);
        buffer.free_buffer();
        return result;
    }
}

impl E1000EDriver {
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new(device: E1000EDevice) -> Self {
        let mut iface_config = smoltcp::iface::Config::new(HardwareAddress::Ethernet(
            smoltcp::wire::EthernetAddress(device.mac_address()),
        ));

        iface_config.random_seed = rand() as u64;

        let inner: Arc<SpinLock<E1000EDevice>> = Arc::new(SpinLock::new(device));
        let result = E1000EDriver { inner };
        return result;
    }
}

impl Clone for E1000EDriver {
    fn clone(&self) -> Self {
        return E1000EDriver {
            inner: self.inner.clone(),
        };
    }
}

impl phy::Device for E1000EDriver {
    type RxToken<'a> = E1000ERxToken;
    type TxToken<'a> = E1000ETxToken;

    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        match self.inner.lock().e1000e_receive() {
            Some(buffer) => Some((
                E1000ERxToken(buffer),
                E1000ETxToken {
                    driver: self.clone(),
                },
            )),
            None => {
                return None;
            }
        }
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        match self.inner.lock().e1000e_can_transmit() {
            true => Some(E1000ETxToken {
                driver: self.clone(),
            }),
            false => None,
        }
    }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        let mut caps = smoltcp::phy::DeviceCapabilities::default();
        // 网卡的最大传输单元. 请与IP层的MTU进行区分。这个值应当是网卡的最大传输单元，而不是IP层的MTU。
        // The maximum size of the received packet is limited by the 82574 hardware to 1536 bytes. Packets larger then 1536 bytes are silently discarded. Any packet smaller than 1536 bytes is processed by the 82574.
        // 82574l manual pp205
        caps.max_transmission_unit = 1536;
        /*
           Maximum burst size, in terms of MTU.
           The network device is unable to send or receive bursts large than the value returned by this function.
           If None, there is no fixed limit on burst size, e.g. if network buffers are dynamically allocated.
        */
        caps.max_burst_size = Some(1);
        return caps;
    }
}

impl E1000EInterface {
    pub fn new(mut driver: E1000EDriver) -> Arc<Self> {
        let iface_id = generate_iface_id();
        let mut iface_config = smoltcp::iface::Config::new(HardwareAddress::Ethernet(
            smoltcp::wire::EthernetAddress(driver.inner.lock().mac_address()),
        ));
        iface_config.random_seed = rand() as u64;

        let iface =
            smoltcp::iface::Interface::new(iface_config, &mut driver, Instant::now().into());

        let result = Arc::new(E1000EInterface {
            driver: E1000EDriverWrapper(UnsafeCell::new(driver)),
            common: IfaceCommon::new(iface_id, iface),
            name: format!("eth{}", iface_id),
        });

        return result;
    }
}

impl Device for E1000EInterface {
    fn dev_type(&self) -> DeviceType {
        todo!()
    }

    fn id_table(&self) -> IdTable {
        todo!()
    }

    fn set_bus(&self, _bus: Option<Weak<dyn Bus>>) {
        todo!()
    }

    fn set_class(&self, _class: Option<Weak<dyn Class>>) {
        todo!()
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        todo!()
    }

    fn set_driver(&self, _driver: Option<Weak<dyn Driver>>) {
        todo!()
    }

    fn is_dead(&self) -> bool {
        todo!()
    }

    fn can_match(&self) -> bool {
        todo!()
    }

    fn set_can_match(&self, _can_match: bool) {
        todo!()
    }

    fn state_synced(&self) -> bool {
        todo!()
    }
}

impl Iface for E1000EInterface {
    fn common(&self) -> &IfaceCommon {
        return &self.common;
    }

    fn mac(&self) -> smoltcp::wire::EthernetAddress {
        let mac = self.driver.inner.lock().mac_address();
        return smoltcp::wire::EthernetAddress::from_bytes(&mac);
    }

    #[inline]
    fn name(&self) -> String {
        return self.name.clone();
    }

    fn poll(&self) -> Result<(), SystemError> {
        self.common.poll(self.driver.force_get_mut())
    }
}

impl KObject for E1000EInterface {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn set_inode(&self, _inode: Option<Arc<crate::filesystem::kernfs::KernFSInode>>) {
        todo!()
    }

    fn inode(&self) -> Option<Arc<crate::filesystem::kernfs::KernFSInode>> {
        todo!()
    }

    fn parent(&self) -> Option<alloc::sync::Weak<dyn KObject>> {
        todo!()
    }

    fn set_parent(&self, _parent: Option<alloc::sync::Weak<dyn KObject>>) {
        todo!()
    }

    fn kset(&self) -> Option<Arc<crate::driver::base::kset::KSet>> {
        todo!()
    }

    fn set_kset(&self, _kset: Option<Arc<crate::driver::base::kset::KSet>>) {
        todo!()
    }

    fn kobj_type(&self) -> Option<&'static dyn crate::driver::base::kobject::KObjType> {
        todo!()
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn set_name(&self, _name: String) {
        todo!()
    }

    fn kobj_state(
        &self,
    ) -> crate::libs::rwlock::RwLockReadGuard<crate::driver::base::kobject::KObjectState> {
        todo!()
    }

    fn kobj_state_mut(
        &self,
    ) -> crate::libs::rwlock::RwLockWriteGuard<crate::driver::base::kobject::KObjectState> {
        todo!()
    }

    fn set_kobj_state(&self, _state: KObjectState) {
        todo!()
    }

    fn set_kobj_type(&self, _ktype: Option<&'static dyn KObjType>) {
        todo!()
    }
}

pub fn e1000e_driver_init(device: E1000EDevice) {
    let mac = smoltcp::wire::EthernetAddress::from_bytes(&device.mac_address());
    let driver = E1000EDriver::new(device);
    let iface = E1000EInterface::new(driver);
    // 将网卡的接口信息注册到全局的网卡接口信息表中
    NET_DEVICES
        .write_irqsave()
        .insert(iface.nic_id(), iface.clone());
    info!("e1000e driver init successfully!\tMAC: [{}]", mac);
}
