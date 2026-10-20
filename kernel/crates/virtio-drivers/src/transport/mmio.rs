//! MMIO transport for VirtIO.

use super::{DeviceStatus, DeviceType, Transport};
use crate::{
    align_up,
    queue::Descriptor,
    volatile::{volread, volwrite, ReadOnly, Volatile, WriteOnly},
    Error, PhysAddr, PAGE_SIZE,
};
use core::{
    convert::{TryFrom, TryInto},
    fmt::{self, Display, Formatter},
    mem::{align_of, size_of},
    ptr::NonNull,
};

const MAGIC_VALUE: u32 = 0x7472_6976;
pub(crate) const LEGACY_VERSION: u32 = 1;
pub(crate) const MODERN_VERSION: u32 = 2;
const CONFIG_SPACE_OFFSET: usize = 0x100;

/// The version of the VirtIO MMIO transport supported by a device.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum MmioVersion {
    /// Legacy MMIO transport with page-based addressing.
    Legacy = LEGACY_VERSION,
    /// Modern MMIO transport.
    Modern = MODERN_VERSION,
}

impl TryFrom<u32> for MmioVersion {
    type Error = MmioError;

    fn try_from(version: u32) -> Result<Self, Self::Error> {
        match version {
            LEGACY_VERSION => Ok(Self::Legacy),
            MODERN_VERSION => Ok(Self::Modern),
            _ => Err(MmioError::UnsupportedVersion(version)),
        }
    }
}

impl From<MmioVersion> for u32 {
    fn from(version: MmioVersion) -> Self {
        match version {
            MmioVersion::Legacy => LEGACY_VERSION,
            MmioVersion::Modern => MODERN_VERSION,
        }
    }
}

/// An error encountered initialising a VirtIO MMIO transport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MmioError {
    /// The header doesn't start with the expected magic value 0x74726976.
    BadMagic(u32),
    /// The header reports a version number that is neither 1 (legacy) nor 2 (modern).
    UnsupportedVersion(u32),
    /// The header reports a device ID of 0.
    ZeroDeviceId,
}

impl Display for MmioError {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            Self::BadMagic(magic) => write!(
                f,
                "Invalid magic value {:#010x} (expected 0x74726976).",
                magic
            ),
            Self::UnsupportedVersion(version) => {
                write!(f, "Unsupported Virtio MMIO version {}.", version)
            }
            Self::ZeroDeviceId => write!(f, "Device ID was zero."),
        }
    }
}

/// MMIO Device Register Interface, both legacy and modern.
///
/// Ref: 4.2.2 MMIO Device Register Layout and 4.2.4 Legacy interface
#[repr(C)]
pub struct VirtIOHeader {
    /// Magic value
    magic: ReadOnly<u32>,

    /// Device version number
    ///
    /// Legacy device returns value 0x1.
    version: ReadOnly<u32>,

    /// Virtio Subsystem Device ID
    device_id: ReadOnly<u32>,

    /// Virtio Subsystem Vendor ID
    vendor_id: ReadOnly<u32>,

    /// Flags representing features the device supports
    device_features: ReadOnly<u32>,

    /// Device (host) features word selection
    device_features_sel: WriteOnly<u32>,

    /// Reserved
    __r1: [ReadOnly<u32>; 2],

    /// Flags representing device features understood and activated by the driver
    driver_features: WriteOnly<u32>,

    /// Activated (guest) features word selection
    driver_features_sel: WriteOnly<u32>,

    /// Guest page size
    ///
    /// The driver writes the guest page size in bytes to the register during
    /// initialization, before any queues are used. This value should be a
    /// power of 2 and is used by the device to calculate the Guest address
    /// of the first queue page (see QueuePFN).
    legacy_guest_page_size: WriteOnly<u32>,

    /// Reserved
    __r2: ReadOnly<u32>,

    /// Virtual queue index
    ///
    /// Writing to this register selects the virtual queue that the following
    /// operations on the QueueNumMax, QueueNum, QueueAlign and QueuePFN
    /// registers apply to. The index number of the first queue is zero (0x0).
    queue_sel: WriteOnly<u32>,

    /// Maximum virtual queue size
    ///
    /// Reading from the register returns the maximum size of the queue the
    /// device is ready to process or zero (0x0) if the queue is not available.
    /// This applies to the queue selected by writing to QueueSel and is
    /// allowed only when QueuePFN is set to zero (0x0), so when the queue is
    /// not actively used.
    queue_num_max: ReadOnly<u32>,

