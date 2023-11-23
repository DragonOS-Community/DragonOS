use crate::{
    arch::TraitPciArch,
    driver::pci::pci::{BusDeviceFunction, PciAddr, PciError, PciRoot, SegmentGroupNumber},
};

pub struct RiscV64PciArch;
impl TraitPciArch for RiscV64PciArch {
    fn read_config(bus_device_function: &BusDeviceFunction, offset: u8) -> u32 {
        unimplemented!("RiscV64PciArch::read_config")
    }

    fn write_config(bus_device_function: &BusDeviceFunction, offset: u8, data: u32) {
        unimplemented!("RiscV64PciArch::write_config")
    }

    fn address_pci_to_physical(pci_address: PciAddr) -> crate::mm::PhysAddr {
        unimplemented!("RiscV64PciArch::address_pci_to_physical")
    }

    fn ecam_root(segement: SegmentGroupNumber) -> Result<PciRoot, PciError> {
        unimplemented!("RiscV64PciArch::ecam_root")
    }
}
