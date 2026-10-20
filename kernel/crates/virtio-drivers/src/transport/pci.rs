//! PCI transport for VirtIO.

pub mod bus;

use self::bus::{DeviceFunction, DeviceFunctionInfo, PciError, PciRoot, PCI_CAP_ID_VNDR};
use super::{DeviceStatus, DeviceType, Transport};
use crate::{
    hal::{Hal, PhysAddr},
    nonnull_slice_from_raw_parts,
    volatile::{
        volread, volwrite, ReadOnly, Volatile, VolatileReadable, VolatileWritable, WriteOnly,
    },
    Error,
};
use core::{
    fmt::{self, Display, Formatter},
    mem::{align_of, size_of},
    ptr::{addr_of_mut, NonNull},
};

/// The PCI vendor ID for VirtIO devices.
const VIRTIO_VENDOR_ID: u16 = 0x1af4;

/// The offset to add to a VirtIO device ID to get the corresponding PCI device ID.
const PCI_DEVICE_ID_OFFSET: u16 = 0x1040;

const TRANSITIONAL_NETWORK: u16 = 0x1000;
const TRANSITIONAL_BLOCK: u16 = 0x1001;
const TRANSITIONAL_MEMORY_BALLOONING: u16 = 0x1002;
const TRANSITIONAL_CONSOLE: u16 = 0x1003;
const TRANSITIONAL_SCSI_HOST: u16 = 0x1004;
const TRANSITIONAL_ENTROPY_SOURCE: u16 = 0x1005;
const TRANSITIONAL_9P_TRANSPORT: u16 = 0x1009;

/// The offset of the bar field within `virtio_pci_cap`.
const CAP_BAR_OFFSET: u8 = 4;
/// The offset of the offset field with `virtio_pci_cap`.
const CAP_BAR_OFFSET_OFFSET: u8 = 8;
/// The offset of the `length` field within `virtio_pci_cap`.
const CAP_LENGTH_OFFSET: u8 = 12;
/// The offset of the`notify_off_multiplier` field within `virtio_pci_notify_cap`.
const CAP_NOTIFY_OFF_MULTIPLIER_OFFSET: u8 = 16;

/// Common configuration.
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
/// Notifications.
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
/// ISR Status.
const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
/// Device specific configuration.
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

fn device_type(pci_device_id: u16) -> DeviceType {
    match pci_device_id {
        TRANSITIONAL_NETWORK => DeviceType::Network,
        TRANSITIONAL_BLOCK => DeviceType::Block,
        TRANSITIONAL_MEMORY_BALLOONING => DeviceType::MemoryBalloon,
        TRANSITIONAL_CONSOLE => DeviceType::Console,
        TRANSITIONAL_SCSI_HOST => DeviceType::ScsiHost,
        TRANSITIONAL_ENTROPY_SOURCE => DeviceType::EntropySource,
        TRANSITIONAL_9P_TRANSPORT => DeviceType::_9P,
        id if id >= PCI_DEVICE_ID_OFFSET => DeviceType::from(id - PCI_DEVICE_ID_OFFSET),
        _ => DeviceType::Invalid,
    }
}

/// Returns the type of VirtIO device to which the given PCI vendor and device ID corresponds, or
/// `None` if it is not a recognised VirtIO device.
pub fn virtio_device_type(device_function_info: &DeviceFunctionInfo) -> Option<DeviceType> {
    if device_function_info.vendor_id == VIRTIO_VENDOR_ID {
        let device_type = device_type(device_function_info.device_id);
        if device_type != DeviceType::Invalid {
            return Some(device_type);
        }
    }
    None
}

/// PCI transport for VirtIO.
///
/// Ref: 4.1 Virtio Over PCI Bus
#[derive(Debug)]
pub struct PciTransport {
    device_type: DeviceType,
    /// The bus, device and function identifier for the VirtIO device.
    device_function: DeviceFunction,
    /// The common configuration structure within some BAR.
    common_cfg: NonNull<CommonCfg>,
    /// The start of the queue notification region within some BAR.
    notify_region: NonNull<[WriteOnly<u16>]>,
    notify_off_multiplier: u32,
    /// The ISR status register within some BAR.
    isr_status: NonNull<Volatile<u8>>,
    /// The VirtIO device-specific configuration within some BAR.
    config_space: Option<NonNull<[u32]>>,
}

