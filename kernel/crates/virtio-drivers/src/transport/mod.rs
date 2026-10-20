//! VirtIO transports.

#[cfg(test)]
pub mod fake;
pub mod mmio;
pub mod pci;

use crate::{PhysAddr, Result, PAGE_SIZE};
use bitflags::{bitflags, Flags};
use core::{fmt::Debug, ops::BitAnd, ptr::NonNull};
use log::debug;

/// A VirtIO transport layer.
pub trait Transport {
    /// Gets the device type.
    fn device_type(&self) -> DeviceType;

    /// Reads device features.
    fn read_device_features(&mut self) -> u64;

    /// Writes device features.
    fn write_driver_features(&mut self, driver_features: u64);

    /// Gets the max size of the given queue.
    fn max_queue_size(&mut self, queue: u16) -> u32;

    /// Notifies the given queue on the device.
    fn notify(&mut self, queue: u16);

    /// Gets the device status.
    fn get_status(&self) -> DeviceStatus;

    /// Sets the device status.
    fn set_status(&mut self, status: DeviceStatus);

    /// Sets the guest page size.
    fn set_guest_page_size(&mut self, guest_page_size: u32);

    /// Returns whether the transport requires queues to use the legacy layout.
    ///
    /// Ref: 2.6.2 Legacy Interfaces: A Note on Virtqueue Layout
    fn requires_legacy_layout(&self) -> bool;

    /// Sets up the given queue.
    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: PhysAddr,
        driver_area: PhysAddr,
        device_area: PhysAddr,
    );

    /// Disables and resets the given queue.
    fn queue_unset(&mut self, queue: u16);

    /// Returns whether the queue is in use, i.e. has a nonzero PFN or is marked as ready.
    fn queue_used(&mut self, queue: u16) -> bool;

    /// Acknowledges an interrupt.
    ///
    /// Returns true on success.
    fn ack_interrupt(&mut self) -> bool;

    /// Begins initializing the device.
    ///
    /// Ref: virtio 3.1.1 Device Initialization
    ///
    /// Returns the negotiated set of features.
    fn begin_init<F: Flags<Bits = u64> + BitAnd<Output = F> + Debug>(
        &mut self,
        supported_features: F,
    ) -> F {
        self.set_status(DeviceStatus::empty());
        self.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER);

        let device_features = F::from_bits_truncate(self.read_device_features());
        debug!("Device features: {:?}", device_features);
        let negotiated_features = device_features & supported_features;
        self.write_driver_features(negotiated_features.bits());

        self.set_status(
            DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER | DeviceStatus::FEATURES_OK,
        );

        self.set_guest_page_size(PAGE_SIZE as u32);

        negotiated_features
    }

    /// Finishes initializing the device.
    fn finish_init(&mut self) {
        self.set_status(
            DeviceStatus::ACKNOWLEDGE
                | DeviceStatus::DRIVER
                | DeviceStatus::FEATURES_OK
                | DeviceStatus::DRIVER_OK,
        );
    }

    /// Gets the pointer to the config space.
    fn config_space<T: 'static>(&self) -> Result<NonNull<T>>;
}

bitflags! {
    /// The device status field. Writing 0 into this field resets the device.
    #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
    pub struct DeviceStatus: u32 {
        /// Indicates that the guest OS has found the device and recognized it
        /// as a valid virtio device.
        const ACKNOWLEDGE = 1;

        /// Indicates that the guest OS knows how to drive the device.
        const DRIVER = 2;

        /// Indicates that something went wrong in the guest, and it has given
        /// up on the device. This could be an internal error, or the driver
        /// didn’t like the device for some reason, or even a fatal error
        /// during device operation.
        const FAILED = 128;

        /// Indicates that the driver has acknowledged all the features it
        /// understands, and feature negotiation is complete.
        const FEATURES_OK = 8;

        /// Indicates that the driver is set up and ready to drive the device.
        const DRIVER_OK = 4;

        /// Indicates that the device has experienced an error from which it
        /// can’t recover.
        const DEVICE_NEEDS_RESET = 64;
    }
}

/// Types of virtio devices.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(missing_docs)]
pub enum DeviceType {
    Invalid = 0,
    Network = 1,
    Block = 2,
    Console = 3,
    EntropySource = 4,
    MemoryBallooning = 5,
    IoMemory = 6,
    Rpmsg = 7,
    ScsiHost = 8,
    _9P = 9,
    Mac80211 = 10,
    RprocSerial = 11,
    VirtioCAIF = 12,
    MemoryBalloon = 13,
    GPU = 16,
    Timer = 17,
    Input = 18,
    Socket = 19,
    Crypto = 20,
    SignalDistributionModule = 21,
    Pstore = 22,
    IOMMU = 23,
    Memory = 24,
    FileSystem = 26,
    Pmem = 27,
}

impl From<u32> for DeviceType {
    fn from(virtio_device_id: u32) -> Self {
        match virtio_device_id {
            1 => DeviceType::Network,
            2 => DeviceType::Block,
            3 => DeviceType::Console,
            4 => DeviceType::EntropySource,
            5 => DeviceType::MemoryBalloon,
            6 => DeviceType::IoMemory,
            7 => DeviceType::Rpmsg,
            8 => DeviceType::ScsiHost,
            9 => DeviceType::_9P,
            10 => DeviceType::Mac80211,
            11 => DeviceType::RprocSerial,
            12 => DeviceType::VirtioCAIF,
            13 => DeviceType::MemoryBalloon,
            16 => DeviceType::GPU,
            17 => DeviceType::Timer,
            18 => DeviceType::Input,
            19 => DeviceType::Socket,
            20 => DeviceType::Crypto,
            21 => DeviceType::SignalDistributionModule,
            22 => DeviceType::Pstore,
            23 => DeviceType::IOMMU,
            24 => DeviceType::Memory,
            26 => DeviceType::FileSystem,
            27 => DeviceType::Pmem,
            _ => DeviceType::Invalid,
        }
    }
}

impl From<u16> for DeviceType {
    fn from(virtio_device_id: u16) -> Self {
        u32::from(virtio_device_id).into()
    }
}

impl From<u8> for DeviceType {
    fn from(virtio_device_id: u8) -> Self {
        u32::from(virtio_device_id).into()
    }
}
