use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    arch::TraitPciArch,
    driver::{
        open_firmware::fdt::open_firmware_fdt_driver,
        pci::pci::{pci_init, BusDeviceFunction, PciAddr},
    },
    init::initcall::INITCALL_SUBSYS,
    mm::PhysAddr,
};

use self::pci_host_ecam::pci_host_ecam_driver_init;

mod pci_host_ecam;

pub struct RiscV64PciArch;
impl TraitPciArch for RiscV64PciArch {
    fn read_config(_bus_device_function: &BusDeviceFunction, _offset: u8) -> u32 {
        unimplemented!("RiscV64PciArch::read_config")
    }

    fn write_config(_bus_device_function: &BusDeviceFunction, _offset: u8, _data: u32) {
        unimplemented!("RiscV64pci_root_0().write_config")
    }

    fn address_pci_to_physical(pci_address: PciAddr) -> crate::mm::PhysAddr {
        return PhysAddr::new(pci_address.data());
    }
}

#[unified_init(INITCALL_SUBSYS)]
fn riscv_pci_init() -> Result<(), SystemError> {
    let fdt = open_firmware_fdt_driver().fdt_ref()?;

    pci_host_ecam_driver_init(&fdt)?;
    pci_init();

    return Ok(());
}
