pub mod acpi;
pub mod base;
pub mod disk;
pub mod keyboard;
pub mod net;
pub mod pci;
pub mod timers;
pub mod tty;
pub mod uart;
pub mod video;
pub mod virtio;

use core::fmt::Debug;
pub trait Driver: Sync + Send + Debug {
    fn as_any_ref(&'static self) -> &'static dyn core::any::Any;
}