impl PciTransport {
    /// Construct a new PCI VirtIO device driver for the given device function on the given PCI
    /// root controller.
    ///
    /// The PCI device must already have had its BARs allocated.
    pub fn new<H: Hal>(
        root: &mut PciRoot,
        device_function: DeviceFunction,
    ) -> Result<Self, VirtioPciError> {
        let device_vendor = root.config_read_word(device_function, 0);
        let device_id = (device_vendor >> 16) as u16;
        let vendor_id = device_vendor as u16;
        if vendor_id != VIRTIO_VENDOR_ID {
            return Err(VirtioPciError::InvalidVendorId(vendor_id));
        }
        let device_type = device_type(device_id);

        // Find the PCI capabilities we need.
        let mut common_cfg = None;
        let mut notify_cfg = None;
        let mut notify_off_multiplier = 0;
        let mut isr_cfg = None;
        let mut device_cfg = None;
        for capability in root.capabilities(device_function) {
            if capability.id != PCI_CAP_ID_VNDR {
                continue;
            }
            let cap_len = capability.private_header as u8;
            let cfg_type = (capability.private_header >> 8) as u8;
            if cap_len < 16 {
                continue;
            }
            let struct_info = VirtioCapabilityInfo {
                bar: root.config_read_word(device_function, capability.offset + CAP_BAR_OFFSET)
                    as u8,
                offset: root
                    .config_read_word(device_function, capability.offset + CAP_BAR_OFFSET_OFFSET),
                length: root
                    .config_read_word(device_function, capability.offset + CAP_LENGTH_OFFSET),
            };

            match cfg_type {
                VIRTIO_PCI_CAP_COMMON_CFG if common_cfg.is_none() => {
                    common_cfg = Some(struct_info);
                }
                VIRTIO_PCI_CAP_NOTIFY_CFG if cap_len >= 20 && notify_cfg.is_none() => {
                    notify_cfg = Some(struct_info);
                    notify_off_multiplier = root.config_read_word(
                        device_function,
                        capability.offset + CAP_NOTIFY_OFF_MULTIPLIER_OFFSET,
                    );
                }
                VIRTIO_PCI_CAP_ISR_CFG if isr_cfg.is_none() => {
                    isr_cfg = Some(struct_info);
                }
                VIRTIO_PCI_CAP_DEVICE_CFG if device_cfg.is_none() => {
                    device_cfg = Some(struct_info);
                }
                _ => {}
            }
        }

        let common_cfg = get_bar_region::<H, _>(
            root,
            device_function,
            &common_cfg.ok_or(VirtioPciError::MissingCommonConfig)?,
        )?;

        let notify_cfg = notify_cfg.ok_or(VirtioPciError::MissingNotifyConfig)?;
        if notify_off_multiplier % 2 != 0 {
            return Err(VirtioPciError::InvalidNotifyOffMultiplier(
                notify_off_multiplier,
            ));
        }
        let notify_region = get_bar_region_slice::<H, _>(root, device_function, &notify_cfg)?;

        let isr_status = get_bar_region::<H, _>(
            root,
            device_function,
            &isr_cfg.ok_or(VirtioPciError::MissingIsrConfig)?,
        )?;

        let config_space = if let Some(device_cfg) = device_cfg {
            Some(get_bar_region_slice::<H, _>(
                root,
                device_function,
                &device_cfg,
            )?)
        } else {
            None
        };

        Ok(Self {
            device_type,
            device_function,
            common_cfg,
            notify_region,
            notify_off_multiplier,
            isr_status,
            config_space,
        })
    }
}

impl Transport for PciTransport {
    fn device_type(&self) -> DeviceType {
        self.device_type
    }

