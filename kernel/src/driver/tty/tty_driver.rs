use core::fmt::Debug;

use alloc::sync::Arc;

use crate::driver::base::device::driver::Driver;

use super::tty_device::TtyDevice;

/// TTY 驱动
///
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/tty_driver.h#434
pub trait TtyDriver: Debug + Send + Sync + Driver {
    fn driver_name(&self) -> &str;
    fn dev_name(&self) -> &str;

    fn metadata(&self) -> &TtyDriverMetadata;

    fn other(&self) -> Option<&Arc<dyn TtyDriver>>;

    fn ttys(&self) -> &[Arc<TtyDevice>];

    fn tty_ops(&self) -> Option<&'static dyn TtyDriverOperations> {
        None
    }
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct TtyDriverMetadata {
    ///  name of the driver used in /proc/tty
    driver_name: &'static str,
    /// used for constructing /dev node name
    dev_name: &'static str,
    /// used as a number base for constructing /dev node name
    name_base: i32,
    /// major /dev device number (zero for autoassignment)
    major: i32,
    /// the first minor /dev device number
    minor_start: i32,
    drv_type: TtyDriverType,
    subtype: TtyDriverSubtype,
}

/// https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/tty_driver.h#411
#[derive(Debug, Clone, Copy)]
pub enum TtyDriverType {}

/// https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/tty_driver.h#412
#[derive(Debug, Clone, Copy)]
pub enum TtyDriverSubtype {}

bitflags! {
    /// https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/tty_driver.h?fi=SERIAL_TYPE_NORMAL#492
    pub struct TtyDriverFlags: u64 {

    }
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/tty_driver.h#350
pub trait TtyDriverOperations {}
