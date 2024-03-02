#![allow(dead_code)]
// 目前仅支持单主桥单Segment

use super::pci_irq::{IrqType, PciIrqError};
use crate::arch::{PciArch, TraitPciArch};
use crate::exception::IrqNumber;
use crate::include::bindings::bindings::PAGE_2M_SIZE;
use crate::libs::rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::mm::mmio_buddy::{mmio_pool, MMIOSpaceGuard};

use crate::mm::{PhysAddr, VirtAddr};
use crate::{kdebug, kerror, kinfo, kwarn};
use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::{boxed::Box, collections::LinkedList};
use bitflags::bitflags;

use core::{
    convert::TryFrom,
    fmt::{self, Debug, Display, Formatter},
};
// PCI_DEVICE_LINKEDLIST 添加了读写锁的全局链表，里面存储了检索到的PCI设备结构体
// PCI_ROOT_0 Segment为0的全局PciRoot
lazy_static! {
    pub static ref PCI_DEVICE_LINKEDLIST: PciDeviceLinkedList = PciDeviceLinkedList::new();
    pub static ref PCI_ROOT_0: Option<PciRoot> = {
        match PciRoot::new(0) {
            Ok(root) => Some(root),
            Err(err) => {
                kerror!("Pci_root init failed because of error: {}", err);
                None
            }
        }
    };
}
/// PCI域地址
#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct PciAddr(usize);

impl PciAddr {
    #[inline(always)]
    pub const fn new(address: usize) -> Self {
        Self(address)
    }

    /// @brief 获取PCI域地址的值
    #[inline(always)]
    pub fn data(&self) -> usize {
        self.0
    }

    /// @brief 将PCI域地址加上一个偏移量
    #[inline(always)]
    pub fn add(self, offset: usize) -> Self {
        Self(self.0 + offset)
    }

    /// @brief 判断PCI域地址是否按照指定要求对齐
    #[inline(always)]
    pub fn check_aligned(&self, align: usize) -> bool {
        return self.0 & (align - 1) == 0;
    }
}
impl Debug for PciAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "PciAddr({:#x})", self.0)
    }
}

/// 添加了读写锁的链表，存储PCI设备结构体
pub struct PciDeviceLinkedList {
    list: RwLock<LinkedList<Box<dyn PciDeviceStructure>>>,
}

impl PciDeviceLinkedList {
    /// @brief 初始化结构体
    fn new() -> Self {
        PciDeviceLinkedList {
            list: RwLock::new(LinkedList::new()),
        }
    }
    /// @brief 获取可读的linkedlist(读锁守卫)
    /// @return RwLockReadGuard<LinkedList<Box<dyn PciDeviceStructure>>>  读锁守卫
    pub fn read(&self) -> RwLockReadGuard<LinkedList<Box<dyn PciDeviceStructure>>> {
        self.list.read()
    }
    /// @brief 获取可写的linkedlist(写锁守卫)
    /// @return RwLockWriteGuard<LinkedList<Box<dyn PciDeviceStructure>>>  写锁守卫
    pub fn write(&self) -> RwLockWriteGuard<LinkedList<Box<dyn PciDeviceStructure>>> {
        self.list.write()
    }
    /// @brief 获取链表中PCI结构体数目
    /// @return usize 链表中PCI结构体数目
    pub fn num(&self) -> usize {
        let list = self.list.read();
        list.len()
    }
    /// @brief 添加Pci设备结构体到链表中
    pub fn add(&self, device: Box<dyn PciDeviceStructure>) {
        let mut list = self.list.write();
        list.push_back(device);
    }
}

/// @brief 在链表中寻找满足条件的PCI设备结构体并返回其可变引用
/// @param list 链表的写锁守卫  
/// @param class_code 寄存器值
/// @param subclass 寄存器值，与class_code一起确定设备类型
/// @return Vec<&'a mut Box<(dyn PciDeviceStructure)  包含链表中所有满足条件的PCI结构体的可变引用的容器
pub fn get_pci_device_structure_mut<'a>(
    list: &'a mut RwLockWriteGuard<'_, LinkedList<Box<dyn PciDeviceStructure>>>,
    class_code: u8,
    subclass: u8,
) -> Vec<&'a mut Box<(dyn PciDeviceStructure)>> {
    let mut result = Vec::new();
    for box_pci_device_structure in list.iter_mut() {
        let common_header = (*box_pci_device_structure).common_header();
        if (common_header.class_code == class_code) && (common_header.subclass == subclass) {
            result.push(box_pci_device_structure);
        }
    }
    result
}
/// @brief 在链表中寻找满足条件的PCI设备结构体并返回其不可变引用
/// @param list 链表的读锁守卫  
/// @param class_code 寄存器值
/// @param subclass 寄存器值，与class_code一起确定设备类型
/// @return Vec<&'a Box<(dyn PciDeviceStructure)  包含链表中所有满足条件的PCI结构体的不可变引用的容器
pub fn get_pci_device_structure<'a>(
    list: &'a mut RwLockReadGuard<'_, LinkedList<Box<dyn PciDeviceStructure>>>,
    class_code: u8,
    subclass: u8,
) -> Vec<&'a Box<(dyn PciDeviceStructure)>> {
    let mut result = Vec::new();
    for box_pci_device_structure in list.iter() {
        let common_header = (*box_pci_device_structure).common_header();
        if (common_header.class_code == class_code) && (common_header.subclass == subclass) {
            result.push(box_pci_device_structure);
        }
    }
    result
}

