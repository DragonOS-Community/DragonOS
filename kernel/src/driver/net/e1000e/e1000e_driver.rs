//这个文件的绝大部分内容是copy virtio_net.rs的，考虑到所有的驱动都要用操作系统提供的协议栈，我觉得可以把这些内容抽象出来

use crate::{
    arch::rand::rand,
    driver::{
        base::{
            class::Class,
            device::{bus::Bus, driver::Driver, Device, DeviceCommonData, DeviceType, IdTable},
            kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
        },
        net::{register_netdevice, NetDeivceState, NetDevice, NetDeviceCommonData, Operstate},
    },
    libs::{
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
    net::{generate_iface_id, NET_DEVICES},
    time::Instant,
};
use alloc::{
    string::{String, ToString},
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
    wire::{self, HardwareAddress},
};
use system_error::SystemError;

use super::e1000e::{E1000EBuffer, E1000EDevice};

const DEVICE_NAME: &str = "e1000e";

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

#[cast_to([sync] NetDevice)]
#[cast_to([sync] Device)]
pub struct E1000EInterface {
    driver: E1000EDriverWrapper,
    iface_id: usize,
    iface: SpinLock<smoltcp::iface::Interface>,
    name: String,
    inner: SpinLock<InnerE1000EInterface>,
    locked_kobj_state: LockedKObjectState,
}

#[derive(Debug)]
pub struct InnerE1000EInterface {
    netdevice_common: NetDeviceCommonData,
    device_common: DeviceCommonData,
    kobj_common: KObjectCommonData,
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

        let driver: E1000EDriverWrapper = E1000EDriverWrapper(UnsafeCell::new(driver));
        let result = Arc::new(E1000EInterface {
            driver,
            iface_id,
            iface: SpinLock::new(iface),
            name: format!("eth{}", iface_id),
            inner: SpinLock::new(InnerE1000EInterface {
                netdevice_common: NetDeviceCommonData::default(),
                device_common: DeviceCommonData::default(),
                kobj_common: KObjectCommonData::default(),
            }),
            locked_kobj_state: LockedKObjectState::default(),
        });

        return result;
    }

    pub fn inner(&self) -> SpinLockGuard<InnerE1000EInterface> {
        return self.inner.lock();
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

impl Device for E1000EInterface {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Net
    }

    fn id_table(&self) -> IdTable {
        IdTable::new(DEVICE_NAME.to_string(), None)
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner().device_common.bus.clone()
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner().device_common.bus = bus;
    }

    fn class(&self) -> Option<Arc<dyn Class>> {
        let mut guard = self.inner();
        let r = guard.device_common.class.clone()?.upgrade();
        if r.is_none() {
            guard.device_common.class = None;
        }

        return r;
    }

    fn set_class(&self, class: Option<Weak<dyn Class>>) {
        self.inner().device_common.class = class;
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        let r = self.inner().device_common.driver.clone()?.upgrade();
        if r.is_none() {
            self.inner().device_common.driver = None;
        }

        return r;
    }

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>) {
        self.inner().device_common.driver = driver;
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn can_match(&self) -> bool {
        self.inner().device_common.can_match
    }

    fn set_can_match(&self, can_match: bool) {
        self.inner().device_common.can_match = can_match;
    }

    fn state_synced(&self) -> bool {
        true
    }

    fn dev_parent(&self) -> Option<Weak<dyn Device>> {
        self.inner().device_common.get_parent_weak_or_clear()
    }

    fn set_dev_parent(&self, parent: Option<Weak<dyn Device>>) {
        self.inner().device_common.parent = parent;
    }
}

impl NetDevice for E1000EInterface {
    fn mac(&self) -> smoltcp::wire::EthernetAddress {
        let mac = self.driver.inner.lock().mac_address();
        return smoltcp::wire::EthernetAddress::from_bytes(&mac);
    }

    #[inline]
    fn nic_id(&self) -> usize {
        return self.iface_id;
    }

    #[inline]
    fn iface_name(&self) -> String {
        return self.name.clone();
    }

    fn update_ip_addrs(&self, ip_addrs: &[wire::IpCidr]) -> Result<(), SystemError> {
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

    fn addr_assign_type(&self) -> u8 {
        return self.inner().netdevice_common.addr_assign_type;
    }

    fn net_device_type(&self) -> u16 {
        self.inner().netdevice_common.net_device_type = 1; // 以太网设备
        return self.inner().netdevice_common.net_device_type;
    }

    fn net_state(&self) -> NetDeivceState {
        return self.inner().netdevice_common.state;
    }

    fn set_net_state(&self, state: NetDeivceState) {
        self.inner().netdevice_common.state |= state;
    }

    fn operstate(&self) -> Operstate {
        return self.inner().netdevice_common.operstate;
    }

    fn set_operstate(&self, state: Operstate) {
        self.inner().netdevice_common.operstate = state;
    }
}

impl KObject for E1000EInterface {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<crate::filesystem::kernfs::KernFSInode>>) {
        self.inner().kobj_common.kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<crate::filesystem::kernfs::KernFSInode>> {
        self.inner().kobj_common.kern_inode.clone()
    }

    fn parent(&self) -> Option<alloc::sync::Weak<dyn KObject>> {
        self.inner().kobj_common.parent.clone()
    }

    fn set_parent(&self, parent: Option<alloc::sync::Weak<dyn KObject>>) {
        self.inner().kobj_common.parent = parent;
    }

    fn kset(&self) -> Option<Arc<crate::driver::base::kset::KSet>> {
        self.inner().kobj_common.kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<crate::driver::base::kset::KSet>>) {
        self.inner().kobj_common.kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn crate::driver::base::kobject::KObjType> {
        self.inner().kobj_common.kobj_type
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn set_name(&self, _name: String) {
        // do nothing
    }

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        self.locked_kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        self.locked_kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.locked_kobj_state.write() = state;
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobj_common.kobj_type = ktype;
    }
}

pub fn e1000e_driver_init(device: E1000EDevice) {
    let mac = smoltcp::wire::EthernetAddress::from_bytes(&device.mac_address());
    let driver = E1000EDriver::new(device);
    let iface = E1000EInterface::new(driver);
    // 标识网络设备已经启动
    iface.set_net_state(NetDeivceState::__LINK_STATE_START);

    // 将网卡的接口信息注册到全局的网卡接口信息表中
    NET_DEVICES
        .write_irqsave()
        .insert(iface.nic_id(), iface.clone());
    info!("e1000e driver init successfully!\tMAC: [{}]", mac);

    register_netdevice(iface.clone()).expect("register lo device failed");
}
