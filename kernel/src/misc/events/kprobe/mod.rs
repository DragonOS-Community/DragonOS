use crate::driver::base::device::bus::Bus;
use crate::driver::base::device::{device_manager, device_register, sys_devices_kset, Device};
use crate::driver::base::kobject::KObject;
use crate::init::initcall::INITCALL_DEVICE;
use crate::misc::events::device::{ProbePmuDevice, PROBE_TYPE_ATTR};
use crate::misc::events::get_event_source_bus;
use crate::perf::PERF_TYPE_KPROBE;
use alloc::sync::Arc;
use system_error::SystemError;
use unified_init::macros::unified_init;

static mut KPROBE_DEVICE: Option<Arc<ProbePmuDevice>> = None;

#[unified_init(INITCALL_DEVICE)]
pub fn kprobe_subsys_init() -> Result<(), SystemError> {
    let kprobe_device = ProbePmuDevice::new(
        "kprobe",
        PERF_TYPE_KPROBE,
        Some(Arc::downgrade(&(sys_devices_kset() as Arc<dyn KObject>))),
    );

    let event_source_bus = get_event_source_bus().ok_or(SystemError::EINVAL)?;
    kprobe_device.set_bus(Some(Arc::downgrade(&(event_source_bus as Arc<dyn Bus>))));

    // 注册到/sys/devices下
    device_register(kprobe_device.clone())?;
    unsafe {
        KPROBE_DEVICE = Some(kprobe_device.clone());
    }

    device_manager().create_file(&(kprobe_device as Arc<dyn Device>), &PROBE_TYPE_ATTR)?;
    Ok(())
}
