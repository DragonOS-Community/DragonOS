use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};
use log::info;
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    driver::base::{
        class::{class_manager, Class},
        device::{device_manager, sys_dev_char_kset},
        kobject::KObject,
        subsys::SubSysPrivate,
    },
    init::initcall::INITCALL_SUBSYS,
    time::{timekeeping::do_settimeofday64, PosixTimeSpec},
};

use super::{interface::rtc_read_time, register_default_rtc, sysfs::RtcGeneralDevice};

/// `/sys/class/rtc` 的 class 实例
static mut CLASS_RTC_INSTANCE: Option<Arc<RtcClass>> = None;

/// 获取 `/sys/class/rtc` 的 class 实例
#[inline(always)]
#[allow(dead_code)]
pub fn sys_class_rtc_instance() -> Option<&'static Arc<RtcClass>> {
    unsafe { CLASS_RTC_INSTANCE.as_ref() }
}

/// 初始化帧缓冲区子系统
#[unified_init(INITCALL_SUBSYS)]
pub fn fbmem_init() -> Result<(), SystemError> {
    let rtc_class = RtcClass::new();
    class_manager().class_register(&(rtc_class.clone() as Arc<dyn Class>))?;

    unsafe {
        CLASS_RTC_INSTANCE = Some(rtc_class);
    }

    return Ok(());
}

/// `/sys/class/rtc` 类
#[derive(Debug)]
pub struct RtcClass {
    subsystem: SubSysPrivate,
}

impl RtcClass {
    const NAME: &'static str = "rtc";
    pub fn new() -> Arc<Self> {
        let r = Self {
            subsystem: SubSysPrivate::new(Self::NAME.to_string(), None, None, &[]),
        };
        let r = Arc::new(r);
        r.subsystem()
            .set_class(Some(Arc::downgrade(&r) as Weak<dyn Class>));

        return r;
    }
}

impl Class for RtcClass {
    fn name(&self) -> &'static str {
        return Self::NAME;
    }

    fn dev_kobj(&self) -> Option<Arc<dyn KObject>> {
        Some(sys_dev_char_kset() as Arc<dyn KObject>)
    }

    fn set_dev_kobj(&self, _kobj: Arc<dyn KObject>) {
        unimplemented!("RtcClass::set_dev_kobj");
    }

    fn subsystem(&self) -> &SubSysPrivate {
        return &self.subsystem;
    }
}

/// 注册rtc通用设备
pub(super) fn rtc_register_device(dev: &Arc<RtcGeneralDevice>) -> Result<(), SystemError> {
    device_manager().add_device(dev.clone())?;
    register_default_rtc(dev.clone());
    // 把硬件时间同步到系统时间
    rtc_hctosys(dev);
    return Ok(());
}

fn rtc_hctosys(dev: &Arc<RtcGeneralDevice>) {
    let r = rtc_read_time(dev);
    if let Err(e) = r {
        dev.set_hc2sys_result(Err(e));
        return;
    }

    let time = r.unwrap();
    let timespec64: PosixTimeSpec = time.into();
    let r = do_settimeofday64(timespec64);
    dev.set_hc2sys_result(r);

    info!(
        "Setting system clock to {} {} UTC ({})",
        time.date_string(),
        time.time_string(),
        timespec64.tv_sec
    );
}