    fn read_device_features(&mut self) -> u64 {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        unsafe {
            volwrite!(self.common_cfg, device_feature_select, 0);
            let mut device_features_bits = volread!(self.common_cfg, device_feature) as u64;
            volwrite!(self.common_cfg, device_feature_select, 1);
            device_features_bits |= (volread!(self.common_cfg, device_feature) as u64) << 32;
            device_features_bits
        }
    }

    fn write_driver_features(&mut self, driver_features: u64) {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        unsafe {
            volwrite!(self.common_cfg, driver_feature_select, 0);
            volwrite!(self.common_cfg, driver_feature, driver_features as u32);
            volwrite!(self.common_cfg, driver_feature_select, 1);
            volwrite!(
                self.common_cfg,
                driver_feature,
                (driver_features >> 32) as u32
            );
        }
    }

    fn max_queue_size(&mut self, queue: u16) -> u32 {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        unsafe {
            volwrite!(self.common_cfg, queue_select, queue);
            volread!(self.common_cfg, queue_size).into()
        }
    }

    fn notify(&mut self, queue: u16) {
        // Safe because the common config and notify region pointers are valid and we checked in
        // get_bar_region that they were aligned.
        unsafe {
            volwrite!(self.common_cfg, queue_select, queue);
            // TODO: Consider caching this somewhere (per queue).
            let queue_notify_off = volread!(self.common_cfg, queue_notify_off);

            let offset_bytes = usize::from(queue_notify_off) * self.notify_off_multiplier as usize;
            let index = offset_bytes / size_of::<u16>();
            addr_of_mut!((*self.notify_region.as_ptr())[index]).vwrite(queue);
        }
    }

    fn get_status(&self) -> DeviceStatus {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        let status = unsafe { volread!(self.common_cfg, device_status) };
        DeviceStatus::from_bits_truncate(status.into())
    }

    fn set_status(&mut self, status: DeviceStatus) {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        unsafe {
            volwrite!(self.common_cfg, device_status, status.bits() as u8);
        }
    }

    fn set_guest_page_size(&mut self, _guest_page_size: u32) {
        // No-op, the PCI transport doesn't care.
    }

    fn requires_legacy_layout(&self) -> bool {
        false
    }

    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: PhysAddr,
        driver_area: PhysAddr,
        device_area: PhysAddr,
    ) {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        unsafe {
            volwrite!(self.common_cfg, queue_select, queue);
            volwrite!(self.common_cfg, queue_size, size as u16);
            volwrite!(self.common_cfg, queue_desc, descriptors as u64);
            volwrite!(self.common_cfg, queue_driver, driver_area as u64);
            volwrite!(self.common_cfg, queue_device, device_area as u64);
            volwrite!(self.common_cfg, queue_enable, 1);
        }
    }

    fn queue_unset(&mut self, _queue: u16) {
        // The VirtIO spec doesn't allow queues to be unset once they have been set up for the PCI
        // transport, so this is a no-op.
    }

    fn queue_used(&mut self, queue: u16) -> bool {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        unsafe {
            volwrite!(self.common_cfg, queue_select, queue);
            volread!(self.common_cfg, queue_enable) == 1
        }
    }

    fn ack_interrupt(&mut self) -> bool {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        // Reading the ISR status resets it to 0 and causes the device to de-assert the interrupt.
        let isr_status = unsafe { self.isr_status.as_ptr().vread() };
        // TODO: Distinguish between queue interrupt and device configuration interrupt.
        isr_status & 0x3 != 0
    }

    fn config_space<T>(&self) -> Result<NonNull<T>, Error> {
        if let Some(config_space) = self.config_space {
            if size_of::<T>() > config_space.len() * size_of::<u32>() {
                Err(Error::ConfigSpaceTooSmall)
            } else if align_of::<T>() > 4 {
                // Panic as this should only happen if the driver is written incorrectly.
                panic!(
                    "Driver expected config space alignment of {} bytes, but VirtIO only guarantees 4 byte alignment.",
                    align_of::<T>()
                );
            } else {
                // TODO: Use NonNull::as_non_null_ptr once it is stable.
                let config_space_ptr = NonNull::new(config_space.as_ptr() as *mut u32).unwrap();
                Ok(config_space_ptr.cast())
            }
        } else {
            Err(Error::ConfigSpaceMissing)
        }
    }
}

