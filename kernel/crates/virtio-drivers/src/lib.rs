//! VirtIO guest drivers.
//!
//! These drivers can be used by bare-metal code (such as a bootloader or OS kernel) running in a VM
//! to interact with VirtIO devices provided by the VMM (such as QEMU or crosvm).
//!
//! # Usage
//!
//! You must first implement the [`Hal`] trait, to allocate DMA regions and translate between
//! physical addresses (as seen by devices) and virtual addresses (as seen by your program). You can
//! then construct the appropriate transport for the VirtIO device, e.g. for an MMIO device (perhaps
//! discovered from the device tree):
//!
//! ```
//! use core::ptr::NonNull;
//! use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};
//!
//! # fn example(mmio_device_address: usize) {
//! let header = NonNull::new(mmio_device_address as *mut VirtIOHeader).unwrap();
//! let transport = unsafe { MmioTransport::new(header) }.unwrap();
//! # }
//! ```
//!
//! You can then check what kind of VirtIO device it is and construct the appropriate driver:
//!
//! ```
//! # use virtio_drivers::Hal;
//! # #[cfg(feature = "alloc")]
//! use virtio_drivers::{
//!     device::console::VirtIOConsole,
//!     transport::{mmio::MmioTransport, DeviceType, Transport},
//! };

//!
//! # #[cfg(feature = "alloc")]
//! # fn example<HalImpl: Hal>(transport: MmioTransport) {
//! if transport.device_type() == DeviceType::Console {
//!     let mut console = VirtIOConsole::<HalImpl, _>::new(transport).unwrap();
//!     // Send a byte to the console.
//!     console.send(b'H').unwrap();
//! }
//! # }
//! ```

#![cfg_attr(not(test), no_std)]
#![deny(unused_must_use, missing_docs)]
#![allow(clippy::identity_op)]
#![allow(dead_code)]

#[cfg(any(feature = "alloc", test))]
extern crate alloc;

pub mod device;
mod hal;
/// Virtqueue implementation used by virtio device drivers.
pub mod queue;
pub mod transport;
mod volatile;

use core::{
    fmt::{self, Display, Formatter},
    ptr::{self, NonNull},
};

pub use self::hal::{BufferDirection, Hal, PhysAddr};

/// The page size in bytes supported by the library (4 KiB).
pub const PAGE_SIZE: usize = 0x1000;

/// The type returned by driver methods.
pub type Result<T = ()> = core::result::Result<T, Error>;

/// The error type of VirtIO drivers.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Error {
    /// There are not enough descriptors available in the virtqueue, try again later.
    QueueFull,
    /// The device is not ready.
    NotReady,
    /// The device used a different descriptor chain to the one we were expecting.
    WrongToken,
    /// The queue is already in use.
    AlreadyUsed,
    /// Invalid parameter.
    InvalidParam,
    /// Failed to alloc DMA memory.
    DmaError,
    /// I/O Error
    IoError,
    /// The request was not supported by the device.
    Unsupported,
    /// The config space advertised by the device is smaller than the driver expected.
    ConfigSpaceTooSmall,
    /// The device doesn't have any config space, but the driver expects some.
    ConfigSpaceMissing,
    /// Error from the socket device.
    SocketDeviceError(device::socket::SocketError),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            Self::QueueFull => write!(f, "Virtqueue is full"),
            Self::NotReady => write!(f, "Device not ready"),
            Self::WrongToken => write!(
                f,
                "Device used a different descriptor chain to the one we were expecting"
            ),
            Self::AlreadyUsed => write!(f, "Virtqueue is already in use"),
            Self::InvalidParam => write!(f, "Invalid parameter"),
            Self::DmaError => write!(f, "Failed to allocate DMA memory"),
            Self::IoError => write!(f, "I/O Error"),
            Self::Unsupported => write!(f, "Request not supported by device"),
            Self::ConfigSpaceTooSmall => write!(
                f,
                "Config space advertised by the device is smaller than expected"
            ),
            Self::ConfigSpaceMissing => {
                write!(
                    f,
                    "The device doesn't have any config space, but the driver expects some"
                )
            }
            Self::SocketDeviceError(e) => write!(f, "Error from the socket device: {e:?}"),
        }
    }
}

impl From<device::socket::SocketError> for Error {
    fn from(e: device::socket::SocketError) -> Self {
        Self::SocketDeviceError(e)
    }
}

/// Align `size` up to a page.
fn align_up(size: usize) -> usize {
    (size + PAGE_SIZE) & !(PAGE_SIZE - 1)
}

/// The number of pages required to store `size` bytes, rounded up to a whole number of pages.
fn pages(size: usize) -> usize {
    (size + PAGE_SIZE - 1) / PAGE_SIZE
}

// TODO: Use NonNull::slice_from_raw_parts once it is stable.
/// Creates a non-null raw slice from a non-null thin pointer and length.
fn nonnull_slice_from_raw_parts<T>(data: NonNull<T>, len: usize) -> NonNull<[T]> {
    NonNull::new(ptr::slice_from_raw_parts_mut(data.as_ptr(), len)).unwrap()
}