    /// Virtual queue size
    ///
    /// Queue size is the number of elements in the queue. Writing to this
    /// register notifies the device what size of the queue the driver will use.
    /// This applies to the queue selected by writing to QueueSel.
    queue_num: WriteOnly<u32>,

    /// Used Ring alignment in the virtual queue
    ///
    /// Writing to this register notifies the device about alignment boundary
    /// of the Used Ring in bytes. This value should be a power of 2 and
    /// applies to the queue selected by writing to QueueSel.
    legacy_queue_align: WriteOnly<u32>,

    /// Guest physical page number of the virtual queue
    ///
    /// Writing to this register notifies the device about location of the
    /// virtual queue in the Guest’s physical address space. This value is
    /// the index number of a page starting with the queue Descriptor Table.
    /// Value zero (0x0) means physical address zero (0x00000000) and is illegal.
    /// When the driver stops using the queue it writes zero (0x0) to this
    /// register. Reading from this register returns the currently used page
    /// number of the queue, therefore a value other than zero (0x0) means that
    /// the queue is in use. Both read and write accesses apply to the queue
    /// selected by writing to QueueSel.
    legacy_queue_pfn: Volatile<u32>,

    /// new interface only
    queue_ready: Volatile<u32>,

    /// Reserved
    __r3: [ReadOnly<u32>; 2],

    /// Queue notifier
    queue_notify: WriteOnly<u32>,

    /// Reserved
    __r4: [ReadOnly<u32>; 3],

    /// Interrupt status
    interrupt_status: ReadOnly<u32>,

    /// Interrupt acknowledge
    interrupt_ack: WriteOnly<u32>,

    /// Reserved
    __r5: [ReadOnly<u32>; 2],

    /// Device status
    ///
    /// Reading from this register returns the current device status flags.
    /// Writing non-zero values to this register sets the status flags,
    /// indicating the OS/driver progress. Writing zero (0x0) to this register
    /// triggers a device reset. The device sets QueuePFN to zero (0x0) for
    /// all queues in the device. Also see 3.1 Device Initialization.
    status: Volatile<DeviceStatus>,

    /// Reserved
    __r6: [ReadOnly<u32>; 3],

    // new interface only since here
    queue_desc_low: WriteOnly<u32>,
    queue_desc_high: WriteOnly<u32>,

    /// Reserved
    __r7: [ReadOnly<u32>; 2],

    queue_driver_low: WriteOnly<u32>,
    queue_driver_high: WriteOnly<u32>,

    /// Reserved
    __r8: [ReadOnly<u32>; 2],

    queue_device_low: WriteOnly<u32>,
    queue_device_high: WriteOnly<u32>,

    /// Reserved
    __r9: [ReadOnly<u32>; 21],

    config_generation: ReadOnly<u32>,
}

impl VirtIOHeader {
    /// Constructs a fake VirtIO header for use in unit tests.
    #[cfg(test)]
    pub fn make_fake_header(
        version: u32,
        device_id: u32,
        vendor_id: u32,
        device_features: u32,
        queue_num_max: u32,
    ) -> Self {
        Self {
            magic: ReadOnly::new(MAGIC_VALUE),
            version: ReadOnly::new(version),
            device_id: ReadOnly::new(device_id),
            vendor_id: ReadOnly::new(vendor_id),
            device_features: ReadOnly::new(device_features),
            device_features_sel: WriteOnly::default(),
            __r1: Default::default(),
            driver_features: Default::default(),
            driver_features_sel: Default::default(),
            legacy_guest_page_size: Default::default(),
            __r2: Default::default(),
            queue_sel: Default::default(),
            queue_num_max: ReadOnly::new(queue_num_max),
            queue_num: Default::default(),
            legacy_queue_align: Default::default(),
            legacy_queue_pfn: Default::default(),
            queue_ready: Default::default(),
            __r3: Default::default(),
            queue_notify: Default::default(),
            __r4: Default::default(),
            interrupt_status: Default::default(),
            interrupt_ack: Default::default(),
            __r5: Default::default(),
            status: Volatile::new(DeviceStatus::empty()),
            __r6: Default::default(),
            queue_desc_low: Default::default(),
            queue_desc_high: Default::default(),
            __r7: Default::default(),
            queue_driver_low: Default::default(),
            queue_driver_high: Default::default(),
            __r8: Default::default(),
            queue_device_low: Default::default(),
            queue_device_high: Default::default(),
            __r9: Default::default(),
            config_generation: Default::default(),
        }
    }
}

