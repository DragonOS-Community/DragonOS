// 导出 ahci 相关的 module
pub mod ahcidisk;
pub mod hba;

// 依赖的rust工具包
use crate::driver::disk::ahci::{ahcidisk::LockedAhciDisk, hba::HbaMem, hba::HbaPortType};
use crate::io::disk_info::BLK_GF_AHCI;
use crate::libs::spinlock::SpinLock;
use crate::{kerror, print};
use alloc::{format, string::String, sync::Arc, vec::Vec};

// 依赖的C结构体/常量
use crate::include::bindings::bindings::{
    ahci_cpp_init, kmalloc, pci_device_structure_general_device_t, pci_device_structure_header_t,
    AHCI_MAPPING_BASE, MAX_AHCI_DEVICES, PAGE_2M_MASK, PAGE_OFFSET,
};

// 仅module内可见 全局数据区  hbr_mem, disks
static mut locked_hba_mem_list: SpinLock<Vec<&mut HbaMem>> = SpinLock::new(Vec::new());
static mut locked_disks_list: SpinLock<Vec<Arc<LockedAhciDisk>>> = SpinLock::new(Vec::new());

#[inline]
pub fn virt_2_phys(addr: usize) -> usize {
    PAGE_OFFSET as usize + addr
}

pub fn phys_2_virt(addr: usize) -> usize {
    addr - PAGE_OFFSET as usize
}

/// @brief: 初始化 ahci
pub fn ahci_rust_init() -> Result<(), i32> {
    let mut ahci_port_base_vaddr: usize = 0; // 端口映射base addr
    let mut ahci_port_base_phys_addr: usize = 0; // 端口映射的物理基地址（ahci控制器的参数的地址都是物理地址）
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

        // 全局数据 - 列表
        let mut hba_mem_list = locked_hba_mem_list.lock();
        let mut disks_list = locked_disks_list.lock();

        for i in 0..(ahci_dev_counts as usize) {
            // 对于每一个ahci控制器分配一块空间 (目前slab algorithm最大支持1MB)
            ahci_port_base_vaddr = kmalloc(1048576, 0) as usize;

            // 获取全局引用 : 计算 HBA_MEM 的虚拟地址 依赖于C的宏定义 cal_HBA_MEM_VIRT_ADDR
            let virt_addr = AHCI_MAPPING_BASE as usize + (*gen_devs[i]).BAR5 as usize
                - ((*gen_devs[0]).BAR5 as usize & PAGE_2M_MASK as usize);
            hba_mem_list.push(&mut *(virt_addr as *mut HbaMem));

            // 初始化所有的port
            let pi = hba_mem_list[i].pi.read();
            let mut id = 0;
            for j in 0..32 {
                if (pi >> j) & 1 > 0 {
                    let tp = hba_mem_list[i].ports[j].check_type();
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

                            // 初始化 port
                            hba_mem_list[i].ports[j].init(clb as u64, fb as u64, &ctbas);

                            // 创建 disk
                            id += 1;
                            disks_list.push(LockedAhciDisk::new(
                                format!("ahci_disk_{}", id),
                                BLK_GF_AHCI,
                                &mut hba_mem_list[i].ports[j],
                            )?);
                        }
                    }
                }
            }
        }
    }

    return Ok(());
}

/// @brief: 获取所有的 disk
pub fn disks() -> Vec<Arc<LockedAhciDisk>> {
    let disks_list = unsafe { locked_disks_list.lock() };
    return disks_list.clone();
}

/// @brief: 通过 name 获取 disk
pub fn disks_by_name(name: String) -> Result<Arc<LockedAhciDisk>, i32> {
    let disks_list = unsafe { locked_disks_list.lock() };

    for i in 0..disks_list.len() {
        if disks_list[i].name == name {
            return Ok(disks_list[i].clone());
        }
    }

    return Err(-1);
}
