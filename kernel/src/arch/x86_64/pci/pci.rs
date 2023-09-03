use crate::arch::TraitPciArch;
use crate::driver::acpi::acpi::mcfg_find_segment;
use crate::driver::pci::pci::{
    BusDeviceFunction, PciAddr, PciError, PciRoot, SegmentGroupNumber, PORT_PCI_CONFIG_ADDRESS,
    PORT_PCI_CONFIG_DATA,
};
use crate::include::bindings::bindings::{
    acpi_get_MCFG, acpi_iter_SDT, acpi_system_description_table_header_t, io_in32, io_out32,
};
use crate::mm::PhysAddr;

use core::ffi::c_void;
use core::ptr::NonNull;
pub struct X86_64PciArch {}
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
        let mut data: usize = 0;
        let data_point = &mut data;
        unsafe {
            acpi_iter_SDT(Some(acpi_get_MCFG), data_point as *mut usize as *mut c_void);
        };
        // 防止无PCIE的机器找不到MCFG Table导致的错误
        if data == 0 {
            return Err(PciError::McfgTableNotFound);
        }
        //kdebug!("{}",data);
        //loop{}
        let head = NonNull::new(data as *mut acpi_system_description_table_header_t).unwrap();
        let outcome = unsafe { mcfg_find_segment(head).as_ref() };
        for segmentgroupconfiguration in outcome {
            if segmentgroupconfiguration.segement_group_number == segement {
                return Ok(PciRoot {
                    physical_address_base: PhysAddr::new(
                        segmentgroupconfiguration.base_address as usize,
                    ),
                    mmio_guard: None,
                    segement_group_number: segement,
                    bus_begin: segmentgroupconfiguration.bus_begin,
                    bus_end: segmentgroupconfiguration.bus_end,
                });
            }
        }
        return Err(PciError::SegmentNotFound);
    }
}
