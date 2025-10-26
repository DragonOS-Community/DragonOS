use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    arch::TraitPciArch,
    driver::{
        open_firmware::fdt::open_firmware_fdt_driver,
        pci::pci::{BusDeviceFunction, PciAddr},
    },
    init::initcall::INITCALL_SUBSYS,
    mm::PhysAddr,
};

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
    // vf2 不需要, 事实上 qemu 也不使用 pci，设备都是使用 mmio
    // 因此其实初始化这个没有太大的意义, 先注释掉
    // TODO: 如果取消注释且启用 vf2 平台, 那么需要补充 vf2 的 pcie 驱动

    // let fdt = open_firmware_fdt_driver().fdt_ref()?;

    // pci_host_ecam_driver_init(&fdt)?;
    // pci_init();

    return Ok(());
}
