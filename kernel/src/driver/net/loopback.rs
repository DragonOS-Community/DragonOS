use crate::arch::rand::rand;
use crate::driver::base::class::Class;
use crate::driver::base::device::bus::Bus;
use crate::driver::base::device::driver::Driver;
use crate::driver::base::device::{Device, DeviceCommonData, DeviceType, IdTable};
use crate::driver::base::kobject::{
    KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState,
};
use crate::driver::base::kset::KSet;
use crate::filesystem::kernfs::KernFSInode;
use crate::init::initcall::INITCALL_DEVICE;
use crate::libs::rwlock::{RwLockReadGuard, RwLockWriteGuard};
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::net::{generate_iface_id, NET_DEVICES};
use crate::time::Instant;
use alloc::collections::VecDeque;
use alloc::fmt::Debug;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use smoltcp::wire::HardwareAddress;
use smoltcp::{
    phy::{self},
    wire::{IpAddress, IpCidr},
};
use system_error::SystemError;
use unified_init::macros::unified_init;

use super::{register_netdevice, NetDeivceState, NetDevice, NetDeviceCommonData, Operstate};

const DEVICE_NAME: &str = "loopback";

/// ## 环回接收令牌
/// 用于储存lo网卡接收到的数据
pub struct LoopbackRxToken {
    buffer: Vec<u8>,
}

impl phy::RxToken for LoopbackRxToken {
    /// ## 实现Rxtoken的consume函数
    /// 接受一个函数 `f`，并在 `self.buffer` 上调用它。
    ///
    /// ## 参数
    /// - mut self ：一个可变的 `LoopbackRxToken` 实例。
    /// - f ：接受一个可变的 u8 切片，并返回类型 `R` 的结果。
    ///
    /// ## 返回值
    /// 返回函数 `f` 在 `self.buffer` 上的调用结果。
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(self.buffer.as_mut_slice())
    }
}

/// ## 环回发送令牌
/// 返回驱动用于操作lo设备
pub struct LoopbackTxToken {
    driver: LoopbackDriver,
}

impl phy::TxToken for LoopbackTxToken {
    /// ## 实现TxToken的consume函数
    /// 向lo的队列推入待发送的数据报，实现环回
    ///
    /// ## 参数
    /// - self
    /// - len：数据包的长度
    /// - f：接受一个可变的 u8 切片，并返回类型 `R` 的结果。
    ///
    /// ## 返回值
    /// 返回f对数据包操纵的结果
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

/// ## Loopback设备
/// 成员是一个队列，用来存放接受到的数据包。
/// 当使用lo发送数据包时，不会把数据包传到link层，而是直接发送到该队列，实现环回。
pub struct Loopback {
    //回环设备的缓冲区,接受的数据包会存放在这里，发送的数据包也会发送到这里，实现环回
    queue: VecDeque<Vec<u8>>,
}

impl Loopback {
    /// ## Loopback创建函数
    /// 创建lo设备
    pub fn new() -> Self {
        let queue = VecDeque::new();
        Loopback { queue }
    }
    /// ## Loopback处理接受到的数据包函数
    /// Loopback接受到数据后会调用这个函数来弹出接收的数据，返回给协议栈
    ///
    /// ## 参数
    /// - &mut self ：自身可变引用
    ///
    /// ## 返回值
    /// - queue的头部数据包
    pub fn loopback_receive(&mut self) -> Vec<u8> {
        let buffer = self.queue.pop_front();
        match buffer {
            Some(buffer) => {
                //debug!("lo receive:{:?}", buffer);
                return buffer;
            }
            None => {
                return Vec::new();
            }
        }
    }
    /// ## Loopback发送数据包的函数
    /// Loopback发送数据包给自己的接收队列，实现环回
    ///
    /// ## 参数
    /// - &mut self：自身可变引用
    /// - buffer：需要发送的数据包
    pub fn loopback_transmit(&mut self, buffer: Vec<u8>) {
        //debug!("lo transmit!");
        self.queue.push_back(buffer)
    }
}

/// ## driver的包裹器
/// 为实现获得不可变引用的Interface的内部可变性，故为Driver提供UnsafeCell包裹器
///
/// 参考virtio_net.rs
struct LoopbackDriverWapper(UnsafeCell<LoopbackDriver>);
unsafe impl Send for LoopbackDriverWapper {}
unsafe impl Sync for LoopbackDriverWapper {}

/// ## deref 方法返回一个指向 `LoopbackDriver` 的引用。
impl Deref for LoopbackDriverWapper {
    type Target = LoopbackDriver;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.0.get() }
    }
}
/// ## `deref_mut` 方法返回一个指向可变 `LoopbackDriver` 的引用。
impl DerefMut for LoopbackDriverWapper {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.0.get() }
    }
}