//Bar0寄存器的offset
const BAR0_OFFSET: u8 = 0x10;
//Status、Command寄存器的offset
const STATUS_COMMAND_OFFSET: u8 = 0x04;
/// ID for vendor-specific PCI capabilities.(Virtio Capabilities)
pub const PCI_CAP_ID_VNDR: u8 = 0x09;
pub const PCI_CAP_ID_MSI: u8 = 0x05;
pub const PCI_CAP_ID_MSIX: u8 = 0x11;
pub const PORT_PCI_CONFIG_ADDRESS: u16 = 0xcf8;
pub const PORT_PCI_CONFIG_DATA: u16 = 0xcfc;
// pci设备分组的id
pub type SegmentGroupNumber = u16; //理论上最多支持65535个Segment_Group

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
    pub struct Command: u16 {
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

/// The type of a PCI device function header.
/// 标头类型/设备类型
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HeaderType {
    /// A normal PCI device.
    Standard,
    /// A PCI to PCI bridge.
    PciPciBridge,
    /// A PCI to CardBus bridge.
    PciCardbusBridge,
    /// Unrecognised header type.
    Unrecognised(u8),
}
/// u8到HeaderType的转换
impl From<u8> for HeaderType {
    fn from(value: u8) -> Self {
        match value {
            0x00 => Self::Standard,
            0x01 => Self::PciPciBridge,
            0x02 => Self::PciCardbusBridge,
            _ => Self::Unrecognised(value),
        }
    }
}
/// Pci可能触发的各种错误
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PciError {
    /// The device reported an invalid BAR type.
    InvalidBarType,
    CreateMmioError,
    InvalidBusDeviceFunction,
    SegmentNotFound,
    McfgTableNotFound,
    GetWrongHeader,
    UnrecognisedHeaderType,
    PciDeviceStructureTransformError,
    PciIrqError(PciIrqError),
}
///实现PciError的Display trait，使其可以直接输出
impl Display for PciError {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            Self::InvalidBarType => write!(f, "Invalid PCI BAR type."),
            Self::CreateMmioError => write!(f, "Error occurred while creating mmio."),
            Self::InvalidBusDeviceFunction => write!(f, "Found invalid BusDeviceFunction."),
            Self::SegmentNotFound => write!(f, "Target segment not found"),
            Self::McfgTableNotFound => write!(f, "ACPI MCFG Table not found"),
            Self::GetWrongHeader => write!(f, "GetWrongHeader with vendor id 0xffff"),
            Self::UnrecognisedHeaderType => write!(f, "Found device with unrecognised header type"),
            Self::PciDeviceStructureTransformError => {
                write!(f, "Found None When transform Pci device structure")
            }
            Self::PciIrqError(err) => write!(f, "Error occurred while setting irq :{:?}.", err),
        }
    }
}

/// trait类型Pci_Device_Structure表示pci设备，动态绑定三种具体设备类型：Pci_Device_Structure_General_Device、Pci_Device_Structure_Pci_to_Pci_Bridge、Pci_Device_Structure_Pci_to_Cardbus_Bridge
pub trait PciDeviceStructure: Send + Sync {
    /// @brief 获取设备类型
    /// @return HeaderType 设备类型
    fn header_type(&self) -> HeaderType;
    /// @brief 当其为standard设备时返回&Pci_Device_Structure_General_Device，其余情况返回None
    #[inline(always)]
    fn as_standard_device(&self) -> Option<&PciDeviceStructureGeneralDevice> {
        None
    }
    /// @brief 当其为pci to pci bridge设备时返回&Pci_Device_Structure_Pci_to_Pci_Bridge，其余情况返回None
    #[inline(always)]
    fn as_pci_to_pci_bridge_device(&self) -> Option<&PciDeviceStructurePciToPciBridge> {
        None
    }
    /// @brief 当其为pci to cardbus bridge设备时返回&Pci_Device_Structure_Pci_to_Cardbus_Bridge，其余情况返回None
    #[inline(always)]
    fn as_pci_to_carbus_bridge_device(&self) -> Option<&PciDeviceStructurePciToCardbusBridge> {
        None
    }
    /// @brief 获取Pci设备共有的common_header
    /// @return 返回其不可变引用
    fn common_header(&self) -> &PciDeviceStructureHeader;
    /// @brief 当其为standard设备时返回&mut Pci_Device_Structure_General_Device，其余情况返回None
    #[inline(always)]
    fn as_standard_device_mut(&mut self) -> Option<&mut PciDeviceStructureGeneralDevice> {
        None
    }
    /// @brief 当其为pci to pci bridge设备时返回&mut Pci_Device_Structure_Pci_to_Pci_Bridge，其余情况返回None
    #[inline(always)]
    fn as_pci_to_pci_bridge_device_mut(&mut self) -> Option<&mut PciDeviceStructurePciToPciBridge> {
        None
    }
    /// @brief 当其为pci to cardbus bridge设备时返回&mut Pci_Device_Structure_Pci_to_Cardbus_Bridge，其余情况返回None
    #[inline(always)]
    fn as_pci_to_carbus_bridge_device_mut(
        &mut self,
    ) -> Option<&mut PciDeviceStructurePciToCardbusBridge> {
        None
    }
    /// @brief 返回迭代器，遍历capabilities
    fn capabilities(&self) -> Option<CapabilityIterator> {
        None
    }
    /// @brief 获取Status、Command寄存器的值
    fn status_command(&self) -> (Status, Command) {
        let common_header = self.common_header();
        let status = Status::from_bits_truncate(common_header.status);
        let command = Command::from_bits_truncate(common_header.command);
        (status, command)
    }
    /// @brief 设置Command寄存器的值
    fn set_command(&mut self, command: Command) {
        let common_header = self.common_header_mut();
        let command = command.bits();
        common_header.command = command;
        PciArch::write_config(
            &common_header.bus_device_function,
            STATUS_COMMAND_OFFSET,
            command as u32,
        );
    }
    /// @brief 获取Pci设备共有的common_header
    /// @return 返回其可变引用
    fn common_header_mut(&mut self) -> &mut PciDeviceStructureHeader;

    /// @brief 读取standard设备的bar寄存器，映射后将结果加入结构体的standard_device_bar变量
    /// @return 只有standard设备才返回成功或者错误，其余返回None
    #[inline(always)]
    fn bar_ioremap(&mut self) -> Option<Result<u8, PciError>> {
        None
    }
    /// @brief 获取PCI设备的bar寄存器的引用
    /// @return
    #[inline(always)]
    fn bar(&mut self) -> Option<&PciStandardDeviceBar> {
        None
    }
    /// @brief 通过设置该pci设备的command
    fn enable_master(&mut self) {
        self.set_command(Command::IO_SPACE | Command::MEMORY_SPACE | Command::BUS_MASTER);
    }
    /// @brief 寻找设备的msix空间的offset
    fn msix_capability_offset(&self) -> Option<u8> {
        for capability in self.capabilities()? {
            if capability.id == PCI_CAP_ID_MSIX {
                return Some(capability.offset);
            }
        }
        None
    }
    /// @brief 寻找设备的msi空间的offset
    fn msi_capability_offset(&self) -> Option<u8> {
        for capability in self.capabilities()? {
            if capability.id == PCI_CAP_ID_MSI {
                return Some(capability.offset);
            }
        }
        None
    }
    /// @brief 返回结构体中的irq_type的可变引用
    fn irq_type_mut(&mut self) -> Option<&mut IrqType>;
    /// @brief 返回结构体中的irq_vector的可变引用
    fn irq_vector_mut(&mut self) -> Option<&mut Vec<IrqNumber>>;
}