/// MMIO Device Register Interface.
///
/// Ref: 4.2.2 MMIO Device Register Layout and 4.2.4 Legacy interface
#[derive(Debug)]
pub struct MmioTransport {
    header: NonNull<VirtIOHeader>,
    version: MmioVersion,
}

impl MmioTransport {
    /// Constructs a new VirtIO MMIO transport, or returns an error if the header reports an
    /// unsupported version.
    ///
    /// # Safety
    /// `header` must point to a properly aligned valid VirtIO MMIO region, which must remain valid
    /// for the lifetime of the transport that is returned.
    pub unsafe fn new(header: NonNull<VirtIOHeader>) -> Result<Self, MmioError> {
        let magic = volread!(header, magic);
        if magic != MAGIC_VALUE {
            return Err(MmioError::BadMagic(magic));
        }
        if volread!(header, device_id) == 0 {
            return Err(MmioError::ZeroDeviceId);
        }
        let version = volread!(header, version).try_into()?;
        Ok(Self { header, version })
    }

    /// Gets the version of the VirtIO MMIO transport.
    pub fn version(&self) -> MmioVersion {
        self.version
    }

    /// Gets the vendor ID.
    pub fn vendor_id(&self) -> u32 {
        // Safe because self.header points to a valid VirtIO MMIO region.
        unsafe { volread!(self.header, vendor_id) }
    }
}

impl Transport for MmioTransport {
    fn device_type(&self) -> DeviceType {
        // Safe because self.header points to a valid VirtIO MMIO region.
        let device_id = unsafe { volread!(self.header, device_id) };
        device_id.into()
    }

    fn read_device_features(&mut self) -> u64 {
        // Safe because self.header points to a valid VirtIO MMIO region.
        unsafe {
            volwrite!(self.header, device_features_sel, 0); // device features [0, 32)
            let mut device_features_bits = volread!(self.header, device_features).into();
            volwrite!(self.header, device_features_sel, 1); // device features [32, 64)
            device_features_bits += (volread!(self.header, device_features) as u64) << 32;
            device_features_bits
        }
    }

    fn write_driver_features(&mut self, driver_features: u64) {
        // Safe because self.header points to a valid VirtIO MMIO region.
        unsafe {
            volwrite!(self.header, driver_features_sel, 0); // driver features [0, 32)
            volwrite!(self.header, driver_features, driver_features as u32);
            volwrite!(self.header, driver_features_sel, 1); // driver features [32, 64)
            volwrite!(self.header, driver_features, (driver_features >> 32) as u32);
        }
    }

    fn max_queue_size(&mut self, queue: u16) -> u32 {
        // Safe because self.header points to a valid VirtIO MMIO region.
        unsafe {
            volwrite!(self.header, queue_sel, queue.into());
            volread!(self.header, queue_num_max)
        }
    }

    fn notify(&mut self, queue: u16) {
        // Safe because self.header points to a valid VirtIO MMIO region.
        unsafe {
            volwrite!(self.header, queue_notify, queue.into());
        }
    }

    fn get_status(&self) -> DeviceStatus {
        // Safe because self.header points to a valid VirtIO MMIO region.
        unsafe { volread!(self.header, status) }
    }

    fn set_status(&mut self, status: DeviceStatus) {
        // Safe because self.header points to a valid VirtIO MMIO region.
        unsafe {
            volwrite!(self.header, status, status);
        }
    }

    fn set_guest_page_size(&mut self, guest_page_size: u32) {
        match self.version {
            MmioVersion::Legacy => {
                // Safe because self.header points to a valid VirtIO MMIO region.
                unsafe {
                    volwrite!(self.header, legacy_guest_page_size, guest_page_size);
                }
            }
            MmioVersion::Modern => {
                // No-op, modern devices don't care.
            }
        }
    }

