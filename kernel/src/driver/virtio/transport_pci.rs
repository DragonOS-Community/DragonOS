//! PCI transport for VirtIO.

use crate::arch::{
    msi::{arch_pci_msi_vector_alloc, arch_pci_msi_vector_setup},
    PciArch, TraitPciArch,
};
use crate::driver::base::device::DeviceId;
use crate::driver::pci::pci::{
    BusDeviceFunction, PciAddr, PciBarMappingRequest, PciBarSubresourceGuard, PciDeviceStructure,
    PciDeviceStructureGeneralDevice, PciError, PciStandardDeviceBar, PCI_CAP_ID_VNDR,
};

use crate::driver::pci::root::pci_root_0;

use crate::exception::IrqNumber;

use crate::libs::volatile::{ReadOnly, Volatile, VolatileReadable, VolatileWritable, WriteOnly};
use crate::mm::{MemoryManagementArch, PhysAddr, VirtAddr};

use alloc::{sync::Arc, vec, vec::Vec};
use core::{
    fmt::{self, Display, Formatter},
    mem::{align_of, size_of},
    ptr::{self, addr_of_mut, NonNull},
};
use log::warn;
use virtio_drivers::{
    transport::{DeviceStatus, DeviceType, Transport},
    Error, Hal, PhysAddr as VirtioPhysAddr,
};

use super::transport::VirtioSharedMemoryRegion;
use super::VIRTIO_VENDOR_ID;
use crate::driver::pci::pci_irq::IrqType;

/// The offset to add to a VirtIO device ID to get the corresponding PCI device ID.
/// The offset of the PCI VirtIO device ID.
const PCI_DEVICE_ID_OFFSET: u16 = 0x1040;
/// PCI VirtIO device IDs and their corresponding device types.
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
const CAP_SHARED_MEMORY_OFFSET_HI_OFFSET: u8 = 16;
const CAP_SHARED_MEMORY_LENGTH_HI_OFFSET: u8 = 20;
const VIRTIO_PCI_CAP64_LEN: u8 = 24;

/// Common configuration.
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
/// Notifications.
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
/// ISR Status.
const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
/// Device specific configuration.
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;
/// Additional shared memory capability.
const VIRTIO_PCI_CAP_SHARED_MEMORY_CFG: u8 = 8;

fn shared_memory_cap_len_supported(cap_len: u8) -> bool {
    cap_len >= VIRTIO_PCI_CAP64_LEN
}

/// The MSI-X table entry index for VirtIO device receive interrupts.
const VIRTIO_RECV_VECTOR_INDEX: u16 = 0;
// Receive queue number
const QUEUE_RECEIVE: u16 = 0;
/// Converts a PCI device ID to the corresponding VirtIO device type.
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

/// PCI transport for VirtIO.
///
/// Ref: 4.1 Virtio Over PCI Bus
#[allow(dead_code)]
#[derive(Debug)]
pub struct PciTransport {
    device_type: DeviceType,
    /// The bus, device and function identifier for the VirtIO device.
    _bus_device_function: BusDeviceFunction,
    /// The common configuration structure within some BAR.
    common_cfg: NonNull<CommonCfg>,
    /// The start of the queue notification region within some BAR.
    notify_region: NonNull<[WriteOnly<u16>]>,
    notify_off_multiplier: u32,
    /// Cached notify-region indices keyed by virtqueue index.
    queue_notify_indices: Vec<Option<usize>>,
    /// The ISR status register within some BAR.
    isr_status: NonNull<Volatile<u8>>,
    /// The VirtIO device-specific configuration within some BAR.
    config_space: Option<NonNull<[u32]>>,
    dev_id: Arc<DeviceId>,
    device: Arc<PciDeviceStructureGeneralDevice>,
    shared_memory_regions: Vec<(u8, Option<VirtioSharedMemoryRegion>)>,
}

