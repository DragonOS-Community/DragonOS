// 导出 ahci 相关的 module
pub mod ahci_inode;
pub mod ahcidisk;
pub mod hba;

use crate::io::device::BlockDevice;
// 依赖的rust工具包
use crate::filesystem::devfs::devfs_register;
use crate::io::disk_info::BLK_GF_AHCI;
use crate::kerror;
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::mm::virt_2_phys;
use crate::{
    driver::disk::ahci::{
        ahcidisk::LockedAhciDisk,
        hba::HbaMem,
        hba::{HbaPort, HbaPortType},
    },
    kdebug,
};
use ahci_inode::LockedAhciInode;
use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::compiler_fence;

// 依赖的C结构体/常量
use crate::include::bindings::bindings::{
    ahci_cpp_init, pci_device_structure_general_device_t, pci_device_structure_header_t,
    AHCI_MAPPING_BASE, MAX_AHCI_DEVICES, PAGE_2M_MASK,
};

// 仅module内可见 全局数据区  hbr_port, disks
static LOCKED_HBA_MEM_LIST: SpinLock<Vec<&mut HbaMem>> = SpinLock::new(Vec::new());
static LOCKED_DISKS_LIST: SpinLock<Vec<Arc<LockedAhciDisk>>> = SpinLock::new(Vec::new());

/* TFES - Task File Error Status */
#[allow(non_upper_case_globals)]
pub const HBA_PxIS_TFES: u32 = 1 << 30;

