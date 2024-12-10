use crate::driver::{base::device::Device, input::serio::serio_device::SerioDevice};

// todo: https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/libps2.h#33
#[allow(unused)]
pub trait Ps2Device: Device + SerioDevice {}
