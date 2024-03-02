//! PCI transport for VirtIO.
use crate::arch::{PciArch, TraitPciArch};
use crate::driver::base::device::DeviceId;
use crate::driver::pci::pci::{
    BusDeviceFunction, PciDeviceStructure, PciDeviceStructureGeneralDevice, PciError,
    PciStandardDeviceBar, PCI_CAP_ID_VNDR,
};

use crate::driver::pci::pci_irq::{IrqCommonMsg, IrqSpecificMsg, PciInterrupt, PciIrqMsg, IRQ};
use crate::driver::virtio::irq::virtio_irq_manager;
use crate::exception::irqdata::IrqHandlerData;
use crate::exception::irqdesc::{IrqHandler, IrqReturn};

use crate::exception::IrqNumber;

use crate::libs::volatile::{
    volread, volwrite, ReadOnly, Volatile, VolatileReadable, VolatileWritable, WriteOnly,
};
use crate::mm::VirtAddr;

use alloc::string::ToString;
use alloc::sync::Arc;
use core::{
    fmt::{self, Display, Formatter},
    mem::{align_of, size_of},
    ptr::{self, addr_of_mut, NonNull},
};
use system_error::SystemError;
use virtio_drivers::{
    transport::{DeviceStatus, DeviceType, Transport},
    Error, Hal, PhysAddr,
};

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

/// Virtio设备接收中断的设备号
const VIRTIO_RECV_VECTOR: IrqNumber = IrqNumber::new(56);
/// Virtio设备接收中断的设备号的表项号
const VIRTIO_RECV_VECTOR_INDEX: u16 = 0;
// 接收的queue号
const QUEUE_RECEIVE: u16 = 0;
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
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PciTransport {
    device_type: DeviceType,
    /// The bus, device and function identifier for the VirtIO device.
    _bus_device_function: BusDeviceFunction,
    /// The common configuration structure within some BAR.
    common_cfg: NonNull<CommonCfg>,
    /// The start of the queue notification region within some BAR.
    notify_region: NonNull<[WriteOnly<u16>]>,
    notify_off_multiplier: u32,
    /// The ISR status register within some BAR.
    isr_status: NonNull<Volatile<u8>>,
    /// The VirtIO device-specific configuration within some BAR.
    config_space: Option<NonNull<[u32]>>,
    irq: IrqNumber,
    dev_id: Arc<DeviceId>,
}