impl LoopbackDriverWapper {
    /// ## force_get_mut返回一个指向可变 `LoopbackDriver` 的引用。
    #[allow(clippy::mut_from_ref)]
    #[allow(clippy::mut_from_ref)]
    fn force_get_mut(&self) -> &mut LoopbackDriver {
        unsafe { &mut *self.0.get() }
    }
}

/// ## Loopback驱动
/// 负责操作Loopback设备实现基本的网卡功能
pub struct LoopbackDriver {
    pub inner: Arc<SpinLock<Loopback>>,
}

impl LoopbackDriver {
    /// ## LoopbackDriver创建函数
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
    type RxToken<'a>
        = LoopbackRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = LoopbackTxToken
    where
        Self: 'a;
    /// ## 返回设备的物理层特性。
    /// lo设备的最大传输单元为65535，最大突发大小为1，传输介质默认为Ethernet
    fn capabilities(&self) -> phy::DeviceCapabilities {
        let mut result = phy::DeviceCapabilities::default();
        result.max_transmission_unit = 65535;
        result.max_burst_size = Some(1);
        result.medium = smoltcp::phy::Medium::Ethernet;
        return result;
    }
    /// ## Loopback驱动处理接受数据事件
    /// 驱动调用Loopback的receive函数，处理buffer封装成（rx，tx）返回给上层
    ///
    /// ## 参数
    /// - `&mut self` ：自身可变引用
    /// - `_timestamp`
    ///
    /// ## 返回值
    /// - None: 如果接收队列为空，返回 `None`，以通知上层没有可以接收的包
    /// - Option::Some((rx, tx))：如果接收队列不为空，返回 `Some`，其中包含一个接收令牌 `rx` 和一个发送令牌 `tx`
    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let buffer = self.inner.lock().loopback_receive();
        //receive队列为为空，返回NONE值以通知上层没有可以receive的包
        if buffer.is_empty() {
            return Option::None;
        }
        let rx = LoopbackRxToken { buffer };
        let tx = LoopbackTxToken {
            driver: self.clone(),
        };
        return Option::Some((rx, tx));
    }
    /// ## Loopback驱动处理发送数据包事件
    /// Loopback驱动在需要发送数据时会调用这个函数来获取一个发送令牌。
    ///
    /// ## 参数
    /// - `&mut self` ：自身可变引用
    /// - `_timestamp`
    ///
    /// ## 返回值
    /// - 返回一个 `Some`，其中包含一个发送令牌，该令牌包含一个对自身的克隆引用
    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        Some(LoopbackTxToken {
            driver: self.clone(),
        })
    }
}

/// ## LoopbackInterface结构
/// 封装驱动包裹器和iface，设置接口名称
#[cast_to([sync] NetDevice)]
#[cast_to([sync] Device)]
pub struct LoopbackInterface {
    driver: LoopbackDriverWapper,
    iface_id: usize,
    iface: SpinLock<smoltcp::iface::Interface>,
    name: String,
    inner: SpinLock<InnerLoopbackInterface>,
    locked_kobj_state: LockedKObjectState,
}

#[derive(Debug)]
pub struct InnerLoopbackInterface {
    netdevice_common: NetDeviceCommonData,
    device_common: DeviceCommonData,
    kobj_common: KObjectCommonData,
}

