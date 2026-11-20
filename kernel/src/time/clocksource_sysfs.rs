//! `/sys/devices/system/clocksource/clocksource0` 的只读导出。
//!
//! Linux 通过该目录报告内核已注册的 clocksource 以及当前选中的
//! clocksource。OCM-Agent 会用它判断当前是否运行在容器中，因此即使
//! 内核已经正常使用 kvm-clock，也需要把这个 ABI 暴露给用户态。

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    driver::base::{
        device::bus::sys_devices_system_kset,
        kobject::{CommonKobj, DynamicKObjKType, KObject, KObjectManager},
    },
    filesystem::{
        sysfs::{file::sysfs_emit_str, sysfs_instance, Attribute, SysFSOpsSupport},
        vfs::InodeMode,
    },
    init::initcall::INITCALL_POSTCORE,
    libs::lazy_init::Lazy,
    time::{
        clocksource::{Clocksource, CLOCKSOURCE_LIST},
        timekeeping::{timekeeper, timekeeping_is_initialized},
    },
};

/// 保持 clocksource/clocksource0 两个 kobject 存活。
///
/// sysfs 目录只保存 kobject 的弱引用，因此必须由内核侧持有强引用。
static CLOCKSOURCE_SYSFS: Lazy<Option<(Arc<CommonKobj>, Arc<CommonKobj>)>> = Lazy::new();

#[derive(Debug)]
struct AttrAvailableClocksource;

impl Attribute for AttrAvailableClocksource {
    fn name(&self) -> &str {
        "available_clocksource"
    }

    fn mode(&self) -> InodeMode {
        InodeMode::S_IRUGO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        // 在锁外预留容量，锁内只克隆 Arc；读取 clocksource_data()
        // 放到释放 list 锁之后，缩短关中断的临界区。
        let mut sources: Vec<Arc<dyn Clocksource>> = Vec::with_capacity(16);
        {
            let list = CLOCKSOURCE_LIST.lock_irqsave();
            for cs in list.iter() {
                sources.push(cs.clone());
            }
        }

        // Linux 使用空格分隔每个 clocksource，并在末尾保留一个空格。
        let mut out = String::new();
        for cs in sources {
            out.push_str(&cs.clocksource_data().name);
            out.push(' ');
        }
        out.push('\n');
        sysfs_emit_str(buf, &out)
    }
}

#[derive(Debug)]
struct AttrCurrentClocksource;

impl Attribute for AttrCurrentClocksource {
    fn name(&self) -> &str {
        "current_clocksource"
    }

    fn mode(&self) -> InodeMode {
        InodeMode::S_IRUGO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let name = if timekeeping_is_initialized() {
            timekeeper()
                .current_clocksource()
                .map(|cs| cs.clocksource_data().name.clone())
        } else {
            None
        };

        let out = match name {
            Some(name) => format!("{name}\n"),
            None => "unknown\n".to_string(),
        };
        sysfs_emit_str(buf, &out)
    }
}

#[unified_init(INITCALL_POSTCORE)]
fn clocksource_sysfs_init() -> Result<(), SystemError> {
    if CLOCKSOURCE_SYSFS.initialized() {
        return Ok(());
    }

    let system = sys_devices_system_kset().as_kobject();
    let clocksource = CommonKobj::new("clocksource".to_string());
    clocksource.set_parent(Some(Arc::downgrade(&system)));
    KObjectManager::init_and_add_kobj(clocksource.clone(), Some(&DynamicKObjKType))?;

    let clocksource0 = CommonKobj::new("clocksource0".to_string());
    let clocksource_dyn: Arc<dyn KObject> = clocksource.clone();
    clocksource0.set_parent(Some(Arc::downgrade(&clocksource_dyn)));
    KObjectManager::init_and_add_kobj(clocksource0.clone(), Some(&DynamicKObjKType))?;

    let clocksource0_dyn: Arc<dyn KObject> = clocksource0.clone();
    sysfs_instance().create_file(&clocksource0_dyn, &AttrAvailableClocksource)?;
    sysfs_instance().create_file(&clocksource0_dyn, &AttrCurrentClocksource)?;

    CLOCKSOURCE_SYSFS.init(Some((clocksource, clocksource0)));
    Ok(())
}