/// Pci_Device_Structure_Header PCI设备结构体共有的头部
#[derive(Clone, Debug)]
pub struct PciDeviceStructureHeader {
    // ==== busdevicefunction变量表示该结构体所处的位置
    pub bus_device_function: BusDeviceFunction,
    pub vendor_id: u16, // 供应商ID 0xffff是一个无效值，在读取访问不存在的设备的配置空间寄存器时返回
    pub device_id: u16, // 设备ID，标志特定设备
    pub command: u16, // 提供对设备生成和响应pci周期的能力的控制 向该寄存器写入0时，设备与pci总线断开除配置空间访问以外的所有连接
    pub status: u16,  // 用于记录pci总线相关时间的状态信息寄存器
    pub revision_id: u8, // 修订ID，指定特定设备的修订标志符
    pub prog_if: u8, // 编程接口字节，一个只读寄存器，指定设备具有的寄存器级别的编程接口（如果有的话）
    pub subclass: u8, // 子类。指定设备执行的特定功能的只读寄存器
    pub class_code: u8, // 类代码，一个只读寄存器，指定设备执行的功能类型
    pub cache_line_size: u8, // 缓存线大小：以 32 位为单位指定系统缓存线大小。设备可以限制它可以支持的缓存线大小的数量，如果不支持的值写入该字段，设备将表现得好像写入了 0 值
    pub latency_timer: u8,   // 延迟计时器：以 PCI 总线时钟为单位指定延迟计时器。
    pub header_type: u8, // 标头类型 a value of 0x0 specifies a general device, a value of 0x1 specifies a PCI-to-PCI bridge, and a value of 0x2 specifies a CardBus bridge. If bit 7 of this register is set, the device has multiple functions; otherwise, it is a single function device.
    pub bist: u8, // Represents that status and allows control of a devices BIST (built-in self test).
                  // Here is the layout of the BIST register:
                  // |     bit7     |    bit6    | Bits 5-4 |     Bits 3-0    |
                  // | BIST Capable | Start BIST | Reserved | Completion Code |
                  // for more details, please visit https://wiki.osdev.org/PCI
}

/// Pci_Device_Structure_General_Device PCI标准设备结构体
#[derive(Clone, Debug)]
pub struct PciDeviceStructureGeneralDevice {
    pub common_header: PciDeviceStructureHeader,
    // 中断结构体，包括legacy,msi,msix三种情况
    pub irq_type: IrqType,
    // 使用的中断号的vec集合
    pub irq_vector: Vec<IrqNumber>,
    pub standard_device_bar: PciStandardDeviceBar,
    pub cardbus_cis_pointer: u32, // 指向卡信息结构，供在 CardBus 和 PCI 之间共享芯片的设备使用。
    pub subsystem_vendor_id: u16,
    pub subsystem_id: u16,
    pub expansion_rom_base_address: u32,
    pub capabilities_pointer: u8,
    pub reserved0: u8,
    pub reserved1: u16,
    pub reserved2: u32,
    pub interrupt_line: u8, // 指定设备的中断引脚连接到系统中断控制器的哪个输入，并由任何使用中断引脚的设备实现。对于 x86 架构，此寄存器对应于 PIC IRQ 编号 0-15（而不是 I/O APIC IRQ 编号），并且值0xFF定义为无连接。
    pub interrupt_pin: u8, // 指定设备使用的中断引脚。其中值为0x1INTA#、0x2INTB#、0x3INTC#、0x4INTD#，0x0表示设备不使用中断引脚。
    pub min_grant: u8, // 一个只读寄存器，用于指定设备所需的突发周期长度（以 1/4 微秒为单位）（假设时钟速率为 33 MHz）
    pub max_latency: u8, // 一个只读寄存器，指定设备需要多长时间访问一次 PCI 总线（以 1/4 微秒为单位）。
}
impl PciDeviceStructure for PciDeviceStructureGeneralDevice {
    #[inline(always)]
    fn header_type(&self) -> HeaderType {
        HeaderType::Standard
    }
    #[inline(always)]
    fn as_standard_device(&self) -> Option<&PciDeviceStructureGeneralDevice> {
        Some(self)
    }
    #[inline(always)]
    fn as_standard_device_mut(&mut self) -> Option<&mut PciDeviceStructureGeneralDevice> {
        Some(self)
    }
    #[inline(always)]
    fn common_header(&self) -> &PciDeviceStructureHeader {
        &self.common_header
    }
    #[inline(always)]
    fn common_header_mut(&mut self) -> &mut PciDeviceStructureHeader {
        &mut self.common_header
    }
    fn capabilities(&self) -> Option<CapabilityIterator> {
        Some(CapabilityIterator {
            bus_device_function: self.common_header.bus_device_function,
            next_capability_offset: Some(self.capabilities_pointer),
        })
    }
    fn bar_ioremap(&mut self) -> Option<Result<u8, PciError>> {
        let common_header = &self.common_header;
        match pci_bar_init(common_header.bus_device_function) {
            Ok(bar) => {
                self.standard_device_bar = bar;
                Some(Ok(0))
            }
            Err(e) => Some(Err(e)),
        }
    }
    fn bar(&mut self) -> Option<&PciStandardDeviceBar> {
        Some(&self.standard_device_bar)
    }
    #[inline(always)]
    fn irq_type_mut(&mut self) -> Option<&mut IrqType> {
        Some(&mut self.irq_type)
    }
    #[inline(always)]
    fn irq_vector_mut(&mut self) -> Option<&mut Vec<IrqNumber>> {
        Some(&mut self.irq_vector)
    }
}

