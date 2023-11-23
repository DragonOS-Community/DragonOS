use crate::arch::TraitPciArch;
use crate::driver::acpi::acpi_manager;
use crate::driver::pci::pci::{
    BusDeviceFunction, PciAddr, PciError, PciRoot, SegmentGroupNumber, PORT_PCI_CONFIG_ADDRESS,
    PORT_PCI_CONFIG_DATA,
};
use crate::include::bindings::bindings::{io_in32, io_out32};
use crate::mm::PhysAddr;

use acpi::mcfg::Mcfg;

pub struct X86_64PciArch;
impl TraitPciArch for X86_64PciArch {
    fn read_config(bus_device_function: &BusDeviceFunction, offset: u8) -> u32 {
        // 构造pci配置空间地址
        let address = ((bus_device_function.bus as u32) << 16)
            | ((bus_device_function.device as u32) << 11)
            | ((bus_device_function.function as u32 & 7) << 8)
            | (offset & 0xfc) as u32
            | (0x80000000);
        let ret = unsafe {
            io_out32(PORT_PCI_CONFIG_ADDRESS, address);
            let temp = io_in32(PORT_PCI_CONFIG_DATA);
            temp
        };
        return ret;
    }

    fn write_config(bus_device_function: &BusDeviceFunction, offset: u8, data: u32) {
        let address = ((bus_device_function.bus as u32) << 16)
            | ((bus_device_function.device as u32) << 11)
            | ((bus_device_function.function as u32 & 7) << 8)
            | (offset & 0xfc) as u32
            | (0x80000000);
        unsafe {
            io_out32(PORT_PCI_CONFIG_ADDRESS, address);
            // 写入数据
            io_out32(PORT_PCI_CONFIG_DATA, data);
        }
    }

    fn address_pci_to_physical(pci_address: PciAddr) -> PhysAddr {
        return PhysAddr::new(pci_address.data());
    }

    fn ecam_root(segement: SegmentGroupNumber) -> Result<PciRoot, PciError> {
        let mcfg = acpi_manager()
            .tables()
            .expect("get acpi_manager table error")
            .find_table::<Mcfg>()
            .map_err(|_| PciError::McfgTableNotFound)?;
        for mcfg_entry in mcfg.entries() {
            if mcfg_entry.pci_segment_group == segement {
                return Ok(PciRoot {
                    physical_address_base: PhysAddr::new(mcfg_entry.base_address as usize),
                    mmio_guard: None,
                    segement_group_number: segement,
                    bus_begin: mcfg_entry.bus_number_start,
                    bus_end: mcfg_entry.bus_number_end,
                });
            }
        }
        return Err(PciError::SegmentNotFound);
    }
}
