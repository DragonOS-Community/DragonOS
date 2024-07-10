use crate::arch::rand::rand;
use crate::driver::base::class::Class;
use crate::driver::base::device::bus::Bus;
use crate::driver::base::device::driver::Driver;
use crate::driver::base::device::{Device, DeviceType, IdTable};
use crate::driver::base::kobject::{KObjType, KObject, KObjectState};
use crate::libs::spinlock::SpinLock;
use crate::net::{generate_iface_id, NET_DEVICES};
use crate::time::Instant;
use alloc::collections::VecDeque;
use alloc::fmt::Debug;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use log::info;
use smoltcp::wire::HardwareAddress;
use smoltcp::{
    phy::{self},
    wire::{IpAddress, IpCidr},
};
use system_error::SystemError;
use unified_init::define_unified_initializer_slice;
use unified_init::macros::unified_init;

use super::NetDevice;

const DEVICE_NAME: &str = "loopback";

define_unified_initializer_slice!(INITIALIZER_LIST);

// pub struct LoopbackBuffer {
//     buffer: Vec<u8>,
//     length: usize,
// }

// impl LoopbackBuffer {
//     pub fn new(length: usize) -> Self {
//         let buffer = vec![0; length / size_of::<usize>()];
//         LoopbackBuffer{
//             buffer,
//             length,
//         }
//        }

//     pub fn as_mut_slice(&mut self) -> &mut [u8] {
//         self.buffer.as_mut_slice()
//     }
// }

pub struct LoopbackRxToken {
    buffer: Vec<u8>,
}

impl phy::RxToken for LoopbackRxToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(self.buffer.as_mut_slice())
    }
}

pub struct LoopbackTxToken {
    driver: LoopbackDriver,
}

impl phy::TxToken for LoopbackTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = vec![0; len];
        let result = f(buffer.as_mut_slice());
        let mut device = self.driver.inner.lock();
        device.loopback_transmit(buffer);
        result
    }
}

pub struct Loopback {
    //回环设备的transmit缓冲区，
    queue: VecDeque<Vec<u8>>,
}

impl Loopback {
    pub fn new() -> Self {
        let queue = VecDeque::new();
        Loopback { queue }
    }

    pub fn loopback_receive(&mut self) -> Vec<u8> {
        let buffer = self.queue.pop_front();
        match buffer {
            Some(buffer) => {
                return buffer;
            }
            None => {
                return Vec::new();
            }
        }
    }

    pub fn loopback_transmit(&mut self, buffer: Vec<u8>) {
        self.queue.push_back(buffer)
    }
}

//driver的包裹器
//参考virtio_net.rs
struct LoopbackDriverWapper(UnsafeCell<LoopbackDriver>);
unsafe impl Send for LoopbackDriverWapper {}
unsafe impl Sync for LoopbackDriverWapper {}

impl Deref for LoopbackDriverWapper {
    type Target = LoopbackDriver;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.0.get() }
    }
}

impl DerefMut for LoopbackDriverWapper {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.0.get() }
    }
}

impl LoopbackDriverWapper {
    #[allow(clippy::mut_from_ref)]
    fn force_get_mut(&self) -> &mut LoopbackDriver {
        unsafe { &mut *self.0.get() }
    }
}

pub struct LoopbackDriver {
    pub inner: Arc<SpinLock<Loopback>>,
}

impl LoopbackDriver {
    pub fn new() -> Self {
        let inner = Arc::new(SpinLock::new(Loopback::new()));
        LoopbackDriver { inner }
    }
}

impl Clone for LoopbackDriver {
    fn clone(&self) -> Self {
        LoopbackDriver {
            inner: self.inner.clone(),
        }
    }
}

impl phy::Device for LoopbackDriver {
    type RxToken<'a> = LoopbackRxToken;
    type TxToken<'a> = LoopbackTxToken;

    fn capabilities(&self) -> phy::DeviceCapabilities {
        //loopback的最大传输单元
        let mut result = phy::DeviceCapabilities::default();
        result.max_transmission_unit = 65535;
        result.max_burst_size = Some(1);
        return result;
    }
    //收包
    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let buffer = self.inner.lock().loopback_receive();
        let rx = LoopbackRxToken { buffer };
        let tx = LoopbackTxToken {
            driver: self.clone(),
        };
        Option::Some((rx, tx))
    }
    //发包
    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        Some(LoopbackTxToken {
            driver: self.clone(),
        })
    }
}

pub struct LoopbackInterface {
    driver: LoopbackDriverWapper,
    iface_id: usize,
    iface: SpinLock<smoltcp::iface::Interface>,
    name: String,
}

impl LoopbackInterface {
    pub fn new(mut driver: LoopbackDriver) -> Arc<Self> {
        let iface_id = generate_iface_id();
        let mut iface_config = smoltcp::iface::Config::new(HardwareAddress::Ethernet(
            smoltcp::wire::EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]),
        ));
        iface_config.random_seed = rand() as u64;

        let mut iface =
            smoltcp::iface::Interface::new(iface_config, &mut driver, Instant::now().into());
        //设置网卡地址为127.0.0.1
        iface.update_ip_addrs(|ip_addrs| {
            ip_addrs
                .push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8))
                .unwrap();
        });
        let driver = LoopbackDriverWapper(UnsafeCell::new(driver));
        Arc::new(LoopbackInterface {
            driver,
            iface_id,
            iface: SpinLock::new(iface),
            name: "lo".to_string(),
        })
    }
}

impl Debug for LoopbackInterface {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoopbackInterface")
            .field("iface_id", &self.iface_id)
            .field("iface", &"smtoltcp::iface::Interface")
            .field("name", &self.name)
            .finish()
    }
}

impl KObject for LoopbackInterface {
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

impl Device for LoopbackInterface {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Net
    }

    fn id_table(&self) -> IdTable {
        IdTable::new(DEVICE_NAME.to_string(), None)
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
        true
    }
}

impl NetDevice for LoopbackInterface {
    fn mac(&self) -> smoltcp::wire::EthernetAddress {
        let mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        smoltcp::wire::EthernetAddress(mac)
    }

    #[inline]
    fn nic_id(&self) -> usize {
        self.iface_id
    }

    #[inline]
    fn name(&self) -> String {
        self.name.clone()
    }

    fn update_ip_addrs(
        &self,
        ip_addrs: &[smoltcp::wire::IpCidr],
    ) -> Result<(), system_error::SystemError> {
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

    fn poll(&self, sockets: &mut smoltcp::iface::SocketSet) -> Result<(), SystemError> {
        let timestamp: smoltcp::time::Instant = Instant::now().into();
        let mut guard = self.iface.lock();
        let poll_res = guard.poll(timestamp, self.driver.force_get_mut(), sockets);
        if poll_res {
            return Ok(());
        }
        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
    }

    #[inline(always)]
    fn inner_iface(&self) -> &SpinLock<smoltcp::iface::Interface> {
        return &self.iface;
    }
}

pub fn loopback_probe() {
    loopback_driver_init();
}

pub fn loopback_driver_init() {
    let mac = smoltcp::wire::EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
    let driver = LoopbackDriver::new();
    let iface = LoopbackInterface::new(driver);

    NET_DEVICES
        .write_irqsave()
        .insert(iface.iface_id, iface.clone());
    info!("loopback driver init successfully!\tMAC: [{}]", mac);
}

#[unified_init(INITIALIZER_LIST)]
pub fn loopback_init() -> Result<(), SystemError> {
    loopback_probe();
    return Ok(());
}
