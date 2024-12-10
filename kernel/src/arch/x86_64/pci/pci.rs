use crate::arch::io::PortIOArch;
use crate::arch::{CurrentPortIOArch, TraitPciArch};
use crate::driver::acpi::acpi_manager;
use crate::driver::pci::ecam::{pci_ecam_root_info_manager, EcamRootInfo};
use crate::driver::pci::pci::{
    pci_init, BusDeviceFunction, PciAddr, PciCam, PciError, PORT_PCI_CONFIG_ADDRESS,
    PORT_PCI_CONFIG_DATA,
};
use crate::driver::pci::root::{pci_root_manager, PciRoot};
use crate::init::initcall::INITCALL_SUBSYS;
use crate::mm::PhysAddr;

use acpi::mcfg::Mcfg;
use log::warn;
use system_error::SystemError;
use unified_init::macros::unified_init;

pub struct X86_64PciArch;

impl X86_64PciArch {
    /// # 在早期引导阶段直接访问PCI配置空间的函数
    /// 参考：https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/pci/early.c?fi=read_pci_config_byte#19
    fn read_config_early(bus: u8, slot: u8, func: u8, offset: u8) -> u8 {
        unsafe {
            CurrentPortIOArch::out32(
                PORT_PCI_CONFIG_ADDRESS,
                0x80000000
                    | ((bus as u32) << 16)
                    | ((slot as u32) << 11)
                    | ((func as u32) << 8)
                    | offset as u32,
            );
        }
        let value = unsafe { CurrentPortIOArch::in8(PORT_PCI_CONFIG_DATA + (offset & 3) as u16) };
        return value;
    }
}

impl TraitPciArch for X86_64PciArch {
    fn read_config(bus_device_function: &BusDeviceFunction, offset: u8) -> u32 {
        // 构造pci配置空间地址
        let address = ((bus_device_function.bus as u32) << 16)
            | ((bus_device_function.device as u32) << 11)
            | ((bus_device_function.function as u32 & 7) << 8)
            | (offset & 0xfc) as u32
            | (0x80000000);
        let ret = unsafe {
            CurrentPortIOArch::out32(PORT_PCI_CONFIG_ADDRESS, address);
            let temp = CurrentPortIOArch::in32(PORT_PCI_CONFIG_DATA);
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
            CurrentPortIOArch::out32(PORT_PCI_CONFIG_ADDRESS, address);
            // 写入数据
            CurrentPortIOArch::out32(PORT_PCI_CONFIG_DATA, data);
        }
    }

    fn address_pci_to_physical(pci_address: PciAddr) -> PhysAddr {
        return PhysAddr::new(pci_address.data());
    }
}

#[unified_init(INITCALL_SUBSYS)]
fn x86_64_pci_init() -> Result<(), SystemError> {
    if discover_ecam_root().is_err() {
        // ecam初始化失败，使用portio访问pci配置空间
        // 参考：https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/pci/broadcom_bus.c#27
        let bus_begin = X86_64PciArch::read_config_early(0, 0, 0, 0x44);
        let bus_end = X86_64PciArch::read_config_early(0, 0, 0, 0x45);

        if !pci_root_manager().has_root(bus_begin as u16) {
            let root = PciRoot::new(None, PciCam::Portiocam, bus_begin, bus_end);
            pci_root_manager().add_pci_root(root.unwrap());
        } else {
            warn!("x86_64_pci_init(): pci_root_manager {}", bus_begin);
        }
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
