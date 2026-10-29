#![allow(dead_code)]

use super::pci::{
    BusDeviceFunction, Command, PciAddr, PciDeviceStructureGeneralDevice, PciError,
    STATUS_COMMAND_OFFSET,
};
use super::root::pci_root_0;

use crate::arch::{MMArch, PciArch, TraitPciArch};
use crate::libs::spinlock::SpinLock;
use crate::mm::mmio_buddy::{mmio_pool, MMIOSpaceGuard};
use crate::mm::{MemoryManagementArch, PhysAddr, VirtAddr};

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::{self, Display, Formatter};

const BAR0_OFFSET: u8 = 0x10;
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
        size: u64,
        /// The virtaddress for a memory bar(mapped).
        mmio_guard: Option<Arc<MMIOSpaceGuard>>,
        /// Individually mapped subranges for metadata-only BARs.
        mapped_ranges: Vec<PciBarMappedRange>,
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
    ///@return Option<(u64, u64) 是Memory Bar返回内存地址与大小，不是则返回None
    pub fn memory_address_size(&self) -> Option<(u64, u64)> {
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
            mmio_guard.as_ref().map(|guard| guard.vaddr())
        } else {
            None
        }
    }

    pub fn virtual_address_at(&self, offset: u64, length: usize) -> Option<VirtAddr> {
        let Self::Memory {
            size,
            mmio_guard,
            mapped_ranges,
            ..
        } = self
        else {
            return None;
        };
        let length = u64::try_from(length).ok()?;
        let end = offset.checked_add(length)?;
        if end > *size {
            return None;
        }
        if let Some(guard) = mmio_guard {
            return Some(guard.vaddr() + usize::try_from(offset).ok()?);
        }
        mapped_ranges.iter().find_map(|range| {
            let range_end = range.offset.checked_add(range.length)?;
            if offset < range.offset || end > range_end {
                return None;
            }
            Some(range.vaddr + usize::try_from(offset - range.offset).ok()?)
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciBarMappingRequest {
    pub bar: u8,
    pub offset: u64,
    pub length: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PciBarSubresource {
    bdf: BusDeviceFunction,
    bar: u8,
    offset: u64,
    length: u64,
    physical_start: u64,
}

lazy_static! {
    static ref PCI_BAR_SUBRESOURCES: SpinLock<Vec<PciBarSubresource>> = SpinLock::new(Vec::new());
}

#[derive(Debug)]
pub struct PciBarSubresourceGuard {
    resource: PciBarSubresource,
}

impl PciBarSubresourceGuard {
    pub fn reserve(
        device: &PciDeviceStructureGeneralDevice,
        bar: u8,
        offset: u64,
        length: u64,
        physical_start: PhysAddr,
    ) -> Result<Self, PciError> {
        if length == 0 {
            return Err(PciError::InvalidBarRange);
        }
        let end = offset
            .checked_add(length)
            .ok_or(PciError::InvalidBarRange)?;
        let bar_info = device.standard_device_bar.read();
        let (_, bar_size) = bar_info
            .get_bar(bar)?
            .memory_address_size()
            .ok_or(PciError::InvalidBarType)?;
        if end > bar_size {
            return Err(PciError::InvalidBarRange);
        }
        drop(bar_info);

        let resource = PciBarSubresource {
            bdf: device.common_header.bus_device_function,
            bar,
            offset,
            length,
            physical_start: physical_start.data() as u64,
        };
        let physical_end = resource
            .physical_start
            .checked_add(length)
            .ok_or(PciError::InvalidBarRange)?;
        let mut resources = PCI_BAR_SUBRESOURCES.lock_irqsave();
        if resources.iter().any(|existing| {
            let existing_end = existing.physical_start.saturating_add(existing.length);
            resource.physical_start < existing_end && existing.physical_start < physical_end
        }) {
            return Err(PciError::InvalidBarRange);
        }
        resources
            .try_reserve(1)
            .map_err(|_| PciError::CreateMmioError)?;
        resources.push(resource);
        Ok(Self { resource })
    }
}

impl Drop for PciBarSubresourceGuard {
    fn drop(&mut self) {
        let mut resources = PCI_BAR_SUBRESOURCES.lock_irqsave();
        if let Some(index) = resources.iter().position(|item| *item == self.resource) {
            resources.remove(index);
        }
    }
}

fn unallocated_bar_has_required_mapping(
    address: u64,
    bar: u8,
    required_mappings: &[PciBarMappingRequest],
) -> bool {
    address == 0 && required_mappings.iter().any(|request| request.bar == bar)
}

#[derive(Clone, Debug)]
pub(crate) struct PciBarMappedRange {
    offset: u64,
    length: u64,
    vaddr: VirtAddr,
    _guard: Arc<MMIOSpaceGuard>,
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
                ..
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
    metadata_only_bars: &[u8],
    required_mappings: &[PciBarMappingRequest],
) -> Result<PciStandardDeviceBar, PciError> {
    struct DecodeGuard {
        bus_device_function: BusDeviceFunction,
        command: u16,
    }

    impl Drop for DecodeGuard {
        fn drop(&mut self) {
            pci_root_0().write_config(
                self.bus_device_function,
                STATUS_COMMAND_OFFSET.into(),
                u32::from(self.command),
            );
        }
    }

    // Prepare every variable-size bookkeeping container before disabling PCI
    // decode or publishing an MMIO mapping. The number of requested subranges
    // is the upper bound for both interval merging and mapped-range metadata.
    let mut consumed_mappings = Vec::new();
    consumed_mappings
        .try_reserve_exact(required_mappings.len())
        .map_err(|_| PciError::CreateMmioError)?;
    consumed_mappings.resize(required_mappings.len(), false);
    let mut intervals_by_bar: [Vec<(usize, usize)>; 6] = core::array::from_fn(|_| Vec::new());
    let mut merged_by_bar: [Vec<(usize, usize)>; 6] = core::array::from_fn(|_| Vec::new());
    let mut mapped_by_bar: [Vec<PciBarMappedRange>; 6] = core::array::from_fn(|_| Vec::new());
    for bar in 0..6u8 {
        let count = required_mappings
            .iter()
            .filter(|request| request.bar == bar)
            .count();
        intervals_by_bar[bar as usize]
            .try_reserve_exact(count)
            .map_err(|_| PciError::CreateMmioError)?;
        merged_by_bar[bar as usize]
            .try_reserve_exact(count)
            .map_err(|_| PciError::CreateMmioError)?;
        mapped_by_bar[bar as usize]
            .try_reserve_exact(count)
            .map_err(|_| PciError::CreateMmioError)?;
    }

    let command_status =
        pci_root_0().read_config(bus_device_function, STATUS_COMMAND_OFFSET.into());
    let command = command_status as u16;
    pci_root_0().write_config(
        bus_device_function,
        STATUS_COMMAND_OFFSET.into(),
        u32::from(command & !(Command::IO_SPACE | Command::MEMORY_SPACE).bits()),
    );
    let _decode_guard = DecodeGuard {
        bus_device_function,
        command,
    };
    let mut device_bar: PciStandardDeviceBar = PciStandardDeviceBar::default();
    let mut bar_index_ignore: u8 = 255;
    for bar_index in 0..6 {
        if bar_index == bar_index_ignore {
            continue;
        }
        let bar_info;
        let bar_offset = BAR0_OFFSET + 4 * bar_index;
        let bar_orig = pci_root_0().read_config(bus_device_function, bar_offset.into());
        if bar_orig & 0x00000001 == 0x00000001 {
            // I/O space
            pci_root_0().write_config(bus_device_function, bar_offset.into(), 0xffffffff);
            let size_mask = pci_root_0().read_config(bus_device_function, bar_offset.into());
            pci_root_0().write_config(bus_device_function, bar_offset.into(), bar_orig);
            let size = (!(size_mask & 0xfffffffc)).wrapping_add(1);
            if size == 0 {
                continue;
            }
            let address = bar_orig & 0xfffffffc;
            bar_info = BarInfo::IO { address, size };
        } else {
            // Memory space
            let mut address = u64::from(bar_orig & 0xfffffff0);
            let prefetchable = bar_orig & 0x00000008 != 0;
            let address_type = MemoryBarType::try_from(((bar_orig & 0x00000006) >> 1) as u8)?;
            let size = if address_type == MemoryBarType::Width64 {
                if bar_index >= 5 {
                    return Err(PciError::InvalidBarType);
                }
                let high_offset = BAR0_OFFSET + 4 * (bar_index + 1);
                let address_top = pci_root_0().read_config(bus_device_function, high_offset.into());
                address |= u64::from(address_top) << 32;
                bar_index_ignore = bar_index + 1; //下个bar跳过，因为64位的memory bar覆盖了两个bar

                pci_root_0().write_config(bus_device_function, bar_offset.into(), 0xffffffff);
                pci_root_0().write_config(bus_device_function, high_offset.into(), 0xffffffff);
                let size_mask_low =
                    pci_root_0().read_config(bus_device_function, bar_offset.into());
                let size_mask_high =
                    pci_root_0().read_config(bus_device_function, high_offset.into());
                pci_root_0().write_config(bus_device_function, bar_offset.into(), bar_orig);
                pci_root_0().write_config(bus_device_function, high_offset.into(), address_top);
                let size_mask =
                    (u64::from(size_mask_high) << 32) | u64::from(size_mask_low & 0xfffffff0);
                (!size_mask).wrapping_add(1)
            } else {
                pci_root_0().write_config(bus_device_function, bar_offset.into(), 0xffffffff);
                let size_mask = pci_root_0().read_config(bus_device_function, bar_offset.into());
                pci_root_0().write_config(bus_device_function, bar_offset.into(), bar_orig);
                u64::from((!(size_mask & 0xfffffff0)).wrapping_add(1))
            };
            if size == 0 {
                continue;
            }
            if unallocated_bar_has_required_mapping(address, bar_index, required_mappings) {
                return Err(PciError::InvalidBarRange);
            }

            let pci_address = PciAddr::new(address as usize);
            let paddr = PciArch::address_pci_to_physical(pci_address); //PCI总线域物理地址转换为存储器域物理地址

            // Keep resource discovery independent from virtual mapping. Very large 64-bit BARs,
            // such as a virtiofs DAX window, must not consume an equally large MMIO VA range.
            let (space_guard, mapped_ranges) = if address == 0 {
                // Preserve the BAR metadata for resource discovery, but never map an
                // unallocated BAR at physical address zero.
                (None, Vec::new())
            } else if metadata_only_bars.contains(&bar_index) {
                let intervals = &mut intervals_by_bar[bar_index as usize];
                for (index, request) in required_mappings.iter().enumerate() {
                    if request.bar != bar_index {
                        continue;
                    }
                    let end = request
                        .offset
                        .checked_add(request.length)
                        .ok_or(PciError::InvalidBarRange)?;
                    if request.length == 0 || end > size {
                        return Err(PciError::InvalidBarRange);
                    }
                    consumed_mappings[index] = true;
                    let start =
                        usize::try_from(request.offset).map_err(|_| PciError::InvalidBarRange)?;
                    let end = usize::try_from(end).map_err(|_| PciError::InvalidBarRange)?;
                    let aligned_start = crate::libs::align::page_align_down(start);
                    let aligned_end = end
                        .checked_add(MMArch::PAGE_SIZE - 1)
                        .map(crate::libs::align::page_align_down)
                        .ok_or(PciError::InvalidBarRange)?;
                    intervals.push((aligned_start, aligned_end));
                }
                intervals.sort_unstable_by_key(|interval| interval.0);
                let merged = &mut merged_by_bar[bar_index as usize];
                for &(start, end) in intervals.iter() {
                    if let Some(last) = merged.last_mut() {
                        if start <= last.1 {
                            last.1 = last.1.max(end);
                            continue;
                        }
                    }
                    merged.push((start, end));
                }

                let mapped_ranges = &mut mapped_by_bar[bar_index as usize];
                for &(start, end) in merged.iter() {
                    let length = end.checked_sub(start).ok_or(PciError::InvalidBarRange)?;
                    let request_paddr = paddr
                        .data()
                        .checked_add(start)
                        .map(PhysAddr::new)
                        .ok_or(PciError::InvalidBarRange)?;
                    let page_offset = request_paddr.data() & (MMArch::PAGE_SIZE - 1);
                    let mapped_length = crate::libs::align::page_align_up(
                        length
                            .checked_add(page_offset)
                            .ok_or(PciError::InvalidBarRange)?,
                    );
                    let guard = mmio_pool()
                        .create_mmio(mapped_length)
                        .map_err(|_| PciError::CreateMmioError)?;
                    let vaddr = unsafe {
                        guard
                            .map_any_phys(request_paddr, length)
                            .map_err(|_| PciError::CreateMmioError)?
                    };
                    mapped_ranges.push(PciBarMappedRange {
                        offset: start as u64,
                        length: length as u64,
                        vaddr,
                        _guard: Arc::try_new(guard).map_err(|_| PciError::CreateMmioError)?,
                    });
                }
                (None, core::mem::take(mapped_ranges))
            } else {
                for (index, request) in required_mappings.iter().enumerate() {
                    if request.bar != bar_index {
                        continue;
                    }
                    let end = request
                        .offset
                        .checked_add(request.length)
                        .ok_or(PciError::InvalidBarRange)?;
                    if request.length == 0 || end > size {
                        return Err(PciError::InvalidBarRange);
                    }
                    consumed_mappings[index] = true;
                }
                let size_want = usize::try_from(size).map_err(|_| PciError::CreateMmioError)?;
                let guard = mmio_pool()
                    .create_mmio(size_want)
                    .map_err(|_| PciError::CreateMmioError)?;
                unsafe {
                    guard
                        .map_phys(paddr, size_want)
                        .map_err(|_| PciError::CreateMmioError)?;
                }
                (
                    Some(Arc::try_new(guard).map_err(|_| PciError::CreateMmioError)?),
                    Vec::new(),
                )
            };
            bar_info = BarInfo::Memory {
                address_type,
                prefetchable,
                address,
                size,
                mmio_guard: space_guard,
                mapped_ranges,
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
    if consumed_mappings.iter().any(|consumed| !consumed) {
        return Err(PciError::InvalidBarRange);
    }
    //debug!("pci_device_bar:{}", device_bar);
    return Ok(device_bar);
}

#[cfg(test)]
mod bar_mapping_tests {
    use super::{unallocated_bar_has_required_mapping, PciBarMappingRequest};

    #[test]
    fn rejects_required_mapping_for_unallocated_bar() {
        let mappings = [PciBarMappingRequest {
            bar: 2,
            offset: 0x1000,
            length: 0x1000,
        }];

        assert!(unallocated_bar_has_required_mapping(0, 2, &mappings));
        assert!(!unallocated_bar_has_required_mapping(0, 1, &mappings));
        assert!(!unallocated_bar_has_required_mapping(0x1000, 2, &mappings));
    }
}