#[derive(Debug, Clone, Copy)]
pub struct PciInterruptAck {
    isr_status: NonNull<Volatile<u8>>,
}

// The handle only permits an atomic MMIO ISR-status read used for interrupt acknowledgement.
// Queue/config mutation remains owned by PciTransport.
unsafe impl Send for PciInterruptAck {}
unsafe impl Sync for PciInterruptAck {}

impl PciInterruptAck {
    pub fn ack_interrupt(&self) -> bool {
        // Safe because the ISR status pointer comes from a valid VirtIO PCI ISR BAR region.
        // Reading the ISR status resets it to 0 and causes the device to de-assert the interrupt.
        let isr_status = unsafe { self.isr_status.as_ptr().vread() };
        // TODO: Distinguish between queue interrupt and device configuration interrupt.
        isr_status & 0x3 != 0
    }
}

impl PciTransport {
    /// Construct a new PCI VirtIO device driver for the given device function on the given PCI
    /// root controller.
    ///
    /// ## Parameters
    ///
    /// - `device` - The PCI device structure for the VirtIO device.
    /// - `irq_handler` - An optional handler for the device's interrupt. If `None`, a default
    ///   handler `DefaultVirtioIrqHandler` will be used.
    /// - `irq_number_offset` - Currently, this parameter is just simple make a offset to the irq number, cause it's not be allowed to have the same irq number within different device
    #[allow(clippy::extra_unused_type_parameters)]
    pub fn new<H: Hal>(
        device: Arc<PciDeviceStructureGeneralDevice>,
        dev_id: Arc<DeviceId>,
    ) -> Result<Self, VirtioPciError> {
        let header = &device.common_header;
        let bus_device_function = header.bus_device_function;
        if header.vendor_id != VIRTIO_VENDOR_ID {
            return Err(VirtioPciError::InvalidVendorId(header.vendor_id));
        }
        let device_type = device_type(header.device_id);
        // Find the PCI capabilities we need.
        let mut common_cfg: Option<VirtioCapabilityInfo> = None;
        let mut notify_cfg: Option<VirtioCapabilityInfo> = None;
        let mut notify_off_multiplier = 0;
        let mut isr_cfg = None;
        let mut device_cfg = None;
        let mut shared_memory_regions = Vec::new();
        let mut shared_memory_bars = Vec::new();
        let mut transport_config_mappings = Vec::new();
        let mut mapped_config_types = Vec::new();
        for capability in device.capabilities().unwrap() {
            if capability.id != PCI_CAP_ID_VNDR {
                continue;
            }
            let cap_len = capability.private_header as u8;
            let cfg_type = (capability.private_header >> 8) as u8;
            if cap_len < 16 || capability.offset.checked_add(cap_len - 1).is_none() {
                continue;
            }
            let bar = pci_root_0().read_config(
                bus_device_function,
                (capability.offset + CAP_BAR_OFFSET).into(),
            ) as u8;
            let valid_transport_config = matches!(
                cfg_type,
                VIRTIO_PCI_CAP_COMMON_CFG
                    | VIRTIO_PCI_CAP_NOTIFY_CFG
                    | VIRTIO_PCI_CAP_ISR_CFG
                    | VIRTIO_PCI_CAP_DEVICE_CFG
            ) && (cfg_type != VIRTIO_PCI_CAP_NOTIFY_CFG
                || cap_len >= 20);
            if valid_transport_config && bar < 6 && !mapped_config_types.contains(&cfg_type) {
                let offset = pci_root_0().read_config(
                    bus_device_function,
                    (capability.offset + CAP_BAR_OFFSET_OFFSET).into(),
                );
                let length = pci_root_0().read_config(
                    bus_device_function,
                    (capability.offset + CAP_LENGTH_OFFSET).into(),
                );
                transport_config_mappings.push(PciBarMappingRequest {
                    bar,
                    offset: u64::from(offset),
                    length: u64::from(length),
                });
                mapped_config_types.push(cfg_type);
            }
            if cfg_type != VIRTIO_PCI_CAP_SHARED_MEMORY_CFG
                || !shared_memory_cap_len_supported(cap_len)
                || capability.offset.checked_add(cap_len - 1).is_none()
            {
                continue;
            }
            if bar < 6 && !shared_memory_bars.contains(&bar) {
                shared_memory_bars.push(bar);
            }
        }
        transport_config_mappings.extend(device.msix_mapping_requests()?);
        device
            .bar_ioremap_with_mappings(&shared_memory_bars, &transport_config_mappings)
            .unwrap()?;
        device.enable_master();
        // panic!();
        // device_capability is an iterator; iterating over it traverses all capability space.
        for capability in device.capabilities().unwrap() {
            if capability.id != PCI_CAP_ID_VNDR {
                continue;
            }
            let cap_len = capability.private_header as u8;
            let cfg_type = (capability.private_header >> 8) as u8;
            if cap_len < 16 {
                continue;
            }
            if capability.offset.checked_add(cap_len - 1).is_none() {
                continue;
            }
            let struct_info = VirtioCapabilityInfo {
                bar: pci_root_0().read_config(
                    bus_device_function,
                    (capability.offset + CAP_BAR_OFFSET).into(),
                ) as u8,
                offset: pci_root_0().read_config(
                    bus_device_function,
                    (capability.offset + CAP_BAR_OFFSET_OFFSET).into(),
                ),
                length: pci_root_0().read_config(
                    bus_device_function,
                    (capability.offset + CAP_LENGTH_OFFSET).into(),
                ),
            };

            match cfg_type {
                VIRTIO_PCI_CAP_COMMON_CFG if struct_info.bar < 6 && common_cfg.is_none() => {
                    common_cfg = Some(struct_info);
                }
                VIRTIO_PCI_CAP_NOTIFY_CFG
                    if cap_len >= 20 && struct_info.bar < 6 && notify_cfg.is_none() =>
                {
                    notify_cfg = Some(struct_info);
                    notify_off_multiplier = pci_root_0().read_config(
                        bus_device_function,
                        (capability.offset + CAP_NOTIFY_OFF_MULTIPLIER_OFFSET).into(),
                    );
                }
                VIRTIO_PCI_CAP_ISR_CFG if struct_info.bar < 6 && isr_cfg.is_none() => {
                    isr_cfg = Some(struct_info);
                }
                VIRTIO_PCI_CAP_DEVICE_CFG if struct_info.bar < 6 && device_cfg.is_none() => {
                    device_cfg = Some(struct_info);
                }
                VIRTIO_PCI_CAP_SHARED_MEMORY_CFG if shared_memory_cap_len_supported(cap_len) => {
                    if struct_info.bar >= 6 {
                        continue;
                    }
                    let bar_and_id = pci_root_0().read_config(
                        bus_device_function,
                        (capability.offset + CAP_BAR_OFFSET).into(),
                    );
                    let id = (bar_and_id >> 8) as u8;
                    if shared_memory_regions
                        .iter()
                        .any(|(seen_id, _)| *seen_id == id)
                    {
                        continue;
                    }

                    let offset_hi = pci_root_0().read_config(
                        bus_device_function,
                        (capability.offset + CAP_SHARED_MEMORY_OFFSET_HI_OFFSET).into(),
                    );
                    let length_hi = pci_root_0().read_config(
                        bus_device_function,
                        (capability.offset + CAP_SHARED_MEMORY_LENGTH_HI_OFFSET).into(),
                    );
                    let offset = u64::from(struct_info.offset) | (u64::from(offset_hi) << 32);
                    let length = u64::from(struct_info.length) | (u64::from(length_hi) << 32);
                    let region = validate_shared_memory_region(
                        &device.standard_device_bar.read(),
                        struct_info.bar,
                        offset,
                        length,
                    )
                    .filter(|_| {
                        !shared_memory_overlaps_mappings(
                            struct_info.bar,
                            offset,
                            length,
                            &transport_config_mappings,
                        )
                    });
                    if region.is_none() {
                        warn!(
                            "VirtIO PCI shared-memory capability id={} is outside its BAR or cannot be represented",
                            id
                        );
                    }
                    shared_memory_regions.push((id, region));
                }
                _ => {}
            }
        }

        let common_cfg = get_bar_region::<CommonCfg>(
            &device.standard_device_bar.read(),
            &common_cfg.ok_or(VirtioPciError::MissingCommonConfig)?,
        )?;

        let notify_cfg = notify_cfg.ok_or(VirtioPciError::MissingNotifyConfig)?;
        if notify_off_multiplier % 2 != 0 {
            return Err(VirtioPciError::InvalidNotifyOffMultiplier(
                notify_off_multiplier,
            ));
        }
        //debug!("notify.offset={},notify.length={}",notify_cfg.offset,notify_cfg.length);
        let notify_region =
            get_bar_region_slice::<_>(&device.standard_device_bar.read(), &notify_cfg)?;
        let queue_count = unsafe { volread!(common_cfg, num_queues) as usize };
        let queue_notify_indices = vec![None; queue_count];
        let isr_status = get_bar_region::<_>(
            &device.standard_device_bar.read(),
            &isr_cfg.ok_or(VirtioPciError::MissingIsrConfig)?,
        )?;
        let config_space = if let Some(device_cfg) = device_cfg {
            Some(get_bar_region_slice::<_>(
                &device.standard_device_bar.read(),
                &device_cfg,
            )?)
        } else {
            None
        };
        Ok(Self {
            device_type,
            _bus_device_function: bus_device_function,
            common_cfg,
            notify_region,
            notify_off_multiplier,
            queue_notify_indices,
            isr_status,
            config_space,
            dev_id,
            device,
            shared_memory_regions,
        })
    }

