// 导出 ahci 相关的 module
pub mod ahci_inode;
pub mod ahcidisk;
pub mod hba;

use crate::arch::MMArch;
use crate::driver::base::block::disk_info::BLK_GF_AHCI;
use crate::driver::block::cache::cached_block_device::BlockCache;
// 依赖的rust工具包
use crate::driver::pci::pci::{
    get_pci_device_structure_mut, PciDeviceStructure, PCI_DEVICE_LINKEDLIST,
};
use crate::filesystem::devfs::devfs_register;

use crate::driver::disk::ahci::{
    ahcidisk::LockedAhciDisk,
    hba::HbaMem,
    hba::{HbaPort, HbaPortType},
};
use crate::libs::rwlock::RwLockWriteGuard;
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::mm::{MemoryManagementArch, VirtAddr};
use ahci_inode::LockedAhciInode;
use alloc::{boxed::Box, collections::LinkedList, format, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::compiler_fence;
use log::{debug, error};
use system_error::SystemError;

// 仅module内可见 全局数据区  hbr_port, disks
static LOCKED_HBA_MEM_LIST: SpinLock<Vec<&mut HbaMem>> = SpinLock::new(Vec::new());
static LOCKED_DISKS_LIST: SpinLock<Vec<Arc<LockedAhciDisk>>> = SpinLock::new(Vec::new());

const AHCI_CLASS: u8 = 0x1;
const AHCI_SUBCLASS: u8 = 0x6;

/* TFES - Task File Error Status */
#[allow(non_upper_case_globals)]
pub const HBA_PxIS_TFES: u32 = 1 << 30;

/// @brief 寻找所有的ahci设备
/// @param list 链表的写锁
/// @return Result<Vec<&'a mut Box<dyn PciDeviceStructure>>, SystemError>   成功则返回包含所有ahci设备结构体的可变引用的链表，失败则返回err
fn ahci_device_search<'a>(
    list: &'a mut RwLockWriteGuard<'_, LinkedList<Box<dyn PciDeviceStructure>>>,
) -> Result<Vec<&'a mut Box<dyn PciDeviceStructure>>, SystemError> {
    let result = get_pci_device_structure_mut(list, AHCI_CLASS, AHCI_SUBCLASS);

    if result.is_empty() {
        return Err(SystemError::ENODEV);
    }

    return Ok(result);
}

/// @brief: 初始化 ahci
pub fn ahci_init() -> Result<(), SystemError> {
    let mut list = PCI_DEVICE_LINKEDLIST.write();
    let ahci_device = ahci_device_search(&mut list)?;
    // 全局数据 - 列表
    let mut disks_list = LOCKED_DISKS_LIST.lock();

    for device in ahci_device {
        let standard_device = device.as_standard_device_mut().unwrap();
        standard_device.bar_ioremap();
        // 对于每一个ahci控制器分配一块空间
        let ahci_port_base_vaddr =
            Box::leak(Box::new([0u8; (1 << 20) as usize])) as *mut u8 as usize;
        let virtaddr = standard_device
            .bar()
            .ok_or(SystemError::EACCES)?
            .get_bar(5)
            .or(Err(SystemError::EACCES))?
            .virtual_address()
            .unwrap();
        // 最后把这个引用列表放入到全局列表
        let mut hba_mem_list = LOCKED_HBA_MEM_LIST.lock();
        //这里两次unsafe转引用规避rust只能有一个可变引用的检查，提高运行速度
        let hba_mem = unsafe { (virtaddr.data() as *mut HbaMem).as_mut().unwrap() };
        hba_mem_list.push(unsafe { (virtaddr.data() as *mut HbaMem).as_mut().unwrap() });
        let pi = volatile_read!(hba_mem.pi);
        let hba_mem_index = hba_mem_list.len() - 1;
        drop(hba_mem_list);
        // 初始化所有的port
        let mut id = 0;
        for j in 0..32 {
            if (pi >> j) & 1 > 0 {
                let hba_mem_list = LOCKED_HBA_MEM_LIST.lock();
                let hba_mem_port = &mut hba_mem.ports[j];
                let tp = hba_mem_port.check_type();
                match tp {
                    HbaPortType::None => {
                        debug!("<ahci_rust_init> Find a None type Disk.");
                    }
                    HbaPortType::Unknown(err) => {
                        debug!("<ahci_rust_init> Find a Unknown({:?}) type Disk.", err);
                    }
                    _ => {
                        debug!("<ahci_rust_init> Find a {:?} type Disk.", tp);

                        // 计算地址
                        let fb = unsafe {
                            MMArch::virt_2_phys(VirtAddr::new(
                                ahci_port_base_vaddr + (32 << 10) + (j << 8),
                            ))
                        }
                        .unwrap()
                        .data();
                        let clb = unsafe {
                            MMArch::virt_2_phys(VirtAddr::new(ahci_port_base_vaddr + (j << 10)))
                                .unwrap()
                                .data()
                        };
                        let ctbas = (0..32)
                            .map(|x| unsafe {
                                MMArch::virt_2_phys(VirtAddr::new(
                                    ahci_port_base_vaddr + (40 << 10) + (j << 13) + (x << 8),
                                ))
                                .unwrap()
                                .data() as u64
                            })
                            .collect::<Vec<_>>();

                        // 初始化 port
                        hba_mem_port.init(clb as u64, fb as u64, &ctbas);
                        drop(hba_mem_list);
                        compiler_fence(core::sync::atomic::Ordering::SeqCst);
                        // 创建 disk
                        disks_list.push(LockedAhciDisk::new(
                            format!("ahci_disk_{}", id),
                            BLK_GF_AHCI,
                            hba_mem_index as u8,
                            j as u8,
                        )?);
                        id += 1; // ID 从0开始

                        debug!("start register ahci device");

                        // 挂载到devfs上面去
                        let ret = devfs_register(
                            format!("ahci_{}", id).as_str(),
                            LockedAhciInode::new(disks_list.last().unwrap().clone()),
                        );
                        if let Err(err) = ret {
                            error!(
                                "Ahci_{} ctrl = {}, port = {} failed to register, error code = {:?}",
                                id,
                                hba_mem_index as u8,
                                j,
                                err
                            );
                        }
                    }
                }
            }
        }
        BlockCache::init();
    }

    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    return Ok(());
}

/// @brief: 获取所有的 disk
#[allow(dead_code)]
pub fn disks() -> Vec<Arc<LockedAhciDisk>> {
    let disks_list = LOCKED_DISKS_LIST.lock();
    return disks_list.clone();
}

/// @brief: 通过 name 获取 disk
pub fn get_disks_by_name(name: String) -> Result<Arc<LockedAhciDisk>, SystemError> {
    let disks_list: SpinLockGuard<Vec<Arc<LockedAhciDisk>>> = LOCKED_DISKS_LIST.lock();
    let result = disks_list
        .iter()
        .find(|x| x.0.lock().name == name)
        .ok_or(SystemError::ENXIO)?
        .clone();
    return Ok(result);
}

/// @brief: 通过 ctrl_num 和 port_num 获取 port
fn _port(ctrl_num: u8, port_num: u8) -> &'static mut HbaPort {
    let list: SpinLockGuard<Vec<&mut HbaMem>> = LOCKED_HBA_MEM_LIST.lock();
    let port: &HbaPort = &list[ctrl_num as usize].ports[port_num as usize];

    return unsafe { (port as *const HbaPort as *mut HbaPort).as_mut().unwrap() };
}
