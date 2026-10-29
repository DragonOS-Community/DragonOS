use crate::driver::base::device::bus::Bus;
use crate::driver::base::device::{device_manager, device_register, sys_devices_kset, Device};
use crate::driver::base::kobject::KObject;
use crate::init::initcall::INITCALL_DEVICE;
use crate::misc::events::device::{ProbePmuDevice, PROBE_TYPE_ATTR};
use crate::misc::events::get_event_source_bus;
use crate::perf::PERF_TYPE_UPROBE;
use alloc::sync::Arc;
use system_error::SystemError;
use unified_init::macros::unified_init;

static mut UPROBE_DEVICE: Option<Arc<ProbePmuDevice>> = None;

#[unified_init(INITCALL_DEVICE)]
pub fn uprobe_subsys_init() -> Result<(), SystemError> {
    let uprobe_device = ProbePmuDevice::new(
        "uprobe",
        PERF_TYPE_UPROBE,
        Some(Arc::downgrade(&(sys_devices_kset() as Arc<dyn KObject>))),
    );

    let event_source_bus = get_event_source_bus().ok_or(SystemError::EINVAL)?;
    uprobe_device.set_bus(Some(Arc::downgrade(&(event_source_bus as Arc<dyn Bus>))));

    device_register(uprobe_device.clone())?;
    unsafe {
        UPROBE_DEVICE = Some(uprobe_device.clone());
    }

    device_manager().create_file(&(uprobe_device as Arc<dyn Device>), &PROBE_TYPE_ATTR)?;
    Ok(())
}
