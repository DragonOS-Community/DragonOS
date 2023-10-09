use core::intrinsics::unlikely;

use alloc::sync::Arc;

use crate::{driver::Driver, syscall::SystemError};

use super::{bus::BusNotifyEvent, driver::driver_manager, Device, DeviceManager};

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
    /// https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/dd.c#1049
    pub fn device_attach(&self, dev: &Arc<dyn Device>) -> Result<bool, SystemError> {
        return self.do_device_attach(dev, false);
    }

    /// 参考 https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/dd.c#978
    fn do_device_attach(
        &self,
        dev: &Arc<dyn Device>,
        allow_async: bool,
    ) -> Result<bool, SystemError> {
        if unlikely(allow_async) {
            todo!("do_device_attach: allow_async")
        }
        if dev.is_dead() {
            return Ok(false);
        }

        let mut do_async = false;
        let mut r = Ok(false);

        if dev.driver().is_some() {
            if self.device_is_bound(dev) {
                return Ok(true);
            }

            if self.device_bind_driver(dev).is_ok() {
                return Ok(true);
            } else {
                dev.set_driver(None);
                return Ok(false);
            }
        } else {
            let bus = dev.bus().ok_or(SystemError::EINVAL)?;
            let mut data = DeviceAttachData::new(dev.clone(), allow_async, false);
            let mut flag = true;
            for driver in bus.subsystem().drivers.read().iter() {
                if let Some(driver) = driver.upgrade() {
                    let r = self.do_device_attach_driver(&driver, &mut data);
                    if unlikely(r.is_err()) {
                        flag = false;
                        break;
                    }
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
                kdebug!(
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

    /// 参考 https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/dd.c#899
    fn do_device_attach_driver(
        &self,
        driver: &Arc<dyn Driver>,
        data: &mut DeviceAttachData,
    ) -> Result<(), SystemError> {
        todo!("do_device_attach_driver")
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
        if dev.driver().is_some() {
            return true;
        } else {
            return false;
        }
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
    /// 参考 https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/dd.c#496
    pub fn device_bind_driver(&self, dev: &Arc<dyn Device>) -> Result<(), SystemError> {
        let r = driver_manager().driver_sysfs_add(dev);
        if let Err(e) = r {
            self.device_links_force_bind(dev);
            self.driver_bound(dev);
            return Err(e);
        } else {
            if let Some(bus) = dev.bus() {
                bus.subsystem().bus_notifier().call_chain(
                    BusNotifyEvent::DriverNotBound,
                    Some(dev),
                    None,
                );
            }
        }
        return r;
    }

    /// 参考 https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/dd.c#393
    fn driver_bound(&self, dev: &Arc<dyn Device>) {
        todo!("driver_bound")
    }
}

/// 参考 https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/dd.c#866
#[derive(Debug)]
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

    fn set_have_async(&mut self) {
        self.have_async = true;
    }
}