impl PciTransport {
    /// Construct a new PCI VirtIO device driver for the given device function on the given PCI
    /// root controller.
    ///
    /// ## 参数
    ///
    /// - `device` - The PCI device structure for the VirtIO device.
    /// - `irq_handler` - An optional handler for the device's interrupt. If `None`, a default
    ///     handler `DefaultVirtioIrqHandler` will be used.
    pub fn new<H: Hal>(
        device: &mut PciDeviceStructureGeneralDevice,
        dev_id: Arc<DeviceId>,
    ) -> Result<Self, VirtioPciError> {
        let irq = VIRTIO_RECV_VECTOR;
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
        device.bar_ioremap().unwrap()?;
        device.enable_master();
        let standard_device = device.as_standard_device_mut().unwrap();
        // 目前缺少对PCI设备中断号的统一管理，所以这里需要指定一个中断号。不能与其他中断重复
        let irq_vector = standard_device.irq_vector_mut().unwrap();
        irq_vector.push(irq);
        standard_device
            .irq_init(IRQ::PCI_IRQ_MSIX)
            .expect("IRQ init failed");
        // 中断相关信息
        let msg = PciIrqMsg {
            irq_common_message: IrqCommonMsg::init_from(
                0,
                "Virtio_IRQ".to_string(),
                &DefaultVirtioIrqHandler,
                dev_id.clone(),
            ),
            irq_specific_message: IrqSpecificMsg::msi_default(),
        };
        standard_device.irq_install(msg)?;
        standard_device.irq_enable(true)?;
        //device_capability为迭代器，遍历其相当于遍历所有的cap空间
        for capability in device.capabilities().unwrap() {
            if capability.id != PCI_CAP_ID_VNDR {
                continue;
            }
            let cap_len = capability.private_header as u8;
            let cfg_type = (capability.private_header >> 8) as u8;
            if cap_len < 16 {
                continue;
            }
            let struct_info = VirtioCapabilityInfo {
                bar: PciArch::read_config(&bus_device_function, capability.offset + CAP_BAR_OFFSET)
                    as u8,
                offset: PciArch::read_config(
                    &bus_device_function,
                    capability.offset + CAP_BAR_OFFSET_OFFSET,
                ),
                length: PciArch::read_config(
                    &bus_device_function,
                    capability.offset + CAP_LENGTH_OFFSET,
                ),
            };

            match cfg_type {
                VIRTIO_PCI_CAP_COMMON_CFG if common_cfg.is_none() => {
                    common_cfg = Some(struct_info);
                }
                VIRTIO_PCI_CAP_NOTIFY_CFG if cap_len >= 20 && notify_cfg.is_none() => {
                    notify_cfg = Some(struct_info);
                    notify_off_multiplier = PciArch::read_config(
                        &bus_device_function,
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

        let common_cfg = get_bar_region::<_>(
            &device.standard_device_bar,
            &common_cfg.ok_or(VirtioPciError::MissingCommonConfig)?,
        )?;

        let notify_cfg = notify_cfg.ok_or(VirtioPciError::MissingNotifyConfig)?;
        if notify_off_multiplier % 2 != 0 {
            return Err(VirtioPciError::InvalidNotifyOffMultiplier(
                notify_off_multiplier,
            ));
        }
        //kdebug!("notify.offset={},notify.length={}",notify_cfg.offset,notify_cfg.length);
        let notify_region = get_bar_region_slice::<_>(&device.standard_device_bar, &notify_cfg)?;
        let isr_status = get_bar_region::<_>(
            &device.standard_device_bar,
            &isr_cfg.ok_or(VirtioPciError::MissingIsrConfig)?,
        )?;
        let config_space = if let Some(device_cfg) = device_cfg {
            Some(get_bar_region_slice::<_>(
                &device.standard_device_bar,
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
            isr_status,
            config_space,
            irq,
            dev_id,
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
        unsafe {
            volwrite!(self.common_cfg, queue_select, queue);
            volwrite!(self.common_cfg, queue_size, size as u16);
            volwrite!(self.common_cfg, queue_desc, descriptors as u64);
            volwrite!(self.common_cfg, queue_driver, driver_area as u64);
            volwrite!(self.common_cfg, queue_device, device_area as u64);
            // 这里设置队列中断对应的中断项
            if queue == QUEUE_RECEIVE {
                volwrite!(self.common_cfg, queue_msix_vector, VIRTIO_RECV_VECTOR_INDEX);
                let vector = volread!(self.common_cfg, queue_msix_vector);
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

        // todo: 调用pci的中断释放函数，并且在virtio_irq_manager里面删除对应的设备的中断
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
/// cfg空间在哪个bar的多少偏移处，长度多少
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
                "Virtual address {:?} was not aligned to a {} byte boundary as expected.",
                vaddr, alignment
            ),
            Self::BarGetVaddrFailed => write!(f, "Get bar virtaddress failed"),
            Self::Pci(pci_error) => pci_error.fmt(f),
        }
    }
}

/// PCI error到VirtioPciError的转换，层层上报
impl From<PciError> for VirtioPciError {
    fn from(error: PciError) -> Self {
        Self::Pci(error)
    }
}

/// @brief 获取虚拟地址并将其转化为对应类型的指针
/// @param device_bar 存储bar信息的结构体 struct_info 存储cfg空间的位置信息
/// @return Result<NonNull<T>, VirtioPciError> 成功则返回对应类型的指针，失败则返回Error
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
    if struct_info.offset + struct_info.length > bar_size
        || size_of::<T>() > struct_info.length as usize
    {
        return Err(VirtioPciError::BarOffsetOutOfRange);
    }
    //kdebug!("Chossed bar ={},used={}",struct_info.bar,struct_info.offset + struct_info.length);
    let vaddr = (bar_info
        .virtual_address()
        .ok_or(VirtioPciError::BarGetVaddrFailed)?)
        + struct_info.offset as usize;
    if vaddr.data() % align_of::<T>() != 0 {
        return Err(VirtioPciError::Misaligned {
            vaddr,
            alignment: align_of::<T>(),
        });
    }
    let vaddr = NonNull::new(vaddr.data() as *mut u8).unwrap();
    Ok(vaddr.cast())
}

/// @brief 获取虚拟地址并将其转化为对应类型的切片的指针
/// @param device_bar 存储bar信息的结构体 struct_info 存储cfg空间的位置信息切片的指针
/// @return Result<NonNull<[T]>, VirtioPciError> 成功则返回对应类型的指针切片，失败则返回Error
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

/// `DefaultVirtioIrqHandler` 是一个默认的virtio设备中断处理程序。
///
/// 当虚拟设备产生中断时，该处理程序会被调用。
///
/// 它首先检查设备ID是否存在，然后尝试查找与设备ID关联的设备。
/// 如果找到设备，它会调用设备的 `handle_irq` 方法来处理中断。
/// 如果没有找到设备，它会记录一条警告并返回 `IrqReturn::NotHandled`，表示中断未被处理。
#[derive(Debug)]
struct DefaultVirtioIrqHandler;

impl IrqHandler for DefaultVirtioIrqHandler {
    fn handle(
        &self,
        irq: IrqNumber,
        _static_data: Option<&dyn IrqHandlerData>,
        dev_id: Option<Arc<dyn IrqHandlerData>>,
    ) -> Result<IrqReturn, SystemError> {
        let dev_id = dev_id.ok_or(SystemError::EINVAL)?;
        let dev_id = dev_id
            .arc_any()
            .downcast::<DeviceId>()
            .map_err(|_| SystemError::EINVAL)?;

        if let Some(dev) = virtio_irq_manager().lookup_device(&dev_id) {
            return dev.handle_irq(irq);
        } else {
            // 未绑定具体设备，因此无法处理中断

            return Ok(IrqReturn::NotHandled);
        }
    }
}
