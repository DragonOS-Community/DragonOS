use core::{cell::UnsafeCell, ops::DerefMut};
use core::ops::Deref;
use alloc::fmt::Debug;
use alloc::string::{String, ToString};
use alloc::rc::Weak;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use alloc::sync::Arc;
use smoltcp::time::Instant;
use smoltcp::wire;
use smoltcp::{iface, phy::{self, DeviceCapabilities, TxToken}, wire::{EthernetAddress, IpAddress, IpCidr}};
use system_error::SystemError;
use crate::driver::base::class::Class;
use crate::driver::base::device::bus::Bus;
use crate::driver::base::device::{Device, DeviceType, IdTable};
use crate::driver::base::device::driver::Driver;
use crate::driver::base::device::DeviceCommonData;
use crate::driver::base::kobject::{KObject, KObjectCommonData, LockedKObjectState};
use crate::driver::pci::attr::DeviceID;
use crate::libs::spinlock::SpinLockGuard;
use crate::net::generate_iface_id;
use crate::libs::spinlock::SpinLock;

use super::NetDevice;

const DEVICE_NAME: &str = "loopback";

//定义Loopback网络接口
pub struct LoopbackInterface {
    device_inner: LoopbackDeviceInnerWapper,
    iface_id: usize,
    iface_name: String,
    dev_id: Arc<DeviceID>,
    iface: SpinLock<iface::Interface>,
    inner: SpinLock<InnerLoopbackInterface>,
    locked_kobj_state: LockedKObjectState,
}

struct InnerLoopbackInterface {
    name: Option<String>,
    device_common: DeviceCommonData,
    kobj_common: KObjectCommonData,
}

impl core::fmt::Debug for LoopbackDeviceInner {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoopbackInterface")
            .field("iface_id", &self.iface_id)
            .field("iface_name", &self.iface_name)
            .field("dev_id", &self.dev_id)
            .field("inner", &self.inner)
            .field("locked_kobj_state", &self.locked_kobj_state)
            .finish()
    }
}

impl LoopbackInterface {
    pub fn new(mut device_inner: LoopbackDeviceInner, dev_id: Arc<DeviceID>) -> Arc<Self> {
        let iface_id = generate_iface_id();
        let hardware_addrs = wire::EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]).into();
        let mut iface_config = iface::Config::new(hardware_addrs);

        let iface = iface::Interface::new(iface_config, &mut dev_id, Instant::now().into());
        iface.update_ip_addrs(|ip_addrs| {
            ip_addrs.push(wire::IpCidr::new(IpAddress::v4(127,0, 0, 1), 8)).unwrap();
        });
        dosf
        let result = Arc::new(LoopbackInterface {
            device_inner: LoopbackDeviceInnerWapper(UnsafeCell::new(device_inner)),
            iface_id,
            locked_kobj_state: LockedKObjectState::default(),
            iface: SpinLock::new(iface),
            iface_name: format!("eth{}", iface_id),
            dev_id,
            inner: SpinLock::new(InnerLoopbackInterface{
                name: None,
                device_common: DeviceCommonData::default(),
                kobj_common: KObjectCommonData::default(),
            })
        });

        result.inner().device_common.driver = 
            Some(Arc::downgrade(None)) as Weak<dyn Driver>;

        return result;
    }

    fn inner(&self) -> SpinLockGuard<InnerLoopbackInterface> {
        return self.inner.lock();
    }
    #[allow(dead_code)]
    pub fn iface_name(&self) -> String{
        self.iface_name.clone()
    }
}

impl Device for LoopbackInterface {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Net
    }

    fn id_table(&self) -> IdTable {
        IdTable::new(DEVICE_NAME.to_string(), None)
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner().device_common.bus.clone();
    }

    fn set_bus(&self, bus:Option<Weak<dyn Bus>>) {
        self.inner().device_common.bus = bus;
    }

    fn class(&self) -> Option<Arc<dyn Class>> {
        let mut guard = self.inner();
        let r = guard.device_common.class.clone();
        if r.is_none() {
            guard.device_common.class = None;
        }
         

         return r;
    }

    fn set_class(&self, class: Option<Weak<dyn Class>>) {
        self.inner().device_common.class = class;
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        let r = self.inner().device_common.driver;
        if r.is_none() {
            self.inner().device_common.driver = None;
        }
        return r;
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn can_match(&self) -> bool {
        self.inner().device_common.can_match
    }

    fn set_can_match(&self, can_match:bool) {
        self.inner().device_common.can_match = can_match;
    }

    fn state_synced(&self) -> bool {
        true
    }

    fn set_driver(&self, driver: Option<alloc::sync::Weak<dyn Driver>>) {
        dfafda
    }

}