    fn requires_legacy_layout(&self) -> bool {
        match self.version {
            MmioVersion::Legacy => true,
            MmioVersion::Modern => false,
        }
    }

    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: PhysAddr,
        driver_area: PhysAddr,
        device_area: PhysAddr,
    ) {
        match self.version {
            MmioVersion::Legacy => {
                assert_eq!(
                    driver_area - descriptors,
                    size_of::<Descriptor>() * size as usize
                );
                assert_eq!(
                    device_area - descriptors,
                    align_up(
                        size_of::<Descriptor>() * size as usize
                            + size_of::<u16>() * (size as usize + 3)
                    )
                );
                let align = PAGE_SIZE as u32;
                let pfn = (descriptors / PAGE_SIZE) as u32;
                assert_eq!(pfn as usize * PAGE_SIZE, descriptors);
                // Safe because self.header points to a valid VirtIO MMIO region.
                unsafe {
                    volwrite!(self.header, queue_sel, queue.into());
                    volwrite!(self.header, queue_num, size);
                    volwrite!(self.header, legacy_queue_align, align);
                    volwrite!(self.header, legacy_queue_pfn, pfn);
                }
            }
            MmioVersion::Modern => {
                // Safe because self.header points to a valid VirtIO MMIO region.
                unsafe {
                    volwrite!(self.header, queue_sel, queue.into());
                    volwrite!(self.header, queue_num, size);
                    volwrite!(self.header, queue_desc_low, descriptors as u32);
                    volwrite!(self.header, queue_desc_high, (descriptors >> 32) as u32);
                    volwrite!(self.header, queue_driver_low, driver_area as u32);
                    volwrite!(self.header, queue_driver_high, (driver_area >> 32) as u32);
                    volwrite!(self.header, queue_device_low, device_area as u32);
                    volwrite!(self.header, queue_device_high, (device_area >> 32) as u32);
                    volwrite!(self.header, queue_ready, 1);
                }
            }
        }
    }

    fn queue_unset(&mut self, queue: u16) {
        match self.version {
            MmioVersion::Legacy => {
                // Safe because self.header points to a valid VirtIO MMIO region.
                unsafe {
                    volwrite!(self.header, queue_sel, queue.into());
                    volwrite!(self.header, queue_num, 0);
                    volwrite!(self.header, legacy_queue_align, 0);
                    volwrite!(self.header, legacy_queue_pfn, 0);
                }
            }
            MmioVersion::Modern => {
                // Safe because self.header points to a valid VirtIO MMIO region.
                unsafe {
                    volwrite!(self.header, queue_sel, queue.into());

                    volwrite!(self.header, queue_ready, 0);
                    // Wait until we read the same value back, to ensure synchronisation (see 4.2.2.2).
                    while volread!(self.header, queue_ready) != 0 {}

                    volwrite!(self.header, queue_num, 0);
                    volwrite!(self.header, queue_desc_low, 0);
                    volwrite!(self.header, queue_desc_high, 0);
                    volwrite!(self.header, queue_driver_low, 0);
                    volwrite!(self.header, queue_driver_high, 0);
                    volwrite!(self.header, queue_device_low, 0);
                    volwrite!(self.header, queue_device_high, 0);
                }
            }
        }
    }

    fn queue_used(&mut self, queue: u16) -> bool {
        // Safe because self.header points to a valid VirtIO MMIO region.
        unsafe {
            volwrite!(self.header, queue_sel, queue.into());
            match self.version {
                MmioVersion::Legacy => volread!(self.header, legacy_queue_pfn) != 0,
                MmioVersion::Modern => volread!(self.header, queue_ready) != 0,
            }
        }
    }

    fn ack_interrupt(&mut self) -> bool {
        // Safe because self.header points to a valid VirtIO MMIO region.
        unsafe {
            let interrupt = volread!(self.header, interrupt_status);
            if interrupt != 0 {
                volwrite!(self.header, interrupt_ack, interrupt);
                true
            } else {
                false
            }
        }
    }

    fn config_space<T>(&self) -> Result<NonNull<T>, Error> {
        if align_of::<T>() > 4 {
            // Panic as this should only happen if the driver is written incorrectly.
            panic!(
                "Driver expected config space alignment of {} bytes, but VirtIO only guarantees 4 byte alignment.",
                align_of::<T>()
            );
        }
        Ok(NonNull::new((self.header.as_ptr() as usize + CONFIG_SPACE_OFFSET) as _).unwrap())
    }
}

impl Drop for MmioTransport {
    fn drop(&mut self) {
        // Reset the device when the transport is dropped.
        self.set_status(DeviceStatus::empty())
    }
}
