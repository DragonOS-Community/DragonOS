use crate::{
    arch::TraitPciArch,
    driver::pci::pci::{BusDeviceFunction, PciAddr},
    mm::PhysAddr,
};

pub struct LoongArch64PciArch;
impl TraitPciArch for LoongArch64PciArch {
    fn read_config(_bus_device_function: &BusDeviceFunction, _offset: u8) -> u32 {
        unimplemented!("LoongArch64PciArch::read_config")
    }

    fn write_config(_bus_device_function: &BusDeviceFunction, _offset: u8, _data: u32) {
        unimplemented!("LoongArch64PciArch pci_root_0().write_config")
    }

    fn address_pci_to_physical(pci_address: PciAddr) -> crate::mm::PhysAddr {
        return PhysAddr::new(pci_address.data());
    }
}
