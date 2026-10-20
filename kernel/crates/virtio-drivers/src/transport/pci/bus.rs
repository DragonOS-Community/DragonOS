//! Module for dealing with a PCI bus in general, without anything specific to VirtIO.

use bitflags::bitflags;
use core::{
    convert::TryFrom,
    fmt::{self, Display, Formatter},
};
use log::warn;

const INVALID_READ: u32 = 0xffffffff;

/// The maximum number of devices on a bus.
const MAX_DEVICES: u8 = 32;
/// The maximum number of functions on a device.
const MAX_FUNCTIONS: u8 = 8;

/// The offset in bytes to the status and command fields within PCI configuration space.
const STATUS_COMMAND_OFFSET: u8 = 0x04;
/// The offset in bytes to BAR0 within PCI configuration space.
const BAR0_OFFSET: u8 = 0x10;

/// ID for vendor-specific PCI capabilities.
pub const PCI_CAP_ID_VNDR: u8 = 0x09;

bitflags! {
    /// The status register in PCI configuration space.
    #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
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
    #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
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

/// Errors accessing a PCI device.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PciError {
    /// The device reported an invalid BAR type.
    InvalidBarType,
}

impl Display for PciError {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            Self::InvalidBarType => write!(f, "Invalid PCI BAR type."),
        }
    }
}

/// The root complex of a PCI bus.
#[derive(Debug)]
pub struct PciRoot {
    mmio_base: *mut u32,
    cam: Cam,
}

/// A PCI Configuration Access Mechanism.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Cam {
    /// The PCI memory-mapped Configuration Access Mechanism.
    ///
    /// This provides access to 256 bytes of configuration space per device function.
    MmioCam,
    /// The PCIe memory-mapped Enhanced Configuration Access Mechanism.
    ///
    /// This provides access to 4 KiB of configuration space per device function.
    Ecam,
}

impl Cam {
    /// Returns the total size in bytes of the memory-mapped region.
    pub const fn size(self) -> u32 {
        match self {
            Self::MmioCam => 0x1000000,
            Self::Ecam => 0x10000000,
        }
    }
}

impl PciRoot {
    /// Wraps the PCI root complex with the given MMIO base address.
    ///
    /// Panics if the base address is not aligned to a 4-byte boundary.
    ///
    /// # Safety
    ///
    /// `mmio_base` must be a valid pointer to an appropriately-mapped MMIO region of at least
    /// 16 MiB (if `cam == Cam::MmioCam`) or 256 MiB (if `cam == Cam::Ecam`). The pointer must be
    /// valid for the entire lifetime of the program (i.e. `'static`), which implies that no Rust
    /// references may be used to access any of the memory region at any point.
    pub unsafe fn new(mmio_base: *mut u8, cam: Cam) -> Self {
        assert!(mmio_base as usize & 0x3 == 0);
        Self {
            mmio_base: mmio_base as *mut u32,
            cam,
        }
    }

    /// Makes a clone of the `PciRoot`, pointing at the same MMIO region.
    ///
    /// # Safety
    ///
    /// This function allows concurrent mutable access to the PCI CAM. To avoid this causing
    /// problems, the returned `PciRoot` instance must only be used to read read-only fields.
    unsafe fn unsafe_clone(&self) -> Self {
        Self {
            mmio_base: self.mmio_base,
            cam: self.cam,
        }
    }

    fn cam_offset(&self, device_function: DeviceFunction, register_offset: u8) -> u32 {
        assert!(device_function.valid());

        let bdf = (device_function.bus as u32) << 8
            | (device_function.device as u32) << 3
            | device_function.function as u32;
        let address =
            bdf << match self.cam {
                Cam::MmioCam => 8,
                Cam::Ecam => 12,
            } | register_offset as u32;
        // Ensure that address is within range.
        assert!(address < self.cam.size());
        // Ensure that address is word-aligned.
        assert!(address & 0x3 == 0);
        address
    }

    /// Reads 4 bytes from configuration space using the appropriate CAM.
    pub(crate) fn config_read_word(
        &self,
        device_function: DeviceFunction,
        register_offset: u8,
    ) -> u32 {
        let address = self.cam_offset(device_function, register_offset);
        // Safe because both the `mmio_base` and the address offset are properly aligned, and the
        // resulting pointer is within the MMIO range of the CAM.
        unsafe {
            // Right shift to convert from byte offset to word offset.
            (self.mmio_base.add((address >> 2) as usize)).read_volatile()
        }
    }

    /// Writes 4 bytes to configuration space using the appropriate CAM.
    pub(crate) fn config_write_word(
        &mut self,
        device_function: DeviceFunction,
        register_offset: u8,
        data: u32,
    ) {
        let address = self.cam_offset(device_function, register_offset);
        // Safe because both the `mmio_base` and the address offset are properly aligned, and the
        // resulting pointer is within the MMIO range of the CAM.
        unsafe {
            // Right shift to convert from byte offset to word offset.
            (self.mmio_base.add((address >> 2) as usize)).write_volatile(data)
        }
    }

