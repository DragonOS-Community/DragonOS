use log::{error, warn};

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
#[derive(Clone, Debug, Copy)]
pub struct EcamRootInfo {
    /// 段组号
    pub segment_group_number: SegmentGroupNumber,
    /// 该分组中的最小bus
    pub bus_begin: u8,
    /// 该分组中的最大bus
    pub bus_end: u8,
    /// 物理基地址       
    pub physical_address_base: PhysAddr,
}

impl EcamRootInfo {
    pub fn new(
        segment_group_number: SegmentGroupNumber,
        bus_begin: u8,
        bus_end: u8,
        physical_address_base: PhysAddr,
    ) -> Self {
        let ecam_root_info = Self {
            segment_group_number,
            bus_begin,
            bus_end,
            physical_address_base,
        };
        return ecam_root_info;
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
        if !pci_root_manager().has_root(ecam_root_info.segment_group_number) {
            let root = PciRoot::new(
                Some(ecam_root_info),
                PciCam::Ecam,
                ecam_root_info.bus_begin,
                ecam_root_info.bus_end,
            );

            if let Err(err) = root {
                error!("add_ecam_root_info(): failed to create PciRoot: {:?}", err);
                return;
            }

            pci_root_manager().add_pci_root(root.unwrap());
        } else {
            warn!(
                "add_ecam_root_info(): root {} already exists",
                ecam_root_info.segment_group_number
            );
        }
    }
}