impl LoopbackInterface {
    /// ## `new` 是一个公共函数，用于创建一个新的 `LoopbackInterface` 实例。
    /// 生成一个新的接口 ID。创建一个新的接口配置，设置其硬件地址和随机种子，使用接口配置和驱动器创建一个新的 `smoltcp::iface::Interface` 实例。
    /// 设置接口的 IP 地址为 127.0.0.1。
    /// 创建一个新的 `LoopbackDriverWapper` 实例，包装驱动器。
    /// 创建一个新的 `LoopbackInterface` 实例，包含驱动器、接口 ID、接口和名称，并将其封装在一个 `Arc` 中。
    /// ## 参数
    /// - `driver`：一个 `LoopbackDriver` 实例，用于驱动网络环回操作。
    ///
    /// ## 返回值
    /// 返回一个 `Arc<Self>`，即一个指向新创建的 `LoopbackInterface` 实例的智能指针。
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
            inner: SpinLock::new(InnerLoopbackInterface {
                netdevice_common: NetDeviceCommonData::default(),
                device_common: DeviceCommonData::default(),
                kobj_common: KObjectCommonData::default(),
            }),
            locked_kobj_state: LockedKObjectState::default(),
        })
    }

    fn inner(&self) -> SpinLockGuard<InnerLoopbackInterface> {
        return self.inner.lock();
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

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner().kobj_common.kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner().kobj_common.kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner().kobj_common.parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner().kobj_common.parent = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner().kobj_common.kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner().kobj_common.kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
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

impl Device for LoopbackInterface {
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

impl NetDevice for LoopbackInterface {
    /// 由于lo网卡设备不是实际的物理设备，其mac地址需要手动设置为一个默认值，这里默认为00:00:00:00:00
    fn mac(&self) -> smoltcp::wire::EthernetAddress {
        let mac = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        smoltcp::wire::EthernetAddress(mac)
    }

    #[inline]
    fn nic_id(&self) -> usize {
        self.iface_id
    }

    #[inline]
    fn iface_name(&self) -> String {
        self.name.clone()
    }
    /// ## `update_ip_addrs` 用于更新接口的 IP 地址。
    ///
    /// ## 参数
    /// - `&self` ：自身引用
    /// - `ip_addrs` ：一个包含 `smoltcp::wire::IpCidr` 的切片，表示要设置的 IP 地址和子网掩码
    ///
    /// ## 返回值
    /// - 如果 `ip_addrs` 的长度不为 1，返回 `Err(SystemError::EINVAL)`，表示输入参数无效
    /// - 如果更新成功，返回 `Ok(())`
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
    /// ## `poll` 用于轮询接口的状态。
    ///
    /// ## 参数
    /// - `&self` ：自身引用
    /// - `sockets` ：一个可变引用到 `smoltcp::iface::SocketSet`，表示要轮询的套接字集
    ///
    /// ## 返回值
    /// - 如果轮询成功，返回 `Ok(())`
    /// - 如果轮询失败，返回 `Err(SystemError::EAGAIN_OR_EWOULDBLOCK)`，表示需要再次尝试或者操作会阻塞
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
        self.inner().netdevice_common.net_device_type = 24; // 环回设备
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

pub fn loopback_probe() {
    loopback_driver_init();
}
/// ## lo网卡设备初始化函数
/// 创建驱动和iface，初始化一个lo网卡，添加到全局NET_DEVICES中
pub fn loopback_driver_init() {
    let driver = LoopbackDriver::new();
    let iface = LoopbackInterface::new(driver);
    // 标识网络设备已经启动
    iface.set_net_state(NetDeivceState::__LINK_STATE_START);

    NET_DEVICES
        .write_irqsave()
        .insert(iface.iface_id, iface.clone());

    register_netdevice(iface.clone()).expect("register lo device failed");
}

/// ## lo网卡设备的注册函数
#[unified_init(INITCALL_DEVICE)]
pub fn loopback_init() -> Result<(), SystemError> {
    loopback_probe();
    return Ok(());
}