/// Pci_Device_Structure_Pci_to_Pci_Bridge pci-to-pci桥设备结构体
#[derive(Clone, Debug)]
pub struct PciDeviceStructurePciToPciBridge {
    pub common_header: PciDeviceStructureHeader,
    // 中断结构体，包括legacy,msi,msix三种情况
    pub irq_type: IrqType,
    // 使用的中断号的vec集合
    pub irq_vector: Vec<IrqNumber>,
    pub bar0: u32,
    pub bar1: u32,
    pub primary_bus_number: u8,
    pub secondary_bus_number: u8,
    pub subordinate_bus_number: u8,
    pub secondary_latency_timer: u8,
    pub io_base: u8,
    pub io_limit: u8,
    pub secondary_status: u16,
    pub memory_base: u16,
    pub memory_limit: u16,
    pub prefetchable_memory_base: u16,
    pub prefetchable_memory_limit: u16,
    pub prefetchable_base_upper_32_bits: u32,
    pub prefetchable_limit_upper_32_bits: u32,
    pub io_base_upper_16_bits: u16,
    pub io_limit_upper_16_bits: u16,
    pub capability_pointer: u8,
    pub reserved0: u8,
    pub reserved1: u16,
    pub expansion_rom_base_address: u32,
    pub interrupt_line: u8,
    pub interrupt_pin: u8,
    pub bridge_control: u16,
}
impl PciDeviceStructure for PciDeviceStructurePciToPciBridge {
    #[inline(always)]
    fn header_type(&self) -> HeaderType {
        HeaderType::PciPciBridge
    }
    #[inline(always)]
    fn as_pci_to_pci_bridge_device(&self) -> Option<&PciDeviceStructurePciToPciBridge> {
        Some(self)
    }
    #[inline(always)]
    fn as_pci_to_pci_bridge_device_mut(&mut self) -> Option<&mut PciDeviceStructurePciToPciBridge> {
        Some(self)
    }
    #[inline(always)]
    fn common_header(&self) -> &PciDeviceStructureHeader {
        &self.common_header
    }
    #[inline(always)]
    fn common_header_mut(&mut self) -> &mut PciDeviceStructureHeader {
        &mut self.common_header
    }
    #[inline(always)]
    fn irq_type_mut(&mut self) -> Option<&mut IrqType> {
        Some(&mut self.irq_type)
    }
    #[inline(always)]
    fn irq_vector_mut(&mut self) -> Option<&mut Vec<IrqNumber>> {
        Some(&mut self.irq_vector)
    }
}
/// Pci_Device_Structure_Pci_to_Cardbus_Bridge Pci_to_Cardbus桥设备结构体
#[derive(Clone, Debug)]
pub struct PciDeviceStructurePciToCardbusBridge {
    pub common_header: PciDeviceStructureHeader,
    pub cardbus_socket_ex_ca_base_address: u32,
    pub offset_of_capabilities_list: u8,
    pub reserved: u8,
    pub secondary_status: u16,
    pub pci_bus_number: u8,
    pub card_bus_bus_number: u8,
    pub subordinate_bus_number: u8,
    pub card_bus_latency_timer: u8,
    pub memory_base_address0: u32,
    pub memory_limit0: u32,
    pub memory_base_address1: u32,
    pub memory_limit1: u32,
    pub io_base_address0: u32,
    pub io_limit0: u32,
    pub io_base_address1: u32,
    pub io_limit1: u32,
    pub interrupt_line: u8,
    pub interrupt_pin: u8,
    pub bridge_control: u16,
    pub subsystem_device_id: u16,
    pub subsystem_vendor_id: u16,
    pub pc_card_legacy_mode_base_address_16_bit: u32,
}
impl PciDeviceStructure for PciDeviceStructurePciToCardbusBridge {
    #[inline(always)]
    fn header_type(&self) -> HeaderType {
        HeaderType::PciCardbusBridge
    }
    #[inline(always)]
    fn as_pci_to_carbus_bridge_device(&self) -> Option<&PciDeviceStructurePciToCardbusBridge> {
        Some(&self)
    }
    #[inline(always)]
    fn as_pci_to_carbus_bridge_device_mut(
        &mut self,
    ) -> Option<&mut PciDeviceStructurePciToCardbusBridge> {
        Some(self)
    }
    #[inline(always)]
    fn common_header(&self) -> &PciDeviceStructureHeader {
        &self.common_header
    }
    #[inline(always)]
    fn common_header_mut(&mut self) -> &mut PciDeviceStructureHeader {
        &mut self.common_header
    }
    #[inline(always)]
    fn irq_type_mut(&mut self) -> Option<&mut IrqType> {
        None
    }
    #[inline(always)]
    fn irq_vector_mut(&mut self) -> Option<&mut Vec<IrqNumber>> {
        None
    }
}

/// 代表一个PCI segement greoup.
#[derive(Clone, Debug)]
pub struct PciRoot {
    pub physical_address_base: PhysAddr,         //物理地址，acpi获取
    pub mmio_guard: Option<Arc<MMIOSpaceGuard>>, //映射后的虚拟地址，为方便访问数据这里转化成指针
    pub segement_group_number: SegmentGroupNumber, //segement greoup的id
    pub bus_begin: u8,                           //该分组中的最小bus
    pub bus_end: u8,                             //该分组中的最大bus
}
///线程间共享需要，该结构体只需要在初始化时写入数据，无需读写锁保证线程安全
unsafe impl Send for PciRoot {}
unsafe impl Sync for PciRoot {}
///实现PciRoot的Display trait，自定义输出
impl Display for PciRoot {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
                f,
                "PCI Root with segement:{}, bus begin at {}, bus end at {}, physical address at {:?},mapped at {:?}",
                self.segement_group_number, self.bus_begin, self.bus_end, self.physical_address_base, self.mmio_guard
            )
    }
}

impl PciRoot {
    /// @brief 初始化结构体，获取ecam root所在物理地址后map到虚拟地址，再将该虚拟地址加入mmio_base变量
    /// @return 成功返回结果，错误返回错误类型
    pub fn new(segment_group_number: SegmentGroupNumber) -> Result<Self, PciError> {
        let mut pci_root = PciArch::ecam_root(segment_group_number)?;
        pci_root.map()?;
        Ok(pci_root)
    }
    /// @brief  完成物理地址到虚拟地址的映射，并将虚拟地址加入mmio_base变量
    /// @return 返回错误或Ok(0)
    fn map(&mut self) -> Result<u8, PciError> {
        //kdebug!("bus_begin={},bus_end={}", self.bus_begin,self.bus_end);
        let bus_number = (self.bus_end - self.bus_begin) as u32 + 1;
        let bus_number_double = (bus_number - 1) / 2 + 1; //一个bus占据1MB空间，计算全部bus占据空间相对于2MB空间的个数

        let size = (bus_number_double as usize) * (PAGE_2M_SIZE as usize);
        unsafe {
            let space_guard = mmio_pool()
                .create_mmio(size as usize)
                .map_err(|_| PciError::CreateMmioError)?;
            let space_guard = Arc::new(space_guard);
            self.mmio_guard = Some(space_guard.clone());

            assert!(space_guard
                .map_phys(self.physical_address_base, size)
                .is_ok());
        }
        return Ok(0);
    }
    /// @brief 获得要操作的寄存器相对于mmio_offset的偏移量
    /// @param bus_device_function 在同一个group中pci设备的唯一标识符
    /// @param register_offset 寄存器在设备中的offset
    /// @return u32 要操作的寄存器相对于mmio_offset的偏移量
    fn cam_offset(&self, bus_device_function: BusDeviceFunction, register_offset: u16) -> u32 {
        assert!(bus_device_function.valid());
        let bdf = ((bus_device_function.bus - self.bus_begin) as u32) << 8
            | (bus_device_function.device as u32) << 3
            | bus_device_function.function as u32;
        let address = bdf << 12 | register_offset as u32;
        // Ensure that address is word-aligned.
        assert!(address & 0x3 == 0);
        address
    }
    /// @brief 通过bus_device_function和offset读取相应位置寄存器的值（32位）
    /// @param bus_device_function 在同一个group中pci设备的唯一标识符
    /// @param register_offset 寄存器在设备中的offset
    /// @return u32 寄存器读值结果
    pub fn read_config(&self, bus_device_function: BusDeviceFunction, register_offset: u16) -> u32 {
        let address = self.cam_offset(bus_device_function, register_offset);
        unsafe {
            // Right shift to convert from byte offset to word offset.
            ((self.mmio_guard.as_ref().unwrap().vaddr().data() as *mut u32)
                .add((address >> 2) as usize))
            .read_volatile()
        }
    }

