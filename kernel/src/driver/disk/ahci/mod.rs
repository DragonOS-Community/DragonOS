#[macro_use]
pub mod volatile_macro;
// 导出 ahci 相关的 module
pub mod ahcidisk;
pub mod hba;

// 依赖的rust工具包
use crate::driver::disk::ahci::{
    ahcidisk::LockedAhciDisk,
    hba::HbaMem,
    hba::{HbaPort, HbaPortType},
};
use crate::io::disk_info::BLK_GF_AHCI;
use crate::libs::spinlock::SpinLock;
use crate::{kerror, print};
use alloc::boxed::Box;
use alloc::{format, string::String, sync::Arc, vec::Vec};

// 依赖的C结构体/常量
use crate::include::bindings::bindings::{
    ahci_cpp_init, pci_device_structure_general_device_t, pci_device_structure_header_t,
    AHCI_MAPPING_BASE, MAX_AHCI_DEVICES, PAGE_2M_MASK, PAGE_OFFSET,
};

// 仅module内可见 全局数据区  hbr_port, disks
static LOCKED_HBA_MEM_LIST: SpinLock<Vec<&mut HbaMem>> = SpinLock::new(Vec::new());
static LOCKED_DISKS_LIST: SpinLock<Vec<Arc<LockedAhciDisk>>> = SpinLock::new(Vec::new());

#[inline]
pub fn virt_2_phys(addr: usize) -> usize {
    PAGE_OFFSET as usize + addr
}

pub fn phys_2_virt(addr: usize) -> usize {
    addr - PAGE_OFFSET as usize
}

/// @brief: 初始化 ahci
pub fn ahci_rust_init() -> Result<(), i32> {
    let mut ahci_dev_counts: u32 = 0;
    let mut ahci_devs: [*mut pci_device_structure_header_t; MAX_AHCI_DEVICES as usize] =
        [0 as *mut pci_device_structure_header_t; MAX_AHCI_DEVICES as usize];
    let mut gen_devs: [*mut pci_device_structure_general_device_t; MAX_AHCI_DEVICES as usize] =
        [0 as *mut pci_device_structure_general_device_t; MAX_AHCI_DEVICES as usize];

    unsafe {
        // 单线程 init， 所以写 ahci_devs 全局变量不会出错？
        ahci_cpp_init(
            (&mut ahci_dev_counts) as *mut u32,
            (&mut ahci_devs) as *mut *mut pci_device_structure_header_t,
            (&mut gen_devs) as *mut *mut pci_device_structure_general_device_t,
        );
    }
    // 全局数据 - 列表
    let mut hba_mem_list = LOCKED_HBA_MEM_LIST.lock();
    let mut disks_list = LOCKED_DISKS_LIST.lock();

    for i in 0..(ahci_dev_counts as usize) {
        // 对于每一个ahci控制器分配一块空间 (目前slab algorithm最大支持1MB)
        let ahci_port_base_vaddr =
            Box::leak(Box::new([0u8; (1 << 20) as usize])) as *mut u8 as usize;

        // 获取全局引用 : 计算 HBA_MEM 的虚拟地址 依赖于C的宏定义 cal_HBA_MEM_VIRT_ADDR
        let virt_addr = AHCI_MAPPING_BASE as usize + unsafe { (*gen_devs[i]).BAR5 as usize }
            - (unsafe { (*gen_devs[0]).BAR5 as usize } & PAGE_2M_MASK as usize);
        let hba_mem = unsafe { (virt_addr as *mut HbaMem).as_mut().unwrap() };

        // 初始化所有的port
        let pi = v_read!(hba_mem.pi);
        let mut id = 0;
        for j in 0..32 {
            if (pi >> j) & 1 > 0 {
                let tp = hba_mem.ports[j].check_type();
                match tp {
                    HbaPortType::None => {
                        kerror!("<ahci_rust_init> Find a None type Disk.");
                    }
                    HbaPortType::Unknown(err) => {
                        kerror!("<ahci_rust_init> Find a Unknown({:?}) type Disk.", err);
                    }
                    _ => {
                        print!("<ahci_rust_init> Find a {:?} type Disk.", tp);

                        // 计算地址
                        let fb = virt_2_phys(ahci_port_base_vaddr + (32 << 10) + (j << 8));
                        let clb = virt_2_phys(ahci_port_base_vaddr + (j << 10));
                        let ctbas = (0..32)
                            .map(|x| {
                                virt_2_phys(
                                    ahci_port_base_vaddr + (40 << 10) + (j << 13) + (x << 8),
                                ) as u64
                            })
                            .collect::<Vec<_>>();

                        // 初始化 hba_mem.ports[j]
                        hba_mem.ports[j].init(clb as u64, fb as u64, &ctbas);

                        // 创建 disk
                        id += 1;
                        disks_list.push(LockedAhciDisk::new(
                            format!("ahci_disk_{}", id),
                            BLK_GF_AHCI,
                            // &mut hba_mem.ports[j],
                            i as u8,
                            j as u8,
                        )?);
                    }
                }
            }
        }

        // 最后把这个引用列表放入到全局列表
        hba_mem_list.push(hba_mem);
    }

    return Ok(());
}

/// @brief: 获取所有的 disk
pub fn disks() -> Vec<Arc<LockedAhciDisk>> {
    let disks_list = LOCKED_DISKS_LIST.lock();
    return disks_list.clone();
}

/// @brief: 通过 name 获取 disk
pub fn disks_by_name(name: String) -> Result<Arc<LockedAhciDisk>, i32> {
    let disks_list = LOCKED_DISKS_LIST.lock();

    for i in 0..disks_list.len() {
        if disks_list[i].0.lock().name == name {
            return Ok(disks_list[i].clone());
        }
    }

    return Err(-1);
}

/// @brief: 通过 ctrl_num 和 port_num 获取 port
pub fn _port(ctrl_num: u8, port_num: u8) -> &'static mut HbaPort {
    let mut list = LOCKED_HBA_MEM_LIST.lock();
    let port = &list[ctrl_num as usize].ports[port_num as usize];
    return unsafe { (port as *const HbaPort as *mut HbaPort).as_mut().unwrap() };
}