    pub fn pci_device(&self) -> Arc<PciDeviceStructureGeneralDevice> {
        self.device.clone()
    }

    /// Reserves the CPU vector when an interrupt-driven driver actually requests IRQ setup.
    /// Polling-only users such as virtio-vsock do not consume an MSI vector.
    pub(super) fn setup_irq_vector(&self) -> Result<IrqNumber, VirtioPciError> {
        let standard_device = self.device.as_standard_device().unwrap();
        let irq_vector = standard_device.irq_vector_mut().unwrap();
        let mut irq_vector = irq_vector.write();
        if !irq_vector.is_empty() {
            return Err(VirtioPciError::IrqVectorAlreadyAssigned);
        }
        let irq = arch_pci_msi_vector_alloc().ok_or(VirtioPciError::IrqVectorUnavailable)?;
        arch_pci_msi_vector_setup(irq).map_err(|_| VirtioPciError::IrqVectorUnavailable)?;
        irq_vector.push(irq);
        Ok(irq)
    }

    pub fn irq(&self) -> IrqNumber {
        *self
            .device
            .as_standard_device()
            .unwrap()
            .irq_vector_mut()
            .unwrap()
            .read()
            .first()
            .expect("VirtIO PCI IRQ must be configured before it is queried")
    }

    pub fn interrupt_ack(&self) -> PciInterruptAck {
        PciInterruptAck {
            isr_status: self.isr_status,
        }
    }

