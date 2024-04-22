use crate::arch::TraitPciArch;
use crate::driver::acpi::acpi_manager;
use crate::driver::pci::ecam::{pci_ecam_root_info_manager, EcamRootInfo};
use crate::driver::pci::pci::{
    pci_init, BusDeviceFunction, PciAddr, PciError, PORT_PCI_CONFIG_ADDRESS, PORT_PCI_CONFIG_DATA,
};
use crate::include::bindings::bindings::{io_in32, io_out32};
use crate::init::initcall::INITCALL_SUBSYS;
use crate::kerror;
use crate::mm::PhysAddr;

use acpi::mcfg::Mcfg;
use system_error::SystemError;
use unified_init::macros::unified_init;

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
}

#[unified_init(INITCALL_SUBSYS)]
fn x86_64_pci_init() -> Result<(), SystemError> {
    if let Err(e) = discover_ecam_root() {
        kerror!("x86_64_pci_init(): discover_ecam_root error: {:?}", e);
    }
    pci_init();

    return Ok(());
}

/// # discover_ecam_root - 发现使用ECAM的PCI root device
///
/// 该函数用于从ACPI管理器获取MCFG表，并从中发现使用ECAM的PCI root device。
/// 然后，本函数将这些信息添加到pci_ecam_root_info_manager
///
/// ## 返回值
///
/// - Ok(()): 成功发现并添加了所有ECAM根信息
/// - Err(PciError): 在获取ACPI管理器表或发现MCFG表时发生错误
fn discover_ecam_root() -> Result<(), PciError> {
    let mcfg = acpi_manager()
        .tables()
        .expect("get acpi_manager table error")
        .find_table::<Mcfg>()
        .map_err(|_| PciError::McfgTableNotFound)?;
    for mcfg_entry in mcfg.entries() {
        pci_ecam_root_info_manager().add_ecam_root_info(EcamRootInfo::new(
            mcfg_entry.pci_segment_group,
            mcfg_entry.bus_number_start,
            mcfg_entry.bus_number_end,
            PhysAddr::new(mcfg_entry.base_address as usize),
        ));
    }

    Ok(())
}