    /// @brief 通过bus_device_function和offset写入相应位置寄存器值（32位）
    /// @param bus_device_function 在同一个group中pci设备的唯一标识符
    /// @param register_offset 寄存器在设备中的offset
    /// @param data 要写入的值
    pub fn write_config(
        &mut self,
        bus_device_function: BusDeviceFunction,
        register_offset: u16,
        data: u32,
    ) {
        let address = self.cam_offset(bus_device_function, register_offset);
        // Safe because both the `mmio_base` and the address offset are properly aligned, and the
        // resulting pointer is within the MMIO range of the CAM.
        unsafe {
            // Right shift to convert from byte offset to word offset.
            ((self.mmio_guard.as_ref().unwrap().vaddr().data() as *mut u32)
                .add((address >> 2) as usize))
            .write_volatile(data)
        }
    }
    /// @brief 返回迭代器，遍历pcie设备的external_capabilities
    pub fn external_capabilities(
        &self,
        bus_device_function: BusDeviceFunction,
    ) -> ExternalCapabilityIterator {
        ExternalCapabilityIterator {
            root: self,
            bus_device_function,
            next_capability_offset: Some(0x100),
        }
    }
}
/// Gets the capabilities 'pointer' for the device function, if any.
/// @brief 获取第一个capability 的offset
/// @param bus_device_function PCI设备的唯一标识
/// @return Option<u8> offset
pub fn capabilities_offset(bus_device_function: BusDeviceFunction) -> Option<u8> {
    let result = PciArch::read_config(&bus_device_function, STATUS_COMMAND_OFFSET);
    let status: Status = Status::from_bits_truncate((result >> 16) as u16);
    if status.contains(Status::CAPABILITIES_LIST) {
        let cap_pointer = PciArch::read_config(&bus_device_function, 0x34) as u8 & 0xFC;
        Some(cap_pointer)
    } else {
        None
    }
}

/// @brief 读取pci设备头部
/// @param bus_device_function PCI设备的唯一标识
/// @param add_to_list 是否添加到链表
/// @return 返回的header(trait 类型)
fn pci_read_header(
    bus_device_function: BusDeviceFunction,
    add_to_list: bool,
) -> Result<Box<dyn PciDeviceStructure>, PciError> {
    // 先读取公共header
    let result = PciArch::read_config(&bus_device_function, 0x00);
    let vendor_id = result as u16;
    let device_id = (result >> 16) as u16;

    let result = PciArch::read_config(&bus_device_function, 0x04);
    let command = result as u16;
    let status = (result >> 16) as u16;

    let result = PciArch::read_config(&bus_device_function, 0x08);
    let revision_id = result as u8;
    let prog_if = (result >> 8) as u8;
    let subclass = (result >> 16) as u8;
    let class_code = (result >> 24) as u8;

    let result = PciArch::read_config(&bus_device_function, 0x0c);
    let cache_line_size = result as u8;
    let latency_timer = (result >> 8) as u8;
    let header_type = (result >> 16) as u8;
    let bist = (result >> 24) as u8;
    if vendor_id == 0xffff {
        return Err(PciError::GetWrongHeader);
    }
    let header = PciDeviceStructureHeader {
        bus_device_function,
        vendor_id,
        device_id,
        command,
        status,
        revision_id,
        prog_if,
        subclass,
        class_code,
        cache_line_size,
        latency_timer,
        header_type,
        bist,
    };
    match HeaderType::from(header_type & 0x7f) {
        HeaderType::Standard => {
            let general_device = pci_read_general_device_header(header, &bus_device_function);
            let box_general_device = Box::new(general_device);
            let box_general_device_clone = box_general_device.clone();
            if add_to_list {
                PCI_DEVICE_LINKEDLIST.add(box_general_device);
            }
            Ok(box_general_device_clone)
        }
        HeaderType::PciPciBridge => {
            let pci_to_pci_bridge = pci_read_pci_to_pci_bridge_header(header, &bus_device_function);
            let box_pci_to_pci_bridge = Box::new(pci_to_pci_bridge);
            let box_pci_to_pci_bridge_clone = box_pci_to_pci_bridge.clone();
            if add_to_list {
                PCI_DEVICE_LINKEDLIST.add(box_pci_to_pci_bridge);
            }
            Ok(box_pci_to_pci_bridge_clone)
        }
        HeaderType::PciCardbusBridge => {
            let pci_cardbus_bridge =
                pci_read_pci_to_cardbus_bridge_header(header, &bus_device_function);
            let box_pci_cardbus_bridge = Box::new(pci_cardbus_bridge);
            let box_pci_cardbus_bridge_clone = box_pci_cardbus_bridge.clone();
            if add_to_list {
                PCI_DEVICE_LINKEDLIST.add(box_pci_cardbus_bridge);
            }
            Ok(box_pci_cardbus_bridge_clone)
        }
        HeaderType::Unrecognised(_) => Err(PciError::UnrecognisedHeaderType),
    }
}

/// @brief 读取type为0x0的pci设备的header
/// 本函数只应被 pci_read_header()调用
/// @param common_header 共有头部
/// @param bus_device_function PCI设备的唯一标识
/// @return Pci_Device_Structure_General_Device 标准设备头部
fn pci_read_general_device_header(
    common_header: PciDeviceStructureHeader,
    bus_device_function: &BusDeviceFunction,
) -> PciDeviceStructureGeneralDevice {
    let standard_device_bar = PciStandardDeviceBar::default();
    let cardbus_cis_pointer = PciArch::read_config(bus_device_function, 0x28);

    let result = PciArch::read_config(bus_device_function, 0x2c);
    let subsystem_vendor_id = result as u16;
    let subsystem_id = (result >> 16) as u16;

    let expansion_rom_base_address = PciArch::read_config(bus_device_function, 0x30);

    let result = PciArch::read_config(bus_device_function, 0x34);
    let capabilities_pointer = result as u8;
    let reserved0 = (result >> 8) as u8;
    let reserved1 = (result >> 16) as u16;

    let reserved2 = PciArch::read_config(bus_device_function, 0x38);

    let result = PciArch::read_config(bus_device_function, 0x3c);
    let interrupt_line = result as u8;
    let interrupt_pin = (result >> 8) as u8;
    let min_grant = (result >> 16) as u8;
    let max_latency = (result >> 24) as u8;
    PciDeviceStructureGeneralDevice {
        common_header,
        irq_type: IrqType::Unused,
        irq_vector: Vec::new(),
        standard_device_bar,
        cardbus_cis_pointer,
        subsystem_vendor_id,
        subsystem_id,
        expansion_rom_base_address,
        capabilities_pointer,
        reserved0,
        reserved1,
        reserved2,
        interrupt_line,
        interrupt_pin,
        min_grant,
        max_latency,
    }
}