    /// Enumerates PCI devices on the given bus.
    pub fn enumerate_bus(&self, bus: u8) -> BusDeviceIterator {
        // Safe because the BusDeviceIterator only reads read-only fields.
        let root = unsafe { self.unsafe_clone() };
        BusDeviceIterator {
            root,
            next: DeviceFunction {
                bus,
                device: 0,
                function: 0,
            },
        }
    }

    /// Reads the status and command registers of the given device function.
    pub fn get_status_command(&self, device_function: DeviceFunction) -> (Status, Command) {
        let status_command = self.config_read_word(device_function, STATUS_COMMAND_OFFSET);
        let status = Status::from_bits_truncate((status_command >> 16) as u16);
        let command = Command::from_bits_truncate(status_command as u16);
        (status, command)
    }

    /// Sets the command register of the given device function.
    pub fn set_command(&mut self, device_function: DeviceFunction, command: Command) {
        self.config_write_word(
            device_function,
            STATUS_COMMAND_OFFSET,
            command.bits().into(),
        );
    }

    /// Gets an iterator over the capabilities of the given device function.
    pub fn capabilities(&self, device_function: DeviceFunction) -> CapabilityIterator<'_> {
        CapabilityIterator {
            root: self,
            device_function,
            next_capability_offset: self.capabilities_offset(device_function),
        }
    }

    /// Gets information about the given BAR of the given device function.
    pub fn bar_info(
        &mut self,
        device_function: DeviceFunction,
        bar_index: u8,
    ) -> Result<BarInfo, PciError> {
        let bar_orig = self.config_read_word(device_function, BAR0_OFFSET + 4 * bar_index);

        // Get the size of the BAR.
        self.config_write_word(device_function, BAR0_OFFSET + 4 * bar_index, 0xffffffff);
        let size_mask = self.config_read_word(device_function, BAR0_OFFSET + 4 * bar_index);
        // A wrapping add is necessary to correctly handle the case of unused BARs, which read back
        // as 0, and should be treated as size 0.
        let size = (!(size_mask & 0xfffffff0)).wrapping_add(1);

        // Restore the original value.
        self.config_write_word(device_function, BAR0_OFFSET + 4 * bar_index, bar_orig);

        if bar_orig & 0x00000001 == 0x00000001 {
            // I/O space
            let address = bar_orig & 0xfffffffc;
            Ok(BarInfo::IO { address, size })
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
                    self.config_read_word(device_function, BAR0_OFFSET + 4 * (bar_index + 1));
                address |= u64::from(address_top) << 32;
            }
            Ok(BarInfo::Memory {
                address_type,
                prefetchable,
                address,
                size,
            })
        }
    }

    /// Sets the address of the given 32-bit memory or I/O BAR of the given device function.
    pub fn set_bar_32(&mut self, device_function: DeviceFunction, bar_index: u8, address: u32) {
        self.config_write_word(device_function, BAR0_OFFSET + 4 * bar_index, address);
    }

    /// Sets the address of the given 64-bit memory BAR of the given device function.
    pub fn set_bar_64(&mut self, device_function: DeviceFunction, bar_index: u8, address: u64) {
        self.config_write_word(device_function, BAR0_OFFSET + 4 * bar_index, address as u32);
        self.config_write_word(
            device_function,
            BAR0_OFFSET + 4 * (bar_index + 1),
            (address >> 32) as u32,
        );
    }

    /// Gets the capabilities 'pointer' for the device function, if any.
    fn capabilities_offset(&self, device_function: DeviceFunction) -> Option<u8> {
        let (status, _) = self.get_status_command(device_function);
        if status.contains(Status::CAPABILITIES_LIST) {
            Some((self.config_read_word(device_function, 0x34) & 0xFC) as u8)
        } else {
            None
        }
    }
}

/// Information about a PCI Base Address Register.
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
    },
    /// The BAR is for an I/O region.
    IO {
        /// The I/O address, always 4-byte aligned.
        address: u32,
        /// The size of the BAR in bytes.
        size: u32,
    },
}

impl BarInfo {
    /// Returns whether this BAR is a 64-bit memory region, and so takes two entries in the table in
    /// configuration space.
    pub fn takes_two_entries(&self) -> bool {
        matches!(
            self,
            BarInfo::Memory {
                address_type: MemoryBarType::Width64,
                ..
            }
        )
    }

    /// Returns the address and size of this BAR if it is a memory bar, or `None` if it is an IO
    /// BAR.
    pub fn memory_address_size(&self) -> Option<(u64, u32)> {
        if let Self::Memory { address, size, .. } = self {
            Some((*address, *size))
        } else {
            None
        }
    }
}

impl Display for BarInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Memory {
                address_type,
                prefetchable,
                address,
                size,
            } => write!(
                f,
                "Memory space at {:#010x}, size {}, type {:?}, prefetchable {}",
                address, size, address_type, prefetchable
            ),
            Self::IO { address, size } => {
                write!(f, "I/O space at {:#010x}, size {}", address, size)
            }
        }
    }
}

