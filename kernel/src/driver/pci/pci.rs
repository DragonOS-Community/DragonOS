use crate::include::bindings::bindings::{
    initial_mm, mm_map, mm_struct, pci_read_config, pci_write_config, VM_DONTCOPY, VM_IO,
};
use crate::mm::mmio_buddy::MMIO_POOL;
use crate::{kdebug, kerror, kwarn};
use bitflags::bitflags;
use core::{
    convert::TryFrom,
    fmt::{self, Display, Formatter},
};
//Bar0寄存器的offset
const BAR0_OFFSET: u8 = 0x10;
//Status、Command寄存器的offset
const STATUS_COMMAND_OFFSET: u8 = 0x04;
/// ID for vendor-specific PCI capabilities.(Virtio Capabilities)
pub const PCI_CAP_ID_VNDR: u8 = 0x09;

bitflags! {
    /// The status register in PCI configuration space.
    pub struct Status: u16 {
        // Bits 0-2 are reserved.
        /// The state of the device's INTx# signal.
        const INTERRUPT_STATUS = 1 << 3;
        /// The device has a linked list of capabilities.
        const CAPABILITIES_LIST = 1 << 4;
        /// The device is capabile of running at 66 MHz rather than 33 MHz.
        const MHZ_66_CAPABLE = 1 << 5;
        // Bit 6 is reserved.
        /// The device can accept fast back-to-back transactions not from the same agent.
        const FAST_BACK_TO_BACK_CAPABLE = 1 << 7;
        /// The bus agent observed a parity error (if parity error handling is enabled).
        const MASTER_DATA_PARITY_ERROR = 1 << 8;
        // Bits 9-10 are DEVSEL timing.
        /// A target device terminated a transaction with target-abort.
        const SIGNALED_TARGET_ABORT = 1 << 11;
        /// A master device transaction was terminated with target-abort.
        const RECEIVED_TARGET_ABORT = 1 << 12;
        /// A master device transaction was terminated with master-abort.
        const RECEIVED_MASTER_ABORT = 1 << 13;
        /// A device asserts SERR#.
        const SIGNALED_SYSTEM_ERROR = 1 << 14;
        /// The device detects a parity error, even if parity error handling is disabled.
        const DETECTED_PARITY_ERROR = 1 << 15;
    }
}

bitflags! {
    /// The command register in PCI configuration space.
    pub struct CommandRegister: u16 {
        /// The device can respond to I/O Space accesses.
        const IO_SPACE = 1 << 0;
        /// The device can respond to Memory Space accesses.
        const MEMORY_SPACE = 1 << 1;
        /// The device can behave as a bus master.
        const BUS_MASTER = 1 << 2;
        /// The device can monitor Special Cycle operations.
        const SPECIAL_CYCLES = 1 << 3;
        /// The device can generate the Memory Write and Invalidate command.
        const MEMORY_WRITE_AND_INVALIDATE_ENABLE = 1 << 4;
        /// The device will snoop palette register data.
        const VGA_PALETTE_SNOOP = 1 << 5;
        /// The device should take its normal action when a parity error is detected.
        const PARITY_ERROR_RESPONSE = 1 << 6;
        // Bit 7 is reserved.
        /// The SERR# driver is enabled.
        const SERR_ENABLE = 1 << 8;
        /// The device is allowed to generate fast back-to-back transactions.
        const FAST_BACK_TO_BACK_ENABLE = 1 << 9;
        /// Assertion of the device's INTx# signal is disabled.
        const INTERRUPT_DISABLE = 1 << 10;
    }
}

/// Gets the capabilities 'pointer' for the device function, if any.
///@brief 获取第一个capability 的offset
///@param device_function PCI设备的唯一标识
///@return Option<u8> offset
pub fn capabilities_offset(device_function: DeviceFunction) -> Option<u8> {
    let status: Status = unsafe {
        let temp = pci_read_config(
            device_function.bus,
            device_function.device,
            device_function.function,
            STATUS_COMMAND_OFFSET,
        );
        Status::from_bits_truncate((temp >> 16) as u16)
    };
    if status.contains(Status::CAPABILITIES_LIST) {
        let cap_pointer = unsafe {
            let temp = pci_read_config(
                device_function.bus,
                device_function.device,
                device_function.function,
                0x34,
            );
            temp as u8 & 0xFC
        };
        Some(cap_pointer)
    } else {
        None
    }
}
/// An identifier for a PCI bus, device and function.
/// PCI设备的唯一标识
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DeviceFunction {
    /// The PCI bus number, between 0 and 255.
    pub bus: u8,
    /// The device number on the bus, between 0 and 31.
    pub device: u8,
    /// The function number of the device, between 0 and 7.
    pub function: u8,
}
///PCI的Error
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PciError {
    /// The device reported an invalid BAR type.
    InvalidBarType,
    CreateMmioError,
}
///实现PciError的Display trait，使其可以直接输出
impl Display for PciError {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            Self::InvalidBarType => write!(f, "Invalid PCI BAR type."),
            Self::CreateMmioError => write!(f, "Error occurred while creating mmio"),
        }
    }
}