/// @brief 读取type为0x1的pci设备的header
/// 本函数只应被 pci_read_header()调用
/// @param common_header 共有头部
/// @param bus_device_function PCI设备的唯一标识
/// @return Pci_Device_Structure_Pci_to_Pci_Bridge pci-to-pci 桥设备头部
fn pci_read_pci_to_pci_bridge_header(
    common_header: PciDeviceStructureHeader,
    bus_device_function: &BusDeviceFunction,
) -> PciDeviceStructurePciToPciBridge {
    let bar0 = PciArch::read_config(bus_device_function, 0x10);
    let bar1 = PciArch::read_config(bus_device_function, 0x14);

    let result = PciArch::read_config(bus_device_function, 0x18);

    let primary_bus_number = result as u8;
    let secondary_bus_number = (result >> 8) as u8;
    let subordinate_bus_number = (result >> 16) as u8;
    let secondary_latency_timer = (result >> 24) as u8;

    let result = PciArch::read_config(bus_device_function, 0x1c);
    let io_base = result as u8;
    let io_limit = (result >> 8) as u8;
    let secondary_status = (result >> 16) as u16;

    let result = PciArch::read_config(bus_device_function, 0x20);
    let memory_base = result as u16;
    let memory_limit = (result >> 16) as u16;

    let result = PciArch::read_config(bus_device_function, 0x24);
    let prefetchable_memory_base = result as u16;
    let prefetchable_memory_limit = (result >> 16) as u16;

    let prefetchable_base_upper_32_bits = PciArch::read_config(bus_device_function, 0x28);
    let prefetchable_limit_upper_32_bits = PciArch::read_config(bus_device_function, 0x2c);

    let result = PciArch::read_config(bus_device_function, 0x30);
    let io_base_upper_16_bits = result as u16;
    let io_limit_upper_16_bits = (result >> 16) as u16;

    let result = PciArch::read_config(bus_device_function, 0x34);
    let capability_pointer = result as u8;
    let reserved0 = (result >> 8) as u8;
    let reserved1 = (result >> 16) as u16;

    let expansion_rom_base_address = PciArch::read_config(bus_device_function, 0x38);

    let result = PciArch::read_config(bus_device_function, 0x3c);
    let interrupt_line = result as u8;
    let interrupt_pin = (result >> 8) as u8;
    let bridge_control = (result >> 16) as u16;
    PciDeviceStructurePciToPciBridge {
        common_header,
        irq_type: IrqType::Unused,
        irq_vector: Vec::new(),
        bar0,
        bar1,
        primary_bus_number,
        secondary_bus_number,
        subordinate_bus_number,
        secondary_latency_timer,
        io_base,
        io_limit,
        secondary_status,
        memory_base,
        memory_limit,
        prefetchable_memory_base,
        prefetchable_memory_limit,
        prefetchable_base_upper_32_bits,
        prefetchable_limit_upper_32_bits,
        io_base_upper_16_bits,
        io_limit_upper_16_bits,
        capability_pointer,
        reserved0,
        reserved1,
        expansion_rom_base_address,
        interrupt_line,
        interrupt_pin,
        bridge_control,
    }
}

/// @brief 读取type为0x2的pci设备的header
/// 本函数只应被 pci_read_header()调用
/// @param common_header 共有头部
/// @param bus_device_function PCI设备的唯一标识
/// @return   Pci_Device_Structure_Pci_to_Cardbus_Bridge  pci-to-cardbus 桥设备头部
fn pci_read_pci_to_cardbus_bridge_header(
    common_header: PciDeviceStructureHeader,
    busdevicefunction: &BusDeviceFunction,
) -> PciDeviceStructurePciToCardbusBridge {
    let cardbus_socket_ex_ca_base_address = PciArch::read_config(busdevicefunction, 0x10);

    let result = PciArch::read_config(busdevicefunction, 0x14);
    let offset_of_capabilities_list = result as u8;
    let reserved = (result >> 8) as u8;
    let secondary_status = (result >> 16) as u16;

    let result = PciArch::read_config(busdevicefunction, 0x18);
    let pci_bus_number = result as u8;
    let card_bus_bus_number = (result >> 8) as u8;
    let subordinate_bus_number = (result >> 16) as u8;
    let card_bus_latency_timer = (result >> 24) as u8;

    let memory_base_address0 = PciArch::read_config(busdevicefunction, 0x1c);
    let memory_limit0 = PciArch::read_config(busdevicefunction, 0x20);
    let memory_base_address1 = PciArch::read_config(busdevicefunction, 0x24);
    let memory_limit1 = PciArch::read_config(busdevicefunction, 0x28);

    let io_base_address0 = PciArch::read_config(busdevicefunction, 0x2c);
    let io_limit0 = PciArch::read_config(busdevicefunction, 0x30);
    let io_base_address1 = PciArch::read_config(busdevicefunction, 0x34);
    let io_limit1 = PciArch::read_config(busdevicefunction, 0x38);
    let result = PciArch::read_config(busdevicefunction, 0x3c);
    let interrupt_line = result as u8;
    let interrupt_pin = (result >> 8) as u8;
    let bridge_control = (result >> 16) as u16;

    let result = PciArch::read_config(busdevicefunction, 0x40);
    let subsystem_device_id = result as u16;
    let subsystem_vendor_id = (result >> 16) as u16;

    let pc_card_legacy_mode_base_address_16_bit = PciArch::read_config(busdevicefunction, 0x44);
    PciDeviceStructurePciToCardbusBridge {
        common_header,
        cardbus_socket_ex_ca_base_address,
        offset_of_capabilities_list,
        reserved,
        secondary_status,
        pci_bus_number,
        card_bus_bus_number,
        subordinate_bus_number,
        card_bus_latency_timer,
        memory_base_address0,
        memory_limit0,
        memory_base_address1,
        memory_limit1,
        io_base_address0,
        io_limit0,
        io_base_address1,
        io_limit1,
        interrupt_line,
        interrupt_pin,
        bridge_control,
        subsystem_device_id,
        subsystem_vendor_id,
        pc_card_legacy_mode_base_address_16_bit,
    }
}