    pub fn ack_interrupt_ref(&self) -> bool {
        self.interrupt_ack().ack_interrupt()
    }

    pub fn shared_memory_region(&self, id: u8) -> Option<VirtioSharedMemoryRegion> {
        self.shared_memory_regions
            .iter()
            .find(|(region_id, _)| *region_id == id)
            .and_then(|(_, region)| *region)
    }

    pub fn reserve_shared_memory_region(
        &self,
        region: VirtioSharedMemoryRegion,
    ) -> Result<PciBarSubresourceGuard, PciError> {
        PciBarSubresourceGuard::reserve(
            &self.device,
            region.bar(),
            region.offset(),
            region.length(),
            region.physical_address(),
        )
    }

    fn cache_queue_notify_index(&mut self, queue: u16) -> Option<usize> {
        let queue_index = queue as usize;
        if queue_index >= self.queue_notify_indices.len() {
            warn!(
                "VirtIO PCI notify queue {} out of range, num_queues={}",
                queue,
                self.queue_notify_indices.len()
            );
            return None;
        }

        unsafe {
            volwrite!(self.common_cfg, queue_select, queue);
            let queue_notify_off = volread!(self.common_cfg, queue_notify_off);
            let offset_bytes =
                usize::from(queue_notify_off).checked_mul(self.notify_off_multiplier as usize);
            let Some(offset_bytes) = offset_bytes else {
                warn!(
                    "VirtIO PCI notify offset overflow: queue={}, notify_off={}, multiplier={}",
                    queue, queue_notify_off, self.notify_off_multiplier
                );
                return None;
            };

            let Some(end_offset_bytes) = offset_bytes.checked_add(size_of::<u16>()) else {
                warn!(
                    "VirtIO PCI notify offset end overflow: queue={}, offset_bytes={}",
                    queue, offset_bytes
                );
                return None;
            };
            let notify_region_len_bytes = self.notify_region.len() * size_of::<u16>();
            if end_offset_bytes > notify_region_len_bytes {
                warn!(
                    "VirtIO PCI notify offset out of range: queue={}, offset_bytes={}, notify_region_len_bytes={}",
                    queue,
                    offset_bytes,
                    notify_region_len_bytes
                );
                return None;
            }

            let index = offset_bytes / size_of::<u16>();
            self.queue_notify_indices[queue_index] = Some(index);
            Some(index)
        }
    }

