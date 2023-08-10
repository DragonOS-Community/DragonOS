use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::syscall::SystemError;
use crate::virt::kvm::Vcpu;
// use crate::kdebug;

pub struct Hypervisor {
    // sys_fd: u32,	/* For system ioctls(), i.e. /dev/kvm */
    pub nr_vcpus: u32,  /* Number of cpus to run */
    pub vcpu: Vec<Box<dyn Vcpu>>,
    pub host_stack: u64,
    pub mem_slots: u64,
// 	vm_fd: u32,  	/* For VM ioctls() */
//     timerid: u32,   /* Posix timer for interrupts */
//     mem_slots: u32, /* for KVM_SET_USER_MEMORY_REGION */
//     ram_size: u64,  /* Guest memory size, in bytes */
//     ram_start: *u64,
//     ram_pagesize: u64,
//     mem_banks_lock: SpinLock<()>,
//     // mem_banks: Box<[kvm_mem_bank]>,

//     vm_state: u32,
}

#[repr(C)]
/// 通过这个结构可以将虚拟机的物理地址对应到用户进程的虚拟地址
/// 用来表示虚拟机的一段物理内存
pub struct KvmUserspaceMemoryRegion {
    pub slot: u32,              // ID号
    pub flags: u32,             // 表示该段内存属性
    pub guest_phys_addr: u64,   // 客户机物理地址
    pub memory_size: u64,       // 内存大小
    pub userspace_addr: u64,    // 对应的用户态进程中分配的虚拟机地址
}


impl Hypervisor {
    pub fn new(nr_vcpus: u32, host_stack: u64, mem_slots: u64) -> Result<Self, SystemError> {
        let vcpu = Vec::new();
        // for i in 0..nr_vcpus {
        //     vcpu.push(Vcpu::new(i, Arc::new(hypervisor))?);
        // }
        // Allocate stack for vm-exit handlers and fill it with garbage data
        let instance = Self {
            nr_vcpus,
            vcpu,
            host_stack,
            mem_slots,
        };
        Ok(instance)
    }

    pub fn set_user_memory_region(&mut self, kvm_mem_region: &KvmUserspaceMemoryRegion){
        self.mem_slots = kvm_mem_region.userspace_addr as u64;
    }
    // pub fn virtualize_cpu(&self) -> Result<(), SystemError> {
    //     let mut vcpu = self.vcpu[0].lock();
    //     vcpu.virtualize_cpu();
    // }
}