/// @brief 检查所有bus上的设备并将其加入链表
/// @return 成功返回ok(),失败返回失败原因
fn pci_check_all_buses() -> Result<u8, PciError> {
    kinfo!("Checking all devices in PCI bus...");
    let busdevicefunction = BusDeviceFunction {
        bus: 0,
        device: 0,
        function: 0,
    };
    let header = pci_read_header(busdevicefunction, false)?;
    let common_header = header.common_header();
    pci_check_bus(0)?;
    if common_header.header_type & 0x80 != 0 {
        for function in 1..8 {
            pci_check_bus(function)?;
        }
    }
    Ok(0)
}
/// @brief 检查特定设备并将其加入链表
/// @return 成功返回ok(),失败返回失败原因
fn pci_check_function(busdevicefunction: BusDeviceFunction) -> Result<u8, PciError> {
    //kdebug!("PCI check function {}", busdevicefunction.function);
    let header = match pci_read_header(busdevicefunction, true) {
        Ok(header) => header,
        Err(PciError::GetWrongHeader) => {
            return Ok(255);
        }
        Err(e) => {
            return Err(e);
        }
    };
    let common_header = header.common_header();
    if (common_header.class_code == 0x06)
        && (common_header.subclass == 0x04 || common_header.subclass == 0x09)
    {
        let pci_to_pci_bridge = header
            .as_pci_to_pci_bridge_device()
            .ok_or(PciError::PciDeviceStructureTransformError)?;
        let secondary_bus = pci_to_pci_bridge.secondary_bus_number;
        pci_check_bus(secondary_bus)?;
    }
    Ok(0)
}

/// @brief 检查device上的设备并将其加入链表
/// @return 成功返回ok(),失败返回失败原因
fn pci_check_device(bus: u8, device: u8) -> Result<u8, PciError> {
    //kdebug!("PCI check device {}", device);
    let busdevicefunction = BusDeviceFunction {
        bus,
        device,
        function: 0,
    };
    let header = match pci_read_header(busdevicefunction, false) {
        Ok(header) => header,
        Err(PciError::GetWrongHeader) => {
            //设备不存在，直接返回即可，不用终止遍历
            return Ok(255);
        }
        Err(e) => {
            return Err(e);
        }
    };
    pci_check_function(busdevicefunction)?;
    let common_header = header.common_header();
    if common_header.header_type & 0x80 != 0 {
        kdebug!(
            "Detected multi func device in bus{},device{}",
            busdevicefunction.bus,
            busdevicefunction.device
        );
        // 这是一个多function的设备，因此查询剩余的function
        for function in 1..8 {
            let busdevicefunction = BusDeviceFunction {
                bus,
                device,
                function,
            };
            pci_check_function(busdevicefunction)?;
        }
    }
    Ok(0)
}
/// @brief 检查该bus上的设备并将其加入链表
/// @return 成功返回ok(),失败返回失败原因
fn pci_check_bus(bus: u8) -> Result<u8, PciError> {
    //kdebug!("PCI check bus {}", bus);
    for device in 0..32 {
        pci_check_device(bus, device)?;
    }
    Ok(0)
}

/// pci初始化函数
#[inline(never)]
pub fn pci_init() {
    kinfo!("Initializing PCI bus...");
    if let Err(e) = pci_check_all_buses() {
        kerror!("pci init failed when checking bus because of error: {}", e);
        return;
    }
    kinfo!(
        "Total pci device and function num = {}",
        PCI_DEVICE_LINKEDLIST.num()
    );
    let list = PCI_DEVICE_LINKEDLIST.read();
    for box_pci_device in list.iter() {
        let common_header = box_pci_device.common_header();
        match box_pci_device.header_type() {
            HeaderType::Standard if common_header.status & 0x10 != 0 => {
                kinfo!("Found pci standard device with class code ={} subclass={} status={:#x} cap_pointer={:#x}  vendor={:#x}, device id={:#x},bdf={}", common_header.class_code, common_header.subclass, common_header.status, box_pci_device.as_standard_device().unwrap().capabilities_pointer,common_header.vendor_id, common_header.device_id,common_header.bus_device_function);
            }
            HeaderType::Standard => {
                kinfo!(
                    "Found pci standard device with class code ={} subclass={} status={:#x} ",
                    common_header.class_code,
                    common_header.subclass,
                    common_header.status
                );
            }
            HeaderType::PciPciBridge if common_header.status & 0x10 != 0 => {
                kinfo!("Found pci-to-pci bridge device with class code ={} subclass={} status={:#x} cap_pointer={:#x}", common_header.class_code, common_header.subclass, common_header.status, box_pci_device.as_standard_device().unwrap().capabilities_pointer);
            }
            HeaderType::PciPciBridge => {
                kinfo!(
                    "Found pci-to-pci bridge device with class code ={} subclass={} status={:#x} ",
                    common_header.class_code,
                    common_header.subclass,
                    common_header.status
                );
            }
            HeaderType::PciCardbusBridge => {
                kinfo!(
                    "Found pcicardbus bridge device with class code ={} subclass={} status={:#x} ",
                    common_header.class_code,
                    common_header.subclass,
                    common_header.status
                );
            }
            HeaderType::Unrecognised(_) => {}
        }
    }
    kinfo!("PCI bus initialized.");
}