impl DeviceFunction {
    /// Returns whether the device and function numbers are valid, i.e. the device is between 0 and
    /// 31, and the function is between 0 and 7.
    /// @brief 检测DeviceFunction实例是否有效
    /// @param self
    /// @return bool 是否有效
    #[allow(dead_code)]
    pub fn valid(&self) -> bool {
        self.device < 32 && self.function < 8
    }
}
///实现DeviceFunction的Display trait，使其可以直接输出
impl Display for DeviceFunction {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "{:02x}:{:02x}.{}", self.bus, self.device, self.function)
    }
}
/// The location allowed for a memory BAR.
/// memory BAR的三种情况
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MemoryBarType {
    /// The BAR has a 32-bit address and can be mapped anywhere in 32-bit address space.
    Width32,
    /// The BAR must be mapped below 1MiB.
    Below1MiB,
    /// The BAR has a 64-bit address and can be mapped anywhere in 64-bit address space.
    Width64,
}
///实现MemoryBarType与u8的类型转换
impl From<MemoryBarType> for u8 {
    fn from(bar_type: MemoryBarType) -> Self {
        match bar_type {
            MemoryBarType::Width32 => 0,
            MemoryBarType::Below1MiB => 1,
            MemoryBarType::Width64 => 2,
        }
    }
}
///实现MemoryBarType与u8的类型转换
impl TryFrom<u8> for MemoryBarType {
    type Error = PciError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Width32),
            1 => Ok(Self::Below1MiB),
            2 => Ok(Self::Width64),
            _ => Err(PciError::InvalidBarType),
        }
    }
}

/// Information about a PCI Base Address Register.
/// BAR的三种类型 Memory/IO/Unused
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BarInfo {
    /// The BAR is for a memory region.
    Memory {
        /// The size of the BAR address and where it can be located.
        address_type: MemoryBarType,
        /// If true, then reading from the region doesn't have side effects. The CPU may cache reads
        /// and merge repeated stores.
        prefetchable: bool,
        /// The memory address, always 16-byte aligned.
        address: u64,
        /// The size of the BAR in bytes.
        size: u32,
        /// The virtaddress for a memory bar(mapped).
        virtaddress: u64,
    },
    /// The BAR is for an I/O region.
    IO {
        /// The I/O address, always 4-byte aligned.
        address: u32,
        /// The size of the BAR in bytes.
        size: u32,
    },
    Unused,
}

impl BarInfo {
    /// Returns the address and size of this BAR if it is a memory bar, or `None` if it is an IO
    /// BAR.
    ///@brief 得到某个bar的memory_address与size(前提是他的类型为Memory Bar)
    ///@param self
    ///@return Option<(u64, u32) 是Memory Bar返回内存地址与大小，不是则返回None
    pub fn memory_address_size(&self) -> Option<(u64, u32)> {
        if let Self::Memory { address, size, .. } = self {
            Some((*address, *size))
        } else {
            None
        }
    }
    ///@brief 得到某个bar的virtaddress(前提是他的类型为Memory Bar)
    ///@param self
    ///@return Option<(u64) 是Memory Bar返回映射的虚拟地址，不是则返回None
    pub fn virtual_address(&self) -> Option<u64> {
        if let Self::Memory { virtaddress, .. } = self {
            Some(*virtaddress)
        } else {
            None
        }
    }
}
///实现BarInfo的Display trait，使其可以直接输出
impl Display for BarInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Memory {
                address_type,
                prefetchable,
                address,
                size,
                virtaddress,
            } => write!(
                f,
                "Memory space at {:#010x}, size {}, type {:?}, prefetchable {},mapped at {:#x}",
                address, size, address_type, prefetchable, virtaddress
            ),
            Self::IO { address, size } => {
                write!(f, "I/O space at {:#010x}, size {}", address, size)
            }
            Self::Unused => {
                write!(f, "Unused bar")
            }
        }
    }
}
///一个PCI设备有6个BAR寄存器，PciDeviceBar存储其全部信息
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PciDeviceBar {
    bar0: BarInfo,
    bar1: BarInfo,
    bar2: BarInfo,
    bar3: BarInfo,
    bar4: BarInfo,
    bar5: BarInfo,
}

