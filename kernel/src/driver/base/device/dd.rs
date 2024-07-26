use core::intrinsics::unlikely;

use alloc::{string::ToString, sync::Arc};
use intertrait::cast::CastArc;
use log::{debug, error, warn};

use crate::{
    driver::base::kobject::KObject,
    filesystem::{
        sysfs::{
            file::sysfs_emit_str, sysfs_instance, Attribute, SysFSOpsSupport, SYSFS_ATTR_MODE_WO,
        },
        vfs::syscall::ModeType,
    },
    libs::wait_queue::WaitQueue,
};
use system_error::SystemError;

use super::{
    bus::BusNotifyEvent,
    device_manager,
    driver::{driver_manager, Driver, DriverManager},
    Device, DeviceManager,
};

static PROBE_WAIT_QUEUE: WaitQueue = WaitQueue::default();

impl DeviceManager {
    /// 尝试把一个设备与一个驱动匹配
    ///
    /// 当前函数会遍历整个bus的驱动列表，并且尝试把设备与每一个驱动进行匹配。
    /// 一旦有一个驱动匹配成功，就会返回。
    ///
    /// ## 参数
    ///
    /// - `dev`: 设备
    ///
    /// ## 返回
    ///
    /// - Ok(true): 匹配成功
    /// - Ok(false): 没有匹配成功
    /// - Err(SystemError::ENODEV): 设备还没被注册
    ///
    /// ## 参考
    ///
    /// https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/dd.c#1049
    pub fn device_attach(&self, dev: &Arc<dyn Device>) -> Result<bool, SystemError> {
        return self.do_device_attach(dev, false);
    }