    fn fail_bad_notify_config(&mut self, queue: u16) -> ! {
        self.set_status(DeviceStatus::FAILED);
        panic!(
            "VirtIO PCI queue {} has invalid or missing notification register",
            queue
        );
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
        unsafe {
            volwrite!(self.common_cfg, queue_select, queue);
            volread!(self.common_cfg, queue_size).into()
        }
    }

    fn notify(&mut self, queue: u16) {
        let queue_index = queue as usize;
        let Some(index) = self
            .queue_notify_indices
            .get(queue_index)
            .copied()
            .flatten()
            .or_else(|| self.cache_queue_notify_index(queue))
        else {
            self.fail_bad_notify_config(queue);
        };

        // Safe because notify_region is a valid BAR mapping and the cached index is bounds-checked.
        unsafe { addr_of_mut!((*self.notify_region.as_ptr())[index]).vwrite(queue) };
    }

    fn set_status(&mut self, status: DeviceStatus) {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        unsafe {
            volwrite!(self.common_cfg, device_status, status.bits() as u8);
        }
    }

    fn get_status(&self) -> DeviceStatus {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        unsafe { DeviceStatus::from_bits_truncate(volread!(self.common_cfg, device_status).into()) }
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
        descriptors: VirtioPhysAddr,
        driver_area: VirtioPhysAddr,
        device_area: VirtioPhysAddr,
    ) {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        unsafe {
            volwrite!(self.common_cfg, queue_select, queue);
            volwrite!(self.common_cfg, queue_size, size as u16);
            volwrite!(self.common_cfg, queue_desc, descriptors as u64);
            volwrite!(self.common_cfg, queue_driver, driver_area as u64);
            volwrite!(self.common_cfg, queue_device, device_area as u64);
            if self.cache_queue_notify_index(queue).is_none() {
                self.fail_bad_notify_config(queue);
            }
            // Set the interrupt entry corresponding to the queue interrupt
            if matches!(*self.device.irq_type.read(), IrqType::Msix { .. }) {
                if queue == QUEUE_RECEIVE {
                    volwrite!(self.common_cfg, msix_config, VIRTIO_RECV_VECTOR_INDEX);
                    // let cfg_vector = volread!(self.common_cfg, msix_config);
                    // debug!(
                    //     "VirtIO PCI msix_config readback: vector {:#x}",
                    //     cfg_vector
                    // );
                }
                volwrite!(self.common_cfg, queue_msix_vector, VIRTIO_RECV_VECTOR_INDEX);
                let vector = volread!(self.common_cfg, queue_msix_vector);
                // if self.device_type == DeviceType::Network && (queue == 0 || queue == 1) {
                //     debug!(
                //         "VirtIO PCI net queue_msix_vector readback: queue {}, vector {:#x}",
                //         queue, vector
                //     );
                // }
                if vector != VIRTIO_RECV_VECTOR_INDEX {
                    panic!("Vector set failed");
                }
            }
            volwrite!(self.common_cfg, queue_enable, 1);
        }
    }