/// An identifier for a PCI bus, device and function.
/// PCI设备的唯一标识
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BusDeviceFunction {
    /// The PCI bus number, between 0 and 255.
    pub bus: u8,
    /// The device number on the bus, between 0 and 31.
    pub device: u8,
    /// The function number of the device, between 0 and 7.
    pub function: u8,
}
impl BusDeviceFunction {
    /// Returns whether the device and function numbers are valid, i.e. the device is between 0 and
    ///@brief 检测BusDeviceFunction实例是否有效
    ///@param self
    ///@return bool 是否有效
    #[allow(dead_code)]
    pub fn valid(&self) -> bool {
        self.device < 32 && self.function < 8
    }
}
///实现BusDeviceFunction的Display trait，使其可以直接输出
impl Display for BusDeviceFunction {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(
            f,
            "bus {} device {} function{}",
            self.bus, self.device, self.function
        )
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
#[derive(Clone, Debug)]
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
        mmio_guard: Arc<MMIOSpaceGuard>,
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
    pub fn virtual_address(&self) -> Option<VirtAddr> {
        if let Self::Memory { mmio_guard, .. } = self {
            Some(mmio_guard.vaddr())
        } else {
            None
        }
    }
}
///实现BarInfo的Display trait，自定义输出
impl Display for BarInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Memory {
                address_type,
                prefetchable,
                address,
                size,
                mmio_guard,
            } => write!(
                f,
                "Memory space at {:#010x}, size {}, type {:?}, prefetchable {}, mmio_guard: {:?}",
                address, size, address_type, prefetchable, mmio_guard
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
// todo 增加对桥的bar的支持
pub trait PciDeviceBar {}

///一个普通PCI设备（非桥）有6个BAR寄存器，PciStandardDeviceBar存储其全部信息
#[derive(Clone, Debug)]
pub struct PciStandardDeviceBar {
    bar0: BarInfo,
    bar1: BarInfo,
    bar2: BarInfo,
    bar3: BarInfo,
    bar4: BarInfo,
    bar5: BarInfo,
}

impl PciStandardDeviceBar {
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
            5 => Ok(&self.bar5),
            _ => Err(PciError::InvalidBarType),
        }
    }
}
///实现PciStandardDeviceBar的Display trait，使其可以直接输出
impl Display for PciStandardDeviceBar {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "\r\nBar0:{}\r\nBar1:{}\r\nBar2:{}\r\nBar3:{}\r\nBar4:{}\r\nBar5:{}",
            self.bar0, self.bar1, self.bar2, self.bar3, self.bar4, self.bar5
        )
    }
}
///实现PciStandardDeviceBar的Default trait，使其可以简单初始化
impl Default for PciStandardDeviceBar {
    fn default() -> Self {
        PciStandardDeviceBar {
            bar0: BarInfo::Unused,
            bar1: BarInfo::Unused,
            bar2: BarInfo::Unused,
            bar3: BarInfo::Unused,
            bar4: BarInfo::Unused,
            bar5: BarInfo::Unused,
        }
    }
}

///@brief 将某个pci设备的bar寄存器读取值后映射到虚拟地址
///@param self ，bus_device_function PCI设备的唯一标识符
///@return Result<PciStandardDeviceBar, PciError> 成功则返回对应的PciStandardDeviceBar结构体，失败则返回错误类型
pub fn pci_bar_init(
    bus_device_function: BusDeviceFunction,
) -> Result<PciStandardDeviceBar, PciError> {
    let mut device_bar: PciStandardDeviceBar = PciStandardDeviceBar::default();
    let mut bar_index_ignore: u8 = 255;
    for bar_index in 0..6 {
        if bar_index == bar_index_ignore {
            continue;
        }
        let bar_info;
        let bar_orig = PciArch::read_config(&bus_device_function, BAR0_OFFSET + 4 * bar_index);
        PciArch::write_config(
            &bus_device_function,
            BAR0_OFFSET + 4 * bar_index,
            0xffffffff,
        );
        let size_mask = PciArch::read_config(&bus_device_function, BAR0_OFFSET + 4 * bar_index);
        // A wrapping add is necessary to correctly handle the case of unused BARs, which read back
        // as 0, and should be treated as size 0.
        let size = (!(size_mask & 0xfffffff0)).wrapping_add(1);
        //kdebug!("bar_orig:{:#x},size: {:#x}", bar_orig,size);
        // Restore the original value.
        PciArch::write_config(&bus_device_function, BAR0_OFFSET + 4 * bar_index, bar_orig);
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
                let address_top =
                    PciArch::read_config(&bus_device_function, BAR0_OFFSET + 4 * (bar_index + 1));
                address |= u64::from(address_top) << 32;
                bar_index_ignore = bar_index + 1; //下个bar跳过，因为64位的memory bar覆盖了两个bar
            }
            let pci_address = PciAddr::new(address as usize);
            let paddr = PciArch::address_pci_to_physical(pci_address); //PCI总线域物理地址转换为存储器域物理地址

            let space_guard: Arc<MMIOSpaceGuard>;
            unsafe {
                let size_want = size as usize;
                let tmp = mmio_pool()
                    .create_mmio(size_want)
                    .map_err(|_| PciError::CreateMmioError)?;
                space_guard = Arc::new(tmp);
                //kdebug!("Pci bar init: mmio space: {space_guard:?}, paddr={paddr:?}, size_want={size_want}");
                assert!(
                    space_guard.map_phys(paddr, size_want).is_ok(),
                    "pci_bar_init: map_phys failed"
                );
            }
            bar_info = BarInfo::Memory {
                address_type,
                prefetchable,
                address,
                size,
                mmio_guard: space_guard,
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
    //kdebug!("pci_device_bar:{}", device_bar);
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
    pub bus_device_function: BusDeviceFunction,
    pub next_capability_offset: Option<u8>,
}

impl Iterator for CapabilityIterator {
    type Item = CapabilityInfo;
    fn next(&mut self) -> Option<Self::Item> {
        let offset = self.next_capability_offset?;

        // Read the first 4 bytes of the capability.
        let capability_header = PciArch::read_config(&self.bus_device_function, offset);
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

/// Information about a PCIe device capability.
/// PCIe设备的external capability的信息
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct ExternalCapabilityInfo {
    /// The offset of the capability in the PCI configuration space of the device function.
    pub offset: u16,
    /// The ID of the capability.
    pub id: u16,
    /// The third and fourth bytes of the capability, to save reading them again.
    pub capability_version: u8,
}

/// Iterator over capabilities for a device.
/// 创建迭代器以遍历PCIe设备的external capability
#[derive(Debug)]
pub struct ExternalCapabilityIterator<'a> {
    pub root: &'a PciRoot,
    pub bus_device_function: BusDeviceFunction,
    pub next_capability_offset: Option<u16>,
}
impl<'a> Iterator for ExternalCapabilityIterator<'a> {
    type Item = ExternalCapabilityInfo;
    fn next(&mut self) -> Option<Self::Item> {
        let offset = self.next_capability_offset?;

        // Read the first 4 bytes of the capability.
        let capability_header = self.root.read_config(self.bus_device_function, offset);
        let id = capability_header as u16;
        let next_offset = (capability_header >> 20) as u16;
        let capability_version = ((capability_header >> 16) & 0xf) as u8;

        self.next_capability_offset = if next_offset == 0 {
            None
        } else if next_offset < 0x100 || next_offset & 0x3 != 0 {
            kwarn!("Invalid next capability offset {:#04x}", next_offset);
            None
        } else {
            Some(next_offset)
        };

        Some(ExternalCapabilityInfo {
            offset,
            id,
            capability_version,
        })
    }
}
