use crate::driver::base::device::bus::Bus;
use crate::driver::base::device::Device;
use crate::driver::base::subsys::SubSysPrivate;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

#[derive(Debug)]
pub struct EventSourceBus {
    private: SubSysPrivate,
}

impl EventSourceBus {
    pub fn new() -> Arc<Self> {
        let w: Weak<Self> = Weak::new();
        let private = SubSysPrivate::new("event_source".to_string(), Some(w), None, &[]);
        let bus = Arc::new(Self { private });
        bus.subsystem()
            .set_bus(Some(Arc::downgrade(&(bus.clone() as Arc<dyn Bus>))));
        return bus;
    }
}

impl Bus for EventSourceBus {
    fn name(&self) -> String {
        "event_source".to_string()
    }

    fn dev_name(&self) -> String {
        self.name()
    }

    fn root_device(&self) -> Option<Weak<dyn Device>> {
        None
    }

    fn remove(&self, _device: &Arc<dyn Device>) -> Result<(), SystemError> {
        todo!()
    }

    fn shutdown(&self, _device: &Arc<dyn Device>) {
        todo!()
    }

    fn resume(&self, _device: &Arc<dyn Device>) -> Result<(), SystemError> {
        todo!()
    }

    fn subsystem(&self) -> &SubSysPrivate {
        &self.private
    }
}