/// The location allowed for a memory BAR.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MemoryBarType {
    /// The BAR has a 32-bit address and can be mapped anywhere in 32-bit address space.
    Width32,
    /// The BAR must be mapped below 1MiB.
    Below1MiB,
    /// The BAR has a 64-bit address and can be mapped anywhere in 64-bit address space.
    Width64,
}

impl From<MemoryBarType> for u8 {
    fn from(bar_type: MemoryBarType) -> Self {
        match bar_type {
            MemoryBarType::Width32 => 0,
            MemoryBarType::Below1MiB => 1,
            MemoryBarType::Width64 => 2,
        }
    }
}

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

/// Iterator over capabilities for a device.
#[derive(Debug)]
pub struct CapabilityIterator<'a> {
    root: &'a PciRoot,
    device_function: DeviceFunction,
    next_capability_offset: Option<u8>,
}

impl<'a> Iterator for CapabilityIterator<'a> {
    type Item = CapabilityInfo;

    fn next(&mut self) -> Option<Self::Item> {
        let offset = self.next_capability_offset?;

        // Read the first 4 bytes of the capability.
        let capability_header = self.root.config_read_word(self.device_function, offset);
        let id = capability_header as u8;
        let next_offset = (capability_header >> 8) as u8;
        let private_header = (capability_header >> 16) as u16;

        self.next_capability_offset = if next_offset == 0 {
            None
        } else if next_offset < 64 || next_offset & 0x3 != 0 {
            warn!("Invalid next capability offset {:#04x}", next_offset);
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

/// Information about a PCI device capability.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct CapabilityInfo {
    /// The offset of the capability in the PCI configuration space of the device function.
    pub offset: u8,
    /// The ID of the capability.
    pub id: u8,
    /// The third and fourth bytes of the capability, to save reading them again.
    pub private_header: u16,
}

/// An iterator which enumerates PCI devices and functions on a given bus.
#[derive(Debug)]
pub struct BusDeviceIterator {
    /// This must only be used to read read-only fields, and must not be exposed outside this
    /// module, because it uses the same CAM as the main `PciRoot` instance.
    root: PciRoot,
    next: DeviceFunction,
}

impl Iterator for BusDeviceIterator {
    type Item = (DeviceFunction, DeviceFunctionInfo);

    fn next(&mut self) -> Option<Self::Item> {
        while self.next.device < MAX_DEVICES {
            // Read the header for the current device and function.
            let current = self.next;
            let device_vendor = self.root.config_read_word(current, 0);

            // Advance to the next device or function.
            self.next.function += 1;
            if self.next.function >= MAX_FUNCTIONS {
                self.next.function = 0;
                self.next.device += 1;
            }

            if device_vendor != INVALID_READ {
                let class_revision = self.root.config_read_word(current, 8);
                let device_id = (device_vendor >> 16) as u16;
                let vendor_id = device_vendor as u16;
                let class = (class_revision >> 24) as u8;
                let subclass = (class_revision >> 16) as u8;
                let prog_if = (class_revision >> 8) as u8;
                let revision = class_revision as u8;
                let bist_type_latency_cache = self.root.config_read_word(current, 12);
                let header_type = HeaderType::from((bist_type_latency_cache >> 16) as u8 & 0x7f);
                return Some((
                    current,
                    DeviceFunctionInfo {
                        vendor_id,
                        device_id,
                        class,
                        subclass,
                        prog_if,
                        revision,
                        header_type,
                    },
                ));
            }
        }
        None
    }
}

/// An identifier for a PCI bus, device and function.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DeviceFunction {
    /// The PCI bus number, between 0 and 255.
    pub bus: u8,
    /// The device number on the bus, between 0 and 31.
    pub device: u8,
    /// The function number of the device, between 0 and 7.
    pub function: u8,
}

impl DeviceFunction {
    /// Returns whether the device and function numbers are valid, i.e. the device is between 0 and
    /// 31, and the function is between 0 and 7.
    pub fn valid(&self) -> bool {
        self.device < 32 && self.function < 8
    }
}

impl Display for DeviceFunction {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "{:02x}:{:02x}.{}", self.bus, self.device, self.function)
    }
}

/// Information about a PCI device function.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceFunctionInfo {
    /// The PCI vendor ID.
    pub vendor_id: u16,
    /// The PCI device ID.
    pub device_id: u16,
    /// The PCI class.
    pub class: u8,
    /// The PCI subclass.
    pub subclass: u8,
    /// The PCI programming interface byte.
    pub prog_if: u8,
    /// The PCI revision ID.
    pub revision: u8,
    /// The type of PCI device.
    pub header_type: HeaderType,
}

impl Display for DeviceFunctionInfo {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(
            f,
            "{:04x}:{:04x} (class {:02x}.{:02x}, rev {:02x}) {:?}",
            self.vendor_id,
            self.device_id,
            self.class,
            self.subclass,
            self.revision,
            self.header_type,
        )
    }
}

/// The type of a PCI device function header.
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