impl Drop for PciTransport {
    fn drop(&mut self) {
        // Reset the device when the transport is dropped.
        self.set_status(DeviceStatus::empty());
        while self.get_status() != DeviceStatus::empty() {}
    }
}

/// `virtio_pci_common_cfg`, see 4.1.4.3 "Common configuration structure layout".
#[repr(C)]
struct CommonCfg {
    device_feature_select: Volatile<u32>,
    device_feature: ReadOnly<u32>,
    driver_feature_select: Volatile<u32>,
    driver_feature: Volatile<u32>,
    msix_config: Volatile<u16>,
    num_queues: ReadOnly<u16>,
    device_status: Volatile<u8>,
    config_generation: ReadOnly<u8>,
    queue_select: Volatile<u16>,
    queue_size: Volatile<u16>,
    queue_msix_vector: Volatile<u16>,
    queue_enable: Volatile<u16>,
    queue_notify_off: Volatile<u16>,
    queue_desc: Volatile<u64>,
    queue_driver: Volatile<u64>,
    queue_device: Volatile<u64>,
}

/// Information about a VirtIO structure within some BAR, as provided by a `virtio_pci_cap`.
#[derive(Clone, Debug, Eq, PartialEq)]
struct VirtioCapabilityInfo {
    /// The bar in which the structure can be found.
    bar: u8,
    /// The offset within the bar.
    offset: u32,
    /// The length in bytes of the structure within the bar.
    length: u32,
}

fn get_bar_region<H: Hal, T>(
    root: &mut PciRoot,
    device_function: DeviceFunction,
    struct_info: &VirtioCapabilityInfo,
) -> Result<NonNull<T>, VirtioPciError> {
    let bar_info = root.bar_info(device_function, struct_info.bar)?;
    let (bar_address, bar_size) = bar_info
        .memory_address_size()
        .ok_or(VirtioPciError::UnexpectedIoBar)?;
    if bar_address == 0 {
        return Err(VirtioPciError::BarNotAllocated(struct_info.bar));
    }
    if struct_info.offset + struct_info.length > bar_size
        || size_of::<T>() > struct_info.length as usize
    {
        return Err(VirtioPciError::BarOffsetOutOfRange);
    }
    let paddr = bar_address as PhysAddr + struct_info.offset as PhysAddr;
    // Safe because the paddr and size describe a valid MMIO region, at least according to the PCI
    // bus.
    let vaddr = unsafe { H::mmio_phys_to_virt(paddr, struct_info.length as usize) };
    if vaddr.as_ptr() as usize % align_of::<T>() != 0 {
        return Err(VirtioPciError::Misaligned {
            vaddr,
            alignment: align_of::<T>(),
        });
    }
    Ok(vaddr.cast())
}

fn get_bar_region_slice<H: Hal, T>(
    root: &mut PciRoot,
    device_function: DeviceFunction,
    struct_info: &VirtioCapabilityInfo,
) -> Result<NonNull<[T]>, VirtioPciError> {
    let ptr = get_bar_region::<H, T>(root, device_function, struct_info)?;
    Ok(nonnull_slice_from_raw_parts(
        ptr,
        struct_info.length as usize / size_of::<T>(),
    ))
}

/// An error encountered initialising a VirtIO PCI transport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VirtioPciError {
    /// PCI device vender ID was not the VirtIO vendor ID.
    InvalidVendorId(u16),
    /// No valid `VIRTIO_PCI_CAP_COMMON_CFG` capability was found.
    MissingCommonConfig,
    /// No valid `VIRTIO_PCI_CAP_NOTIFY_CFG` capability was found.
    MissingNotifyConfig,
    /// `VIRTIO_PCI_CAP_NOTIFY_CFG` capability has a `notify_off_multiplier` that is not a multiple
    /// of 2.
    InvalidNotifyOffMultiplier(u32),
    /// No valid `VIRTIO_PCI_CAP_ISR_CFG` capability was found.
    MissingIsrConfig,
    /// An IO BAR was provided rather than a memory BAR.
    UnexpectedIoBar,
    /// A BAR which we need was not allocated an address.
    BarNotAllocated(u8),
    /// The offset for some capability was greater than the length of the BAR.
    BarOffsetOutOfRange,
    /// The virtual address was not aligned as expected.
    Misaligned {
        /// The virtual address in question.
        vaddr: NonNull<u8>,
        /// The expected alignment in bytes.
        alignment: usize,
    },
    /// A generic PCI error,
    Pci(PciError),
}

