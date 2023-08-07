use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::syscall::SystemError;
use crate::virt::kvm::Vcpu;
use core::arch::asm;
use crate::{kdebug};

pub struct Hypervisor {
    sys_fd: u32,	/* For system ioctls(), i.e. /dev/kvm */
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
pub struct KvmUserspaceMemoryRegion {
    pub slot: u32,
    pub flags: u32,
    pub guest_phys_addr: u64,
    pub memory_size: u64,
    pub userspace_addr: u64,
}


impl Hypervisor {
    pub fn new(sys_fd:u32, nr_vcpus: u32, host_stack: u64, mem_slots: u64) -> Result<Self, SystemError> {
        let mut vcpu = Vec::new();
        // for i in 0..nr_vcpus {
        //     vcpu.push(Vcpu::new(i, Arc::new(hypervisor))?);
        // }
        // Allocate stack for vm-exit handlers and fill it with garbage data
        let mut instance = Self {
            sys_fd,
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