#[no_mangle]
pub extern "C" fn ahci_init() -> i32 {
    let r = ahci_rust_init();
    if r.is_ok() {
        return 0;
    } else {
        return r.unwrap_err();
    }
}
/// @brief: 初始化 ahci
pub fn ahci_rust_init() -> Result<(), i32> {
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    let mut ahci_dev_counts: u32 = 0;
    let mut ahci_devs: [*mut pci_device_structure_header_t; MAX_AHCI_DEVICES as usize] =
        [0 as *mut pci_device_structure_header_t; MAX_AHCI_DEVICES as usize];
    let mut gen_devs: [*mut pci_device_structure_general_device_t; MAX_AHCI_DEVICES as usize] =
        [0 as *mut pci_device_structure_general_device_t; MAX_AHCI_DEVICES as usize];

    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    unsafe {
        // 单线程 init， 所以写 ahci_devs 全局变量不会出错？
        ahci_cpp_init(
            (&mut ahci_dev_counts) as *mut u32,
            (&mut ahci_devs) as *mut *mut pci_device_structure_header_t,
            (&mut gen_devs) as *mut *mut pci_device_structure_general_device_t,
        );
    }
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    // 全局数据 - 列表
    let mut disks_list = LOCKED_DISKS_LIST.lock();

    for i in 0..(ahci_dev_counts as usize) {
        // 对于每一个ahci控制器分配一块空间 (目前slab algorithm最大支持1MB)
        let ahci_port_base_vaddr =
            Box::leak(Box::new([0u8; (1 << 20) as usize])) as *mut u8 as usize;
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        // 获取全局引用 : 计算 HBA_MEM 的虚拟地址 依赖于C的宏定义 cal_HBA_MEM_VIRT_ADDR
        let virt_addr = AHCI_MAPPING_BASE as usize + unsafe { (*gen_devs[i]).BAR5 as usize }
            - (unsafe { (*gen_devs[0]).BAR5 as usize } & PAGE_2M_MASK as usize);
        compiler_fence(core::sync::atomic::Ordering::SeqCst);

        // 最后把这个引用列表放入到全局列表
        let mut hba_mem_list = LOCKED_HBA_MEM_LIST.lock();
        hba_mem_list.push(unsafe { (virt_addr as *mut HbaMem).as_mut().unwrap() });
        let pi = volatile_read!(hba_mem_list[i].pi);
        drop(hba_mem_list);
        compiler_fence(core::sync::atomic::Ordering::SeqCst);

        // 初始化所有的port
        let mut id = 0;
        for j in 0..32 {
            if (pi >> j) & 1 > 0 {
                let mut hba_mem_list = LOCKED_HBA_MEM_LIST.lock();
                let tp = hba_mem_list[i].ports[j].check_type();
                match tp {
                    HbaPortType::None => {
                        kdebug!("<ahci_rust_init> Find a None type Disk.");
                    }
                    HbaPortType::Unknown(err) => {
                        kdebug!("<ahci_rust_init> Find a Unknown({:?}) type Disk.", err);
                    }
                    _ => {
                        kdebug!("<ahci_rust_init> Find a {:?} type Disk.", tp);

                        // 计算地址
                        let fb = virt_2_phys(ahci_port_base_vaddr + (32 << 10) + (j << 8));
                        let clb = virt_2_phys(ahci_port_base_vaddr + (j << 10));
                        compiler_fence(core::sync::atomic::Ordering::SeqCst);
                        let ctbas = (0..32)
                            .map(|x| {
                                virt_2_phys(
                                    ahci_port_base_vaddr + (40 << 10) + (j << 13) + (x << 8),
                                ) as u64
                            })
                            .collect::<Vec<_>>();

                        // 初始化 port
                        hba_mem_list[i].ports[j].init(clb as u64, fb as u64, &ctbas);

                        // 释放锁
                        drop(hba_mem_list);
                        compiler_fence(core::sync::atomic::Ordering::SeqCst);

                        // 创建 disk
                        disks_list.push(LockedAhciDisk::new(
                            format!("ahci_disk_{}", id),
                            BLK_GF_AHCI,
                            i as u8,
                            j as u8,
                        )?);
                        id += 1; // ID 从0开始

                        kdebug!("start register ahci device");

                        // 挂载到devfs上面去
                        let ret = devfs_register(
                            format!("ahci_{}", id).as_str(),
                            LockedAhciInode::new(disks_list.last().unwrap().clone()),
                        );
                        if let Err(err) = ret {
                            kerror!(
                                "Ahci_{} ctrl = {}, port = {} failed to register, error code = {}",
                                id,
                                i,
                                j,
                                err
                            );
                        }
                    }
                }
            }
        }
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
pub fn get_disks_by_name(name: String) -> Result<Arc<LockedAhciDisk>, i32> {
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    let disks_list: SpinLockGuard<Vec<Arc<LockedAhciDisk>>> = LOCKED_DISKS_LIST.lock();
    for i in 0..disks_list.len() {
        if disks_list[i].0.lock().name == name {
            return Ok(disks_list[i].clone());
        }
    }
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    return Err(-1);
}

/// @brief: 通过 ctrl_num 和 port_num 获取 port
pub fn _port(ctrl_num: u8, port_num: u8) -> &'static mut HbaPort {
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    let list: SpinLockGuard<Vec<&mut HbaMem>> = LOCKED_HBA_MEM_LIST.lock();
    let port: &HbaPort = &list[ctrl_num as usize].ports[port_num as usize];
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    return unsafe { (port as *const HbaPort as *mut HbaPort).as_mut().unwrap() };
}

/// @brief: 测试函数
pub fn __test_ahci() {
    let _res = ahci_rust_init();
    let disk: Arc<LockedAhciDisk> = get_disks_by_name("ahci_disk_0".to_string()).unwrap();
    #[deny(overflowing_literals)]
    let mut buf = [0u8; 3000usize];

    for i in 0..2000 {
        buf[i] = i as u8;
    }

    let _dd = disk;

    // 测试1, 写两个块,读4个块
    // _dd.write_at(123, 2, &buf).unwrap();
    let mut read_buf = [0u8; 3000usize];
    _dd.read_at(122, 4, &mut read_buf).unwrap();

    // 测试2, 只读写一个字节
    for i in 0..512 {
        buf[i] = 233;
    }
    // _dd.write_at(123, 2, &buf).unwrap();
    let mut read_buf2 = [0u8; 3000usize];
    _dd.read_at(122, 4, &mut read_buf2).unwrap();
}
