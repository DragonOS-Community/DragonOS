use fdt::{node::FdtNode, Fdt};
use log::debug;
use system_error::SystemError;

use crate::{
    driver::{
        open_firmware::fdt::open_firmware_fdt_driver,
        pci::ecam::{pci_ecam_root_info_manager, EcamRootInfo},
    },
    mm::PhysAddr,
};

pub(super) fn pci_host_ecam_driver_init(fdt: &Fdt<'_>) -> Result<(), SystemError> {
    let do_check = |node: FdtNode| -> Result<(), SystemError> {
        let reg = node
            .reg()
            .ok_or(SystemError::EINVAL)?
            .next()
            .ok_or(SystemError::EINVAL)?;
        let paddr = reg.starting_address as usize;
        let size = reg.size.unwrap_or(0);
        let bus_range: &[u8] = node.property("bus-range").ok_or(SystemError::EINVAL)?.value;

        let (bus_begin, bus_end) = match bus_range.len() {
            8 => (
                u32::from_be_bytes(bus_range[0..4].try_into().unwrap()),
                u32::from_be_bytes(bus_range[4..8].try_into().unwrap()),
            ),
            _ => panic!("Unexpected bus-range length"),
        };

        let segement_group_number: &[u8] = node
            .property("linux,pci-domain")
            .ok_or(SystemError::EINVAL)?
            .value;

        let segement_group_number = match segement_group_number.len() {
            4 => u32::from_be_bytes(segement_group_number[0..4].try_into().unwrap()),
            _ => panic!("Unexpected linux,pci-domain length"),
        };

        debug!(
            "pci_host_ecam_driver_init(): {} paddr: {:#x} size: {:#x} bus-range: {}-{} segement_group_number: {}",
            node.name,
            paddr,
            size,
            bus_begin,
            bus_end,
            segement_group_number
        );

        pci_ecam_root_info_manager().add_ecam_root_info(EcamRootInfo::new(
            segement_group_number.try_into().unwrap(),
            bus_begin as u8,
            bus_end as u8,
            PhysAddr::new(paddr),
        ));

        Ok(())
    };

    for node in open_firmware_fdt_driver().find_node_by_compatible(&fdt, "pci-host-ecam-generic") {
        if let Err(err) = do_check(node) {
            debug!(
                "pci_host_ecam_driver_init(): check {} error: {:?}",
                node.name, err
            );
        }
    }

    return Ok(());
}