impl PciDeviceBar {
    ///@brief 得到某个bar的barinfo
    ///@param self ，bar_index(0-5)
    ///@return Result<&BarInfo, PciError> bar_index在0-5则返回对应的bar_info结构体，超出范围则返回错误
    pub fn get_bar(&self, bar_index: u8) -> Result<&BarInfo, PciError> {
        match bar_index {
            0 => Ok(&self.bar0),
            1 => Ok(&self.bar1),
            2 => Ok(&self.bar2),
            3 => Ok(&self.bar3),
            4 => Ok(&self.bar4),
            _ => Err(PciError::InvalidBarType),
        }
    }
}
///实现PciDeviceBar的Display trait，使其可以直接输出
impl Display for PciDeviceBar {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "\r\nBar0:{}\r\n Bar1:{}\r\n Bar2:{}\r\n Bar3:{}\r\nBar4:{}\r\nBar5:{}",
            self.bar0, self.bar1, self.bar2, self.bar3, self.bar4, self.bar5
        )
    }
}
///实现PciDeviceBar的Default trait，使其可以简单初始化
impl Default for PciDeviceBar {
    fn default() -> Self {
        PciDeviceBar {
            bar0: BarInfo::Unused,
            bar1: BarInfo::Unused,
            bar2: BarInfo::Unused,
            bar3: BarInfo::Unused,
            bar4: BarInfo::Unused,
            bar5: BarInfo::Unused,
        }
    }
}

///@brief 将某个pci设备的bar全部初始化，memory
///@param self ，device_function PCI设备的唯一标识符
///@return Result<PciDeviceBar, PciError> 成功则返回对应的PciDeviceBar结构体，失败则返回错误类型
pub fn pci_bar_init(device_function: DeviceFunction) -> Result<PciDeviceBar, PciError> {
    let mut device_bar: PciDeviceBar = PciDeviceBar::default();
    let mut bar_index_ignore: u8 = 255;
    for bar_index in 0..6 {
        if bar_index == bar_index_ignore {
            continue;
        }
        let bar_info;
        let mut virtaddress: u64 = 0;
        let bar_orig = unsafe {
            let bar_temp = pci_read_config(
                device_function.bus,
                device_function.device,
                device_function.function,
                BAR0_OFFSET + 4 * bar_index,
            );
            bar_temp
        };
        unsafe {
            pci_write_config(
                device_function.bus,
                device_function.device,
                device_function.function,
                BAR0_OFFSET + 4 * bar_index,
                0xffffffff,
            );
        }
        let size_mask = unsafe {
            let bar_temp = pci_read_config(
                device_function.bus,
                device_function.device,
                device_function.function,
                BAR0_OFFSET + 4 * bar_index,
            );
            bar_temp
        };
        // A wrapping add is necessary to correctly handle the case of unused BARs, which read back
        // as 0, and should be treated as size 0.
        let size = (!(size_mask & 0xfffffff0)).wrapping_add(1);
        //kdebug!("bar_orig:{:#x},size: {:#x}", bar_orig,size);
        // Restore the original value.
        unsafe {
            pci_write_config(
                device_function.bus,
                device_function.device,
                device_function.function,
                BAR0_OFFSET + 4 * bar_index,
                bar_orig,
            );
        }
        if size == 0 {
            continue;
        }
        if bar_orig & 0x00000001 == 0x00000001 {
            // I/O space
            let address = bar_orig & 0xfffffffc;
            bar_info = BarInfo::IO { address, size };
        } else {
            // Memory space
            let mut address = u64::from(bar_orig & 0xfffffff0);
            let prefetchable = bar_orig & 0x00000008 != 0;
            let address_type = MemoryBarType::try_from(((bar_orig & 0x00000006) >> 1) as u8)?;
            if address_type == MemoryBarType::Width64 {
                if bar_index >= 5 {
                    return Err(PciError::InvalidBarType);
                }
                let address_top = unsafe {
                    let bar_temp = pci_read_config(
                        device_function.bus,
                        device_function.device,
                        device_function.function,
                        BAR0_OFFSET + 4 * (bar_index + 1),
                    );
                    bar_temp
                };
                address |= u64::from(address_top) << 32;
                bar_index_ignore = bar_index + 1; //下个bar跳过，因为64位的memory bar覆盖了两个bar
            }
            //kdebug!("address={:#x},size={:#x}",address,size);
            unsafe {
                let vaddr_ptr = &mut virtaddress as *mut u64;
                let mut virtsize: u64 = 0;
                let virtsize_ptr = &mut virtsize as *mut u64;
                let initial_mm_ptr = &mut initial_mm as *mut mm_struct;
                //kdebug!("size want={:#x}", size);
                if let Err(_) = MMIO_POOL.create_mmio(
                    size,
                    (VM_IO | VM_DONTCOPY) as u64,
                    vaddr_ptr,
                    virtsize_ptr,
                ) {
                    kerror!("Create mmio failed when initing pci bar");
                    return Err(PciError::CreateMmioError);
                };
                //kdebug!("virtaddress={:#x},virtsize={:#x}",virtaddress,virtsize);
                mm_map(initial_mm_ptr, virtaddress, size as u64, address);
            }
            bar_info = BarInfo::Memory {
                address_type,
                prefetchable,
                address,
                size,
                virtaddress,
            };
        }
        match bar_index {
            0 => {
                device_bar.bar0 = bar_info;
            }
            1 => {
                device_bar.bar1 = bar_info;
            }
            2 => {
                device_bar.bar2 = bar_info;
            }
            3 => {
                device_bar.bar3 = bar_info;
            }
            4 => {
                device_bar.bar4 = bar_info;
            }
            5 => {
                device_bar.bar5 = bar_info;
            }
            _ => {}
        }
    }
    kdebug!("pci_device_bar:{}", device_bar);
    return Ok(device_bar);
}