    fn queue_unset(&mut self, queue: u16) {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        unsafe {
            volwrite!(self.common_cfg, queue_select, queue);
            volwrite!(self.common_cfg, queue_size, 0);
            volwrite!(self.common_cfg, queue_desc, 0);
            volwrite!(self.common_cfg, queue_driver, 0);
            volwrite!(self.common_cfg, queue_device, 0);
        }
        if let Some(index) = self.queue_notify_indices.get_mut(queue as usize) {
            *index = None;
        }
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
        self.ack_interrupt_ref()
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

        // TODO: Call the PCI interrupt release function and remove the corresponding device
        // interrupt from virtio_irq_manager
    }
}

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
/// Information about which BAR a VirtIO structure resides in, at what offset, and its length.
#[derive(Clone, Debug, Eq, PartialEq)]
struct VirtioCapabilityInfo {
    /// The bar in which the structure can be found.
    bar: u8,
    /// The offset within the bar.
    offset: u32,
    /// The length in bytes of the structure within the bar.
    length: u32,
}

/// An error encountered initialising a VirtIO PCI transport.
/// An error encountered during VirtIO PCI transport initialization.
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
    UnexpectedBarType,
    /// A BAR which we need was not allocated an address.
    BarNotAllocated(u8),
    /// The offset for some capability was greater than the length of the BAR.
    BarOffsetOutOfRange,
    /// The virtual address was not aligned as expected.
    Misaligned {
        /// The virtual address in question.
        vaddr: VirtAddr,
        /// The expected alignment in bytes.
        alignment: usize,
    },
    /// Failed to obtain the virtual address.
    BarGetVaddrFailed,
    /// A generic PCI error,
    Pci(PciError),
    /// No CPU vector is available from the architecture PCI MSI/MSI-X range.
    IrqVectorUnavailable,
    /// The same PCI function was already assigned an interrupt vector.
    IrqVectorAlreadyAssigned,
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
            Self::UnexpectedBarType => write!(f, "Unexpected BAR (expected memory BAR)."),
            Self::BarNotAllocated(bar_index) => write!(f, "Bar {} not allocated.", bar_index),
            Self::BarOffsetOutOfRange => write!(f, "Capability offset greater than BAR length."),
            Self::Misaligned { vaddr, alignment } => write!(
                f,
                "Virtual address {:?} was not aligned to a {} byte boundary as expected.",
                vaddr, alignment
            ),
            Self::BarGetVaddrFailed => write!(f, "Get bar virtaddress failed"),
            Self::Pci(pci_error) => pci_error.fmt(f),
            Self::IrqVectorUnavailable => write!(f, "No PCI MSI/MSI-X CPU vector is available."),
            Self::IrqVectorAlreadyAssigned => {
                write!(
                    f,
                    "The PCI function already has an assigned interrupt vector."
                )
            }
        }
    }
}

/// Conversion from `PciError` to `VirtioPciError`, propagated up through the layers.
impl From<PciError> for VirtioPciError {
    fn from(error: PciError) -> Self {
        Self::Pci(error)
    }
}

/// Obtains a virtual address and casts it to a pointer of the corresponding type.
///
/// * `device_bar` - The BAR info structure.
/// * `struct_info` - The location info of the config space.
///
/// Returns a pointer of type `T` on success, or a `VirtioPciError` on failure.
fn get_bar_region<T>(
    device_bar: &PciStandardDeviceBar,
    struct_info: &VirtioCapabilityInfo,
) -> Result<NonNull<T>, VirtioPciError> {
    let bar_info = device_bar.get_bar(struct_info.bar)?;
    let (bar_address, bar_size) = bar_info
        .memory_address_size()
        .ok_or(VirtioPciError::UnexpectedBarType)?;
    if bar_address == 0 {
        return Err(VirtioPciError::BarNotAllocated(struct_info.bar));
    }
    if u64::from(struct_info.offset)
        .checked_add(u64::from(struct_info.length))
        .is_none_or(|end| end > bar_size)
        || size_of::<T>() > struct_info.length as usize
    {
        return Err(VirtioPciError::BarOffsetOutOfRange);
    }
    //debug!("Chossed bar ={},used={}",struct_info.bar,struct_info.offset + struct_info.length);
    let vaddr = bar_info
        .virtual_address_at(u64::from(struct_info.offset), struct_info.length as usize)
        .ok_or(VirtioPciError::BarGetVaddrFailed)?;
    if !vaddr.data().is_multiple_of(align_of::<T>()) {
        return Err(VirtioPciError::Misaligned {
            vaddr,
            alignment: align_of::<T>(),
        });
    }
    let vaddr = NonNull::new(vaddr.data() as *mut u8).unwrap();
    Ok(vaddr.cast())
}

/// Obtains a virtual address and casts it to a pointer to a slice of the corresponding type.
///
/// * `device_bar` - The BAR info structure.
/// * `struct_info` - The location info of the config space.
///
/// Returns a pointer to a `[T]` slice on success, or a `VirtioPciError` on failure.
fn get_bar_region_slice<T>(
    device_bar: &PciStandardDeviceBar,
    struct_info: &VirtioCapabilityInfo,
) -> Result<NonNull<[T]>, VirtioPciError> {
    let ptr = get_bar_region::<T>(device_bar, struct_info)?;
    // let raw_slice =
    //     ptr::slice_from_raw_parts_mut(ptr.as_ptr(), struct_info.length as usize / size_of::<T>());
    Ok(nonnull_slice_from_raw_parts(
        ptr,
        struct_info.length as usize / size_of::<T>(),
    ))
}

fn nonnull_slice_from_raw_parts<T>(data: NonNull<T>, len: usize) -> NonNull<[T]> {
    NonNull::new(ptr::slice_from_raw_parts_mut(data.as_ptr(), len)).unwrap()
}

fn validate_shared_memory_region(
    device_bar: &PciStandardDeviceBar,
    bar: u8,
    offset: u64,
    length: u64,
) -> Option<VirtioSharedMemoryRegion> {
    let bar_info = device_bar.get_bar(bar).ok()?;
    let (bar_address, bar_size) = bar_info.memory_address_size()?;
    validate_shared_memory_range(bar_address, bar_size, bar, offset, length)
}

fn shared_memory_overlaps_mappings(
    bar: u8,
    offset: u64,
    length: u64,
    mappings: &[PciBarMappingRequest],
) -> bool {
    let Some(end) = offset.checked_add(length) else {
        return true;
    };
    let page_mask = !((crate::arch::MMArch::PAGE_SIZE as u64) - 1);
    let cache_start = offset & page_mask;
    let Some(cache_end) = end
        .checked_add(crate::arch::MMArch::PAGE_SIZE as u64 - 1)
        .map(|value| value & page_mask)
    else {
        return true;
    };
    mappings.iter().any(|mapping| {
        if mapping.bar != bar {
            return false;
        }
        let Some(mapping_end) = mapping.offset.checked_add(mapping.length) else {
            return true;
        };
        let mapping_start = mapping.offset & page_mask;
        let mapping_end =
            mapping_end.saturating_add(crate::arch::MMArch::PAGE_SIZE as u64 - 1) & page_mask;
        cache_start < mapping_end && mapping_start < cache_end
    })
}

fn validate_shared_memory_range(
    bar_address: u64,
    bar_size: u64,
    bar: u8,
    offset: u64,
    length: u64,
) -> Option<VirtioSharedMemoryRegion> {
    if bar_address == 0 {
        return None;
    }

    let end = offset.checked_add(length)?;
    if end > bar_size {
        return None;
    }

    let bar_address = usize::try_from(bar_address).ok()?;
    let offset = usize::try_from(offset).ok()?;
    let physical_base = PciArch::address_pci_to_physical(PciAddr::new(bar_address));
    let physical_address = physical_base.data().checked_add(offset)?;
    Some(VirtioSharedMemoryRegion::new(
        PhysAddr::new(physical_address),
        length,
        bar,
        offset as u64,
    ))
}

#[cfg(test)]
mod shared_memory_tests {
    use super::{
        shared_memory_cap_len_supported, shared_memory_overlaps_mappings,
        validate_shared_memory_range,
    };
    use crate::driver::pci::pci::PciBarMappingRequest;

    #[test]
    fn accepts_padded_shared_memory_capability() {
        assert!(!shared_memory_cap_len_supported(23));
        assert!(shared_memory_cap_len_supported(24));
        assert!(shared_memory_cap_len_supported(28));
    }

    #[test]
    fn validates_shared_memory_range() {
        let region = validate_shared_memory_range(0x1000, 0x4000, 2, 0x1000, 0x2000).unwrap();
        assert_eq!(region.physical_address().data(), 0x2000);
        assert_eq!(region.length(), 0x2000);
    }

    #[test]
    fn rejects_unallocated_shared_memory_bar() {
        assert!(validate_shared_memory_range(0, 0x4000, 2, 0x1000, 0x2000).is_none());
    }

    #[test]
    fn preserves_zero_length_region() {
        let region = validate_shared_memory_range(0x1000, 0x4000, 2, 0x4000, 0).unwrap();
        assert_eq!(region.physical_address().data(), 0x5000);
        assert_eq!(region.length(), 0);
    }

    #[test]
    fn rejects_range_end_overflow() {
        assert!(validate_shared_memory_range(0x1000, u64::MAX, 2, u64::MAX, 1).is_none());
    }

    #[test]
    fn rejects_range_outside_bar() {
        assert!(validate_shared_memory_range(0x1000, 0x4000, 2, 0x3000, 0x1001).is_none());
    }

    #[test]
    fn rejects_shared_page_with_transport_mapping() {
        let mappings = [PciBarMappingRequest {
            bar: 2,
            offset: 0x80,
            length: 0x100,
        }];
        assert!(shared_memory_overlaps_mappings(2, 0x800, 0x800, &mappings));
    }

    #[test]
    fn accepts_mapping_on_adjacent_page() {
        let mappings = [PciBarMappingRequest {
            bar: 2,
            offset: 0,
            length: 0x1000,
        }];
        assert!(!shared_memory_overlaps_mappings(
            2, 0x1000, 0x1000, &mappings
        ));
    }

    #[test]
    fn ignores_mapping_in_different_bar() {
        let mappings = [PciBarMappingRequest {
            bar: 4,
            offset: 0,
            length: 0x2000,
        }];
        assert!(!shared_memory_overlaps_mappings(2, 0, 0x2000, &mappings));
    }
}