impl Display for VirtioPciError {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            Self::InvalidVendorId(vendor_id) => write!(
                f,
                "PCI device vender ID {:#06x} was not the VirtIO vendor ID {:#06x}.",
                vendor_id, VIRTIO_VENDOR_ID
            ),
            Self::MissingCommonConfig => write!(
                f,
                "No valid `VIRTIO_PCI_CAP_COMMON_CFG` capability was found."
            ),
            Self::MissingNotifyConfig => write!(
                f,
                "No valid `VIRTIO_PCI_CAP_NOTIFY_CFG` capability was found."
            ),
            Self::InvalidNotifyOffMultiplier(notify_off_multiplier) => {
                write!(
                    f,
                    "`VIRTIO_PCI_CAP_NOTIFY_CFG` capability has a `notify_off_multiplier` that is not a multiple of 2: {}",
                    notify_off_multiplier
                )
            }
            Self::MissingIsrConfig => {
                write!(f, "No valid `VIRTIO_PCI_CAP_ISR_CFG` capability was found.")
            }
            Self::UnexpectedIoBar => write!(f, "Unexpected IO BAR (expected memory BAR)."),
            Self::BarNotAllocated(bar_index) => write!(f, "Bar {} not allocated.", bar_index),
            Self::BarOffsetOutOfRange => write!(f, "Capability offset greater than BAR length."),
            Self::Misaligned { vaddr, alignment } => write!(
                f,
                "Virtual address {:#018?} was not aligned to a {} byte boundary as expected.",
                vaddr, alignment
            ),
            Self::Pci(pci_error) => pci_error.fmt(f),
        }
    }
}

impl From<PciError> for VirtioPciError {
    fn from(error: PciError) -> Self {
        Self::Pci(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitional_device_ids() {
        assert_eq!(device_type(0x1000), DeviceType::Network);
        assert_eq!(device_type(0x1002), DeviceType::MemoryBalloon);
        assert_eq!(device_type(0x1009), DeviceType::_9P);
    }

    #[test]
    fn offset_device_ids() {
        assert_eq!(device_type(0x1045), DeviceType::MemoryBalloon);
        assert_eq!(device_type(0x1049), DeviceType::_9P);
        assert_eq!(device_type(0x1058), DeviceType::Memory);
        assert_eq!(device_type(0x1040), DeviceType::Invalid);
        assert_eq!(device_type(0x1059), DeviceType::Invalid);
    }

    #[test]
    fn virtio_device_type_valid() {
        assert_eq!(
            virtio_device_type(&DeviceFunctionInfo {
                vendor_id: VIRTIO_VENDOR_ID,
                device_id: TRANSITIONAL_BLOCK,
                class: 0,
                subclass: 0,
                prog_if: 0,
                revision: 0,
                header_type: bus::HeaderType::Standard,
            }),
            Some(DeviceType::Block)
        );
    }

    #[test]
    fn virtio_device_type_invalid() {
        // Non-VirtIO vendor ID.
        assert_eq!(
            virtio_device_type(&DeviceFunctionInfo {
                vendor_id: 0x1234,
                device_id: TRANSITIONAL_BLOCK,
                class: 0,
                subclass: 0,
                prog_if: 0,
                revision: 0,
                header_type: bus::HeaderType::Standard,
            }),
            None
        );

        // Invalid device ID.
        assert_eq!(
            virtio_device_type(&DeviceFunctionInfo {
                vendor_id: VIRTIO_VENDOR_ID,
                device_id: 0x1040,
                class: 0,
                subclass: 0,
                prog_if: 0,
                revision: 0,
                header_type: bus::HeaderType::Standard,
            }),
            None
        );
    }
}