/// Information about a PCI device capability.
/// PCI设备的capability的信息
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct CapabilityInfo {
    /// The offset of the capability in the PCI configuration space of the device function.
    pub offset: u8,
    /// The ID of the capability.
    pub id: u8,
    /// The third and fourth bytes of the capability, to save reading them again.
    pub private_header: u16,
}

/// Iterator over capabilities for a device.
/// 创建迭代器以遍历PCI设备的capability
#[derive(Debug)]
pub struct CapabilityIterator {
    pub device_function: DeviceFunction,
    pub next_capability_offset: Option<u8>,
}

impl Iterator for CapabilityIterator {
    type Item = CapabilityInfo;
    fn next(&mut self) -> Option<Self::Item> {
        let offset = self.next_capability_offset?;

        // Read the first 4 bytes of the capability.
        let capability_header = unsafe {
            let temp = pci_read_config(
                self.device_function.bus,
                self.device_function.device,
                self.device_function.function,
                offset,
            );
            temp
        };
        let id = capability_header as u8;
        let next_offset = (capability_header >> 8) as u8;
        let private_header = (capability_header >> 16) as u16;

        self.next_capability_offset = if next_offset == 0 {
            None
        } else if next_offset < 64 || next_offset & 0x3 != 0 {
            kwarn!("Invalid next capability offset {:#04x}", next_offset);
            None
        } else {
            Some(next_offset)
        };

        Some(CapabilityInfo {
            offset,
            id,
            private_header,
        })
    }
}

/// @brief 设置PCI Config Space里面的Command Register
///
/// @param device_function 设备
/// @param value command register要被设置成的值
pub fn set_command_register(device_function: &DeviceFunction, value: CommandRegister) {
    unsafe {
        pci_write_config(
            device_function.bus,
            device_function.device,
            device_function.function,
            STATUS_COMMAND_OFFSET,
            value.bits().into(),
        );
    }
}
/// @brief 使能对PCI Memory/IO空间的写入，使能PCI设备作为主设备(主动进行Memory的写入等，msix中断使用到)
///
/// @param device_function 设备
pub fn pci_enable_master(device_function: DeviceFunction) {
    set_command_register(
        &device_function,
        CommandRegister::IO_SPACE | CommandRegister::MEMORY_SPACE | CommandRegister::BUS_MASTER,
    );
}
