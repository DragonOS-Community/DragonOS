use crate::driver::base::device::bus::{bus_register, Bus};
use crate::init::initcall::INITCALL_SUBSYS;
use crate::misc::events::subsys::EventSourceBus;
use alloc::sync::Arc;
use system_error::SystemError;
use unified_init::macros::unified_init;

mod kprobe;
mod subsys;

static mut EVENT_SOURCE_BUS: Option<Arc<EventSourceBus>> = None;

fn get_event_source_bus() -> Option<Arc<EventSourceBus>> {
    unsafe { EVENT_SOURCE_BUS.clone() }
}

#[unified_init(INITCALL_SUBSYS)]
pub fn init_event_source_bus() -> Result<(), SystemError> {
    let event_source_bus = EventSourceBus::new();
    let r = bus_register(event_source_bus.clone() as Arc<dyn Bus>);
    if r.is_err() {
        unsafe { EVENT_SOURCE_BUS = None };
        return r;
    }
    unsafe { EVENT_SOURCE_BUS = Some(event_source_bus.clone()) };
    // kprobe::kprobe_subsys_init()?;
    Ok(())
}