impl NetDevice for LoopbackInterface {
    fn update_ip_addrs(&self, ip_addrs: &[wire::IpCidr]) -> Result<(), system_error::SystemError> {
        dfsaf
        Ok(())
    }

    fn mac(&self) -> wire::EthernetAddress {
        EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01])
    }

    #[inline]
    fn nic_id(&self) -> usize {
        return self.iface_id;
    }

    #[inline]
    fn name(&self) -> String {
        return self.iface_name.clone();
    }

    fn poll(&self, sockets: &mut iface::SocketSet) -> Result<(), system_error::SystemError> {
        let timestamp: smoltcp::time::Instant = Instant::now().into();
        let mut guard = self.iface.lock();
        let poll_res = guard.poll(timestamp, self.device_inner.force_get_mut(), sockets);
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
}

impl KObject for LoopbackInterface {
    
}

pub struct LoopbackDeviceInnerWapper(UnsafeCell<LoopbackDeviceInner>);
unsafe impl Send for LoopbackDeviceInnerWapper {}
unsafe impl Sync for LoopbackDeviceInnerWapper {}

impl Deref for LoopbackDeviceInnerWapper {
    type Target = LoopbackDeviceInner;
    fn deref(&self) -> &Self::Target {
        unsafe{&*self.0.get()}
    }
}

impl DerefMut for LoopbackDeviceInnerWapper {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.0.get() }
    }
}

#[allow(clippy::mut_from_ref)]
impl LoopbackDeviceInnerWapper {
    fn force_get_mut(&self) -> &mut <LoopbackDeviceInnerWapper as Deref>::Target {
        unsafe {&mut *self.0.get()}
    }
}

//Loopback网络设备驱动（加锁）
pub struct LoopbackDeviceInner{
    pub inner: Arc<SpinLock<Loopback>>,
}

impl LoopbackDeviceInner {
    pub fn new(loopback: Loopback) -> Self {
        let inner = Arc::new(SpinLock::new(loopback));
        let result = LoopbackDeviceInner{
            inner,
        };
        result
    }
}

#[derive(Debug)]
pub struct Loopback {
    queue: VecDeque<Vec<u8>>,
}

impl Loopback {
    //创建一个loopback设备
    pub fn new() {
        Loopback {
            queue: VecDeque::new(),
        }
    }
}

impl phy::Device for LoopbackDeviceInner {
    type RxToken<'a> = LoopbackRxToken;
    type TxToken<'a> = LoopbackTxToken<'a>;

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
        timestamp: smoltcp::time::Instant
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.inner.lock().queue.pop_front().map(move |buffer| {
            let rx = LoopbackRxToken {buffer};
            let tx = LoopbackTxToken {
                queue: &mut self.inner.lock().queue,
            };
            (rx, tx)
        })
    }
    //发包
    fn transmit(
        &mut self, 
        timestamp: smoltcp::time::Instant
    ) -> Option<Self::TxToken<'_>> {
        Some(LoopbackTxToken{
            queue: &mut self.inner.lock().queue,
        })    
    }
}

impl Clone for LoopbackDeviceInner {
    fn clone(&self) -> Self {
        return LoopbackDeviceInner{
            inner: self.inner.clone(),
        }
    }
}

pub struct LoopbackRxToken{
    buffer: Vec<u8>,
}

impl phy::RxToken for LoopbackRxToken {
    fn consume<R, F>(mut self, f: F) -> R
        where
            F: FnOnce(&mut [u8]) -> R {
        f(&mut self.buffer)
    }
}

pub struct LoopbackTxToken<'a> {
    queue: &'a mut VecDeque<Vec<u8>>,
}

impl<'a> phy::TxToken for LoopbackTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
        where
            F: FnOnce(&mut [u8]) -> R {
        let mut buffer = Vec::new();
        buffer.resize(len, 0);
        let result = f(&self, buffer);
        self.queue.push_back(buffer);
        result
    }
}