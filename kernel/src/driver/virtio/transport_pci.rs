//! PCI transport for VirtIO.
use crate::driver::pci::pci::{
    capabilities_offset, pci_bar_init, pci_enable_master, CapabilityIterator, DeviceFunction,
    PciDeviceBar, PciError, PCI_CAP_ID_VNDR,
};
use crate::include::bindings::bindings::pci_read_config;

use crate::libs::volatile::{
    volread, volwrite, ReadOnly, Volatile, VolatileReadable, VolatileWritable, WriteOnly,
};
use core::{
    fmt::{self, Display, Formatter},
    mem::{align_of, size_of},
    ptr::{self, addr_of_mut, NonNull},
};
use virtio_drivers::{
    transport::{DeviceStatus, DeviceType, Transport},
    Error, Hal, PhysAddr,
};

type VirtAddr = usize;
/// The PCI vendor ID for VirtIO devices.
/// PCI Virtio设备的vendor ID
const VIRTIO_VENDOR_ID: u16 = 0x1af4;

/// The offset to add to a VirtIO device ID to get the corresponding PCI device ID.
/// PCI Virtio设备的DEVICE_ID 的offset
const PCI_DEVICE_ID_OFFSET: u16 = 0x1040;
/// PCI Virtio 设备的DEVICE_ID及其对应的设备类型
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

///@brief device id 转换为设备类型
///@param pci_device_id，device_id
///@return DeviceType 对应的设备类型
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
    pub fn new<H: Hal>(device_function: DeviceFunction) -> Result<Self, VirtioPciError> {
        let device_vendor = unsafe {
            let bar_temp = pci_read_config(
                device_function.bus,
                device_function.device,
                device_function.function,
                0,
            );
            bar_temp
        };
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
        //device_capability为迭代器，遍历其相当于遍历所有的cap空间
        let device_capability = CapabilityIterator {
            device_function: device_function,
            next_capability_offset: capabilities_offset(device_function),
        };

        let device_bar = pci_bar_init(device_function)?;
        pci_enable_master(device_function);
        for capability in device_capability {
            if capability.id != PCI_CAP_ID_VNDR {
                continue;
            }
            let cap_len = capability.private_header as u8;
            let cfg_type = (capability.private_header >> 8) as u8;
            if cap_len < 16 {
                continue;
            }
            let struct_info = VirtioCapabilityInfo {
                bar: unsafe {
                    let temp = pci_read_config(
                        device_function.bus,
                        device_function.device,
                        device_function.function,
                        capability.offset + CAP_BAR_OFFSET,
                    );
                    temp as u8
                },
                offset: unsafe {
                    let temp = pci_read_config(
                        device_function.bus,
                        device_function.device,
                        device_function.function,
                        capability.offset + CAP_BAR_OFFSET_OFFSET,
                    );
                    temp
                },
                length: unsafe {
                    let temp = pci_read_config(
                        device_function.bus,
                        device_function.device,
                        device_function.function,
                        capability.offset + CAP_LENGTH_OFFSET,
                    );
                    temp
                },
            };

            match cfg_type {
                VIRTIO_PCI_CAP_COMMON_CFG if common_cfg.is_none() => {
                    common_cfg = Some(struct_info);
                }
                VIRTIO_PCI_CAP_NOTIFY_CFG if cap_len >= 20 && notify_cfg.is_none() => {
                    notify_cfg = Some(struct_info);
                    notify_off_multiplier = unsafe {
                        let temp = pci_read_config(
                            device_function.bus,
                            device_function.device,
                            device_function.function,
                            capability.offset + CAP_NOTIFY_OFF_MULTIPLIER_OFFSET,
                        );
                        temp
                    };
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

        let common_cfg = get_bar_region::<_>(
            &device_bar,
            &common_cfg.ok_or(VirtioPciError::MissingCommonConfig)?,
        )?;

        let notify_cfg = notify_cfg.ok_or(VirtioPciError::MissingNotifyConfig)?;
        if notify_off_multiplier % 2 != 0 {
            return Err(VirtioPciError::InvalidNotifyOffMultiplier(
                notify_off_multiplier,
            ));
        }
        //kdebug!("notify.offset={},notify.length={}",notify_cfg.offset,notify_cfg.length);
        let notify_region = get_bar_region_slice::<_>(&device_bar, &notify_cfg)?;
        let isr_status = get_bar_region::<_>(
            &device_bar,
            &isr_cfg.ok_or(VirtioPciError::MissingIsrConfig)?,
        )?;
        let config_space = if let Some(device_cfg) = device_cfg {
            Some(get_bar_region_slice::<_>(&device_bar, &device_cfg)?)
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

    fn max_queue_size(&self) -> u32 {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        unsafe { volread!(self.common_cfg, queue_size) }.into()
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
        // kdebug!("queue_select={}",queue);
        // kdebug!("queue_size={}",size as u16);
        // kdebug!("queue_desc={:#x}",descriptors as u64);
        // kdebug!("driver_area={:#x}",driver_area);
        unsafe {
            volwrite!(self.common_cfg, queue_select, queue);
            volwrite!(self.common_cfg, queue_size, size as u16);
            volwrite!(self.common_cfg, queue_desc, descriptors as u64);
            volwrite!(self.common_cfg, queue_driver, driver_area as u64);
            volwrite!(self.common_cfg, queue_device, device_area as u64);
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
        self.set_status(DeviceStatus::empty())
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
///cfg空间在哪个bar的多少偏移处，长度多少
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
/// VirtIO PCI transport 初始化时的错误
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
    ///获取虚拟地址失败
    BarGetVaddrFailed,
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
            Self::UnexpectedBarType => write!(f, "Unexpected BAR (expected memory BAR)."),
            Self::BarNotAllocated(bar_index) => write!(f, "Bar {} not allocated.", bar_index),
            Self::BarOffsetOutOfRange => write!(f, "Capability offset greater than BAR length."),
            Self::Misaligned { vaddr, alignment } => write!(
                f,
                "Virtual address {:#018x} was not aligned to a {} byte boundary as expected.",
                vaddr, alignment
            ),
            Self::BarGetVaddrFailed => write!(f, "Get bar virtaddress failed"),
            Self::Pci(pci_error) => pci_error.fmt(f),
        }
    }
}
///PCI error到VirtioPciError的转换，层层上报
impl From<PciError> for VirtioPciError {
    fn from(error: PciError) -> Self {
        Self::Pci(error)
    }
}

///@brief 获取虚拟地址并将其转化为对应类型的指针
///@param device_bar 存储bar信息的结构体 struct_info 存储cfg空间的位置信息
///@return Result<NonNull<T>, VirtioPciError> 成功则返回对应类型的指针，失败则返回Error
fn get_bar_region<T>(
    device_bar: &PciDeviceBar,
    struct_info: &VirtioCapabilityInfo,
) -> Result<NonNull<T>, VirtioPciError> {
    let bar_info = device_bar.get_bar(struct_info.bar)?;
    let (bar_address, bar_size) = bar_info
        .memory_address_size()
        .ok_or(VirtioPciError::UnexpectedBarType)?;
    if bar_address == 0 {
        return Err(VirtioPciError::BarNotAllocated(struct_info.bar));
    }
    if struct_info.offset + struct_info.length > bar_size
        || size_of::<T>() > struct_info.length as usize
    {
        return Err(VirtioPciError::BarOffsetOutOfRange);
    }
    //kdebug!("Chossed bar ={},used={}",struct_info.bar,struct_info.offset + struct_info.length);
    let vaddr = (bar_info
        .virtual_address()
        .ok_or(VirtioPciError::BarGetVaddrFailed)?) as usize
        + struct_info.offset as usize;
    if vaddr % align_of::<T>() != 0 {
        return Err(VirtioPciError::Misaligned {
            vaddr,
            alignment: align_of::<T>(),
        });
    }
    let vaddr = NonNull::new(vaddr as *mut u8).unwrap();
    Ok(vaddr.cast())
}

///@brief 获取虚拟地址并将其转化为对应类型的
///@param device_bar 存储bar信息的结构体 struct_info 存储cfg空间的位置信息切片的指针
///@return Result<NonNull<[T]>, VirtioPciError> 成功则返回对应类型的指针切片，失败则返回Error
fn get_bar_region_slice<T>(
    device_bar: &PciDeviceBar,
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
