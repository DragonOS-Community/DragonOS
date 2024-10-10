use crate::mm::PhysAddr;

use super::{
    pci::{PciCam, SegmentGroupNumber},
    root::{pci_root_manager, PciRoot},
};

#[inline(always)]
pub fn pci_ecam_root_info_manager() -> &'static EcamRootInfoManager {
    &EcamRootInfoManager
}

/// Ecam pci root info
#[derive(Clone, Copy)]
pub struct EcamRootInfo {
    pub segement_group_number: SegmentGroupNumber,
    pub bus_begin: u8,
    pub bus_end: u8,
    pub physical_address_base: PhysAddr,
}

impl EcamRootInfo {
    pub fn new(
        segement_group_number: SegmentGroupNumber,
        bus_begin: u8,
        bus_end: u8,
        physical_address_base: PhysAddr,
    ) -> Self {
        Self {
            segement_group_number,
            bus_begin,
            bus_end,
            physical_address_base,
        }
    }
}

pub struct EcamRootInfoManager;

impl EcamRootInfoManager {
    /// # add_ecam_root_info - 向EcamRootInfoManager添加EcamRootInfo
    ///
    /// 将一个新的EcamRootInfo添加到EcamRootInfoManager中。
    ///
    /// ## 参数
    ///
    /// - `ecam_root_info`: EcamRootInfo - 要添加的EcamRootInfo实例
    pub fn add_ecam_root_info(&self, ecam_root_info: EcamRootInfo) {
        if !pci_root_manager().has_root(ecam_root_info.segement_group_number) {
            let root = PciRoot::new(
                ecam_root_info.segement_group_number,
                PciCam::Ecam,
                ecam_root_info.physical_address_base,
                ecam_root_info.bus_begin,
                ecam_root_info.bus_end,
            );

            if let Err(err) = root {
                kerror!("add_ecam_root_info(): failed to create PciRoot: {:?}", err);
                return;
            }

            pci_root_manager().add_pci_root(root.unwrap());
        } else {
            kwarn!(
                "add_ecam_root_info(): root {} already exists",
                ecam_root_info.segement_group_number
            );
        }
    }
}