    pub fn device_initial_probe(&self, dev: &Arc<dyn Device>) -> Result<bool, SystemError> {
        return self.do_device_attach(dev, true);
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/dd.c#978
    fn do_device_attach(
        &self,
        dev: &Arc<dyn Device>,
        allow_async: bool,
    ) -> Result<bool, SystemError> {
        if unlikely(allow_async) {
            // todo!("do_device_attach: allow_async")
            warn!("do_device_attach: allow_async is true, but currently not supported");
        }
        if dev.is_dead() {
            return Ok(false);
        }

        warn!("do_device_attach: dev: '{}'", dev.name());

        let mut do_async = false;
        let mut r = Ok(false);

        if dev.driver().is_some() {
            if self.device_is_bound(dev) {
                debug!(
                    "do_device_attach: device '{}' is already bound.",
                    dev.name()
                );
                return Ok(true);
            }

            if self.device_bind_driver(dev).is_ok() {
                return Ok(true);
            } else {
                dev.set_driver(None);
                return Ok(false);
            }
        } else {
            debug!("do_device_attach: device '{}' is not bound.", dev.name());
            let bus = dev
                .bus()
                .and_then(|bus| bus.upgrade())
                .ok_or(SystemError::EINVAL)?;
            let mut data = DeviceAttachData::new(dev.clone(), allow_async, false);
            let mut flag = false;
            for driver in bus.subsystem().drivers().iter() {
                let r = self.do_device_attach_driver(driver, &mut data);
                if unlikely(r.is_err()) {
                    flag = false;
                    break;
                } else if r.unwrap() {
                    flag = true;
                    break;
                }
            }

            if flag {
                r = Ok(true);
            }

            if !flag && allow_async && data.have_async {
                // If we could not find appropriate driver
                // synchronously and we are allowed to do
                // async probes and there are drivers that
                // want to probe asynchronously, we'll
                // try them.

                do_async = true;
                debug!(
                    "do_device_attach: try scheduling asynchronous probe for device: {}",
                    dev.name()
                );
            }
        }

        if do_async {
            todo!("do_device_attach: do_async")
        }
        return r;
    }

    /// 匹配设备和驱动
    ///
    /// ## 参数
    ///
    /// - `driver`: 驱动
    /// - `data`: 匹配数据
    ///
    /// ## 返回
    ///
    /// - Ok(true): 匹配成功
    /// - Ok(false): 没有匹配成功
    /// - Err(SystemError): 匹配过程中出现意外错误,没有匹配成功
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/dd.c#899
    fn do_device_attach_driver(
        &self,
        driver: &Arc<dyn Driver>,
        data: &mut DeviceAttachData,
    ) -> Result<bool, SystemError> {
        if let Some(bus) = driver.bus().and_then(|bus| bus.upgrade()) {
            let r = bus.match_device(&data.dev, driver);

            if let Err(e) = r {
                // 如果不是ENOSYS，则总线出错
                if e != SystemError::ENOSYS {
                    debug!(
                        "do_device_attach_driver: bus.match_device() failed, dev: '{}', err: {:?}",
                        data.dev.name(),
                        e
                    );
                    return Err(e);
                }
            } else if !r.unwrap() {
                return Ok(false);
            }
        }

        let async_allowed = driver.allows_async_probing();
        if data.check_async && async_allowed != data.want_async {
            return Ok(false);
        }

        return driver_manager()
            .probe_device(driver, &data.dev)
            .map(|_| true);
    }

    /// 检查设备是否绑定到驱动程序
    ///
    /// ## 参数
    ///
    /// - `dev`: 设备
    ///
    /// ## 返回
    ///
    /// 如果传递的设备已成功完成对驱动程序的探测，则返回true，否则返回false。
    pub fn device_is_bound(&self, dev: &Arc<dyn Device>) -> bool {
        return driver_manager().driver_is_bound(dev);
    }

    /// 把一个驱动绑定到设备上
    ///
    /// 允许手动绑定驱动到设备上。调用者需要设置好dev.driver()，保证其不为None
    ///
    /// ## 参数
    ///
    /// - `dev`: 设备
    ///
    /// ## 建议
    ///
    /// 使用device_manager().driver_attach()会更好
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/dd.c#496
    pub fn device_bind_driver(&self, dev: &Arc<dyn Device>) -> Result<(), SystemError> {
        let r = driver_manager().driver_sysfs_add(dev);
        if r.is_ok() {
            self.device_links_force_bind(dev);
            driver_manager().driver_bound(dev);
        } else if let Some(bus) = dev.bus().and_then(|bus| bus.upgrade()) {
            bus.subsystem().bus_notifier().call_chain(
                BusNotifyEvent::DriverNotBound,
                Some(dev),
                None,
            );
        }

        if let Err(e) = r.as_ref() {
            error!(
                "device_bind_driver: driver_sysfs_add failed, dev: '{}', err: {:?}",
                dev.name(),
                e
            );
        }
        return r;
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/dd.c?fi=driver_attach#528
    fn unbind_cleanup(&self, dev: &Arc<dyn Device>) {
        dev.set_driver(None);
        // todo: 添加更多操作，清理数据
    }
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/dd.c#866
#[derive(Debug)]
#[allow(dead_code)]
struct DeviceAttachData {
    dev: Arc<dyn Device>,

    ///  Indicates whether we are considering asynchronous probing or
    ///  not. Only initial binding after device or driver registration
    ///  (including deferral processing) may be done asynchronously, the
    ///  rest is always synchronous, as we expect it is being done by
    ///  request from userspace.
    check_async: bool,

    /// Indicates if we are binding synchronous or asynchronous drivers.
    /// When asynchronous probing is enabled we'll execute 2 passes
    /// over drivers: first pass doing synchronous probing and second
    /// doing asynchronous probing (if synchronous did not succeed -
    /// most likely because there was no driver requiring synchronous
    /// probing - and we found asynchronous driver during first pass).
    /// The 2 passes are done because we can't shoot asynchronous
    /// probe for given device and driver from bus_for_each_drv() since
    /// driver pointer is not guaranteed to stay valid once
    /// bus_for_each_drv() iterates to the next driver on the bus.
    want_async: bool,

    /// We'll set have_async to 'true' if, while scanning for matching
    /// driver, we'll encounter one that requests asynchronous probing.
    have_async: bool,
}

impl DeviceAttachData {
    pub fn new(dev: Arc<dyn Device>, check_async: bool, want_async: bool) -> Self {
        Self {
            dev,
            check_async,
            want_async,
            have_async: false,
        }
    }

    #[allow(dead_code)]
    #[inline(always)]
    fn set_have_async(&mut self) {
        self.have_async = true;
    }
}

impl DriverManager {
    /// 尝试把驱动绑定到现有的设备上
    ///
    /// 这个函数会遍历驱动现有的全部设备，然后尝试把他们匹配。
    /// 一旦有一个设备匹配成功，就会返回，并且设备的driver字段会被设置。
    pub fn driver_attach(&self, driver: &Arc<dyn Driver>) -> Result<(), SystemError> {
        let bus = driver
            .bus()
            .and_then(|bus| bus.upgrade())
            .ok_or(SystemError::EINVAL)?;
        for dev in bus.subsystem().devices().iter() {
            self.do_driver_attach(dev, driver);
        }

        return Ok(());
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/dd.c?fi=driver_attach#1134
    #[inline(never)]
    fn do_driver_attach(&self, device: &Arc<dyn Device>, driver: &Arc<dyn Driver>) -> bool {
        let r = self.match_device(driver, device).unwrap_or(false);
        if !r {
            // 不匹配
            return false;
        }

        if driver.allows_async_probing() {
            unimplemented!(
                "do_driver_attach: probe driver '{}' asynchronously",
                driver.name()
            );
        }

        if self.probe_device(driver, device).is_err() {
            return false;
        }

        return true;
    }

    #[inline(always)]
    pub fn match_device(
        &self,
        driver: &Arc<dyn Driver>,
        device: &Arc<dyn Device>,
    ) -> Result<bool, SystemError> {
        return driver
            .bus()
            .and_then(|bus| bus.upgrade())
            .unwrap()
            .match_device(device, driver);
    }

    /// 尝试把设备和驱动绑定在一起
    ///
    ///
    /// ## 返回
    ///
    /// - Ok(): 绑定成功
    /// - Err(ENODEV): 设备未注册
    /// - Err(EBUSY): 设备已经绑定到驱动上
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/dd.c?fi=driver_attach#802
    fn probe_device(
        &self,
        driver: &Arc<dyn Driver>,
        device: &Arc<dyn Device>,
    ) -> Result<(), SystemError> {
        let r = self.do_probe_device(driver, device);
        PROBE_WAIT_QUEUE.wakeup_all(None);
        return r;
    }

    fn do_probe_device(
        &self,
        driver: &Arc<dyn Driver>,
        device: &Arc<dyn Device>,
    ) -> Result<(), SystemError> {
        if device.is_dead() || (!device.is_registered()) {
            return Err(SystemError::ENODEV);
        }
        if device.driver().is_some() {
            return Err(SystemError::EBUSY);
        }

        device.set_can_match(true);

        self.really_probe(driver, device)?;

        return Ok(());
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/dd.c?fi=driver_attach#584
    fn really_probe(
        &self,
        driver: &Arc<dyn Driver>,
        device: &Arc<dyn Device>,
    ) -> Result<(), SystemError> {
        let bind_failed = || {
            device_manager().unbind_cleanup(device);
        };

        let sysfs_failed = || {
            if let Some(bus) = device.bus().and_then(|bus| bus.upgrade()) {
                bus.subsystem().bus_notifier().call_chain(
                    BusNotifyEvent::DriverNotBound,
                    Some(device),
                    None,
                );
            }
        };

        let probe_failed = || {
            self.remove_from_sysfs(device);
        };

        let dev_groups_failed = || {
            device_manager().remove(device);
        };

        device.set_driver(Some(Arc::downgrade(driver)));

        self.add_to_sysfs(device).map_err(|e| {
            error!(
                "really_probe: add_to_sysfs failed, dev: '{}', err: {:?}",
                device.name(),
                e
            );
            sysfs_failed();
            bind_failed();
            e
        })?;

        self.call_driver_probe(device, driver).map_err(|e| {
            error!(
                "really_probe: call_driver_probe failed, dev: '{}', err: {:?}",
                device.name(),
                e
            );

            probe_failed();
            sysfs_failed();
            bind_failed();
            e
        })?;

        device_manager()
            .add_groups(device, driver.dev_groups())
            .map_err(|e| {
                error!(
                    "really_probe: add_groups failed, dev: '{}', err: {:?}",
                    device.name(),
                    e
                );
                dev_groups_failed();
                probe_failed();
                sysfs_failed();
                bind_failed();
                e
            })?;

        // 我们假设所有的设备都有 sync_state 这个属性。如果没有的话，也创建属性文件。
        device_manager()
            .create_file(device, &DeviceAttrStateSynced)
            .map_err(|e| {
                error!(
                    "really_probe: create_file failed, dev: '{}', err: {:?}",
                    device.name(),
                    e
                );
                dev_groups_failed();
                probe_failed();
                sysfs_failed();
                bind_failed();
                e
            })?;

        self.driver_bound(device);

        return Ok(());
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/dd.c?fi=driver_attach#434
    fn add_to_sysfs(&self, device: &Arc<dyn Device>) -> Result<(), SystemError> {
        let driver = device.driver().ok_or(SystemError::EINVAL)?;

        if let Some(bus) = device.bus().and_then(|bus| bus.upgrade()) {
            bus.subsystem().bus_notifier().call_chain(
                BusNotifyEvent::BindDriver,
                Some(device),
                None,
            );
        }

        let driver_kobj = driver.clone() as Arc<dyn KObject>;
        let device_kobj = device.clone() as Arc<dyn KObject>;

        sysfs_instance().create_link(Some(&driver_kobj), &device_kobj, device.name())?;

        let fail_rm_dev_link = || {
            sysfs_instance().remove_link(&driver_kobj, device.name());
        };

        sysfs_instance()
            .create_link(Some(&device_kobj), &driver_kobj, "driver".to_string())
            .inspect_err(|_e| {
                fail_rm_dev_link();
            })?;

        device_manager()
            .create_file(device, &DeviceAttrCoredump)
            .inspect_err(|_e| {
                sysfs_instance().remove_link(&device_kobj, "driver".to_string());
                fail_rm_dev_link();
            })?;

        return Ok(());
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/dd.c?fi=driver_attach#469
    fn remove_from_sysfs(&self, _device: &Arc<dyn Device>) {
        todo!("remove_from_sysfs")
    }

    fn call_driver_probe(
        &self,
        device: &Arc<dyn Device>,
        driver: &Arc<dyn Driver>,
    ) -> Result<(), SystemError> {
        let bus = device
            .bus()
            .and_then(|bus| bus.upgrade())
            .ok_or(SystemError::EINVAL)?;
        let r = bus.probe(device);
        if r == Err(SystemError::ENOSYS) {
            error!(
                "call_driver_probe: bus.probe() failed, dev: '{}', err: {:?}",
                device.name(),
                r
            );
            return r;
        }

        if r.is_ok() {
            return Ok(());
        }

        let err = r.unwrap_err();
        match err {
            SystemError::ENODEV | SystemError::ENXIO => {
                debug!(
                    "driver'{}': probe of {} rejects match {:?}",
                    driver.name(),
                    device.name(),
                    err
                );
            }

            _ => {
                warn!(
                    "driver'{}': probe of {} failed with error {:?}",
                    driver.name(),
                    device.name(),
                    err
                );
            }
        }

        return Err(err);
    }

    /// 当设备被成功探测，进行了'设备->驱动'绑定后，调用这个函数，完成'驱动->设备'的绑定
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/dd.c#393
    fn driver_bound(&self, device: &Arc<dyn Device>) {
        if self.driver_is_bound(device) {
            warn!("driver_bound: device '{}' is already bound.", device.name());
            return;
        }

        let driver = device.driver().unwrap();
        driver.add_device(device.clone());

        if let Some(bus) = device.bus().and_then(|bus| bus.upgrade()) {
            bus.subsystem().bus_notifier().call_chain(
                BusNotifyEvent::BoundDriver,
                Some(device),
                None,
            );
        }

        // todo: 发送kobj bind的uevent
    }

    fn driver_is_bound(&self, device: &Arc<dyn Device>) -> bool {
        if let Some(driver) = device.driver() {
            if driver.find_device_by_name(&device.name()).is_some() {
                return true;
            }
        }

        return false;
    }
}

/// 设备文件夹下的`dev`文件的属性
#[derive(Debug, Clone, Copy)]
pub struct DeviceAttrStateSynced;

impl Attribute for DeviceAttrStateSynced {
    fn mode(&self) -> ModeType {
        // 0o444
        return ModeType::S_IRUGO;
    }

    fn name(&self) -> &str {
        "state_synced"
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let dev = kobj.cast::<dyn Device>().map_err(|kobj| {
            error!(
                "Intertrait casting not implemented for kobj: {}",
                kobj.name()
            );
            SystemError::ENOSYS
        })?;

        let val = dev.state_synced();
        let val = if val { 1 } else { 0 };
        return sysfs_emit_str(buf, format!("{}\n", val).as_str());
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }
}

#[derive(Debug)]
pub(super) struct DeviceAttrCoredump;

impl Attribute for DeviceAttrCoredump {
    fn name(&self) -> &str {
        "coredump"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_WO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_STORE
    }

    fn store(&self, kobj: Arc<dyn KObject>, buf: &[u8]) -> Result<usize, SystemError> {
        let dev = kobj.cast::<dyn Device>().map_err(|kobj| {
            error!(
                "Intertrait casting not implemented for kobj: {}",
                kobj.name()
            );
            SystemError::ENOSYS
        })?;

        let drv = dev.driver().ok_or(SystemError::EINVAL)?;
        drv.coredump(&dev)?;

        return Ok(buf.len());
    }
}
