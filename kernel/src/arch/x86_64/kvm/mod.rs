use core::arch::asm;

use alloc::collections::LinkedList;
use raw_cpuid::CpuId;
use crate::virt::kvm::host_mem::{KvmMemorySlot, KvmUserspaceMemoryRegion, KvmMemoryChange};
use crate::{
    kerror, kdebug,
    // libs::spinlock::{SpinLock, SpinLockGuard},
    syscall::SystemError,
};
// use crate::virt::kvm::guest_code;
use crate::virt::kvm::{HOST_STACK_SIZE, GUEST_STACK_SIZE};
use crate::virt::kvm::KVM;
use self::vmx::mmu::{kvm_mmu_calculate_mmu_pages, KvmMmuPage};
use self::vmx::vcpu::VmxVcpu;
use crate::virt::kvm::vcpu::Vcpu;
use alloc::boxed::Box;
pub mod vmx;

pub const KVM_MMU_HASH_SHIFT: u32 = 10;
pub const KVM_NUM_MMU_PAGES: u32 = 1 << KVM_MMU_HASH_SHIFT;
pub const KVM_NR_MEM_OBJS:u32 = 40;

pub struct X86_64KVMArch {
    n_used_mmu_pages: u32,
    n_requested_mmu_pages: u32, 
    n_max_mmu_pages: u32,
    mmu_valid_gen: u64,
    // mmu_page_hash:[],
    active_mmu_pages: LinkedList<KvmMmuPage>, // 所有分配的mmu page都挂到active_mmu_pages上
    zapped_obsolete_pages: LinkedList<KvmMmuPage>, // 释放的mmu page都挂到zapped_obsolete_pages上,一个全局的invalid_list
}


impl X86_64KVMArch{
    /// @brief 查看CPU是否支持虚拟化
    pub fn kvm_arch_cpu_supports_vm() -> Result<(), SystemError> {
        let cpuid = CpuId::new();
        // Check to see if CPU is Intel (“GenuineIntel”).
        if let Some(vi) = cpuid.get_vendor_info() {
            if vi.as_str() != "GenuineIntel" {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
        }
        // Check processor supports for Virtual Machine Extension (VMX) technology 
        // CPUID.1:ECX.VMX[bit 5] = 1 (Intel Manual: 24.6 Discovering Support for VMX) 
        if let Some(fi) = cpuid.get_feature_info(){
            if !fi.has_vmx() {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
        }
        Ok(())
    }

    /// @brief 初始化KVM
    pub fn kvm_arch_init() -> Result<(), SystemError> {
        Ok(())
    }


    pub fn kvm_arch_dev_ioctl(cmd: u32, _arg: usize) -> Result<usize, SystemError> {
        match cmd {
            _ => {
                kerror!("unknown kvm ioctl cmd: {}", cmd);
                return Err(SystemError::EINVAL);
            }
        }
    }

    pub fn kvm_arch_vcpu_create(id:u32) -> Result<Box<dyn Vcpu>, SystemError> {
        let current_kvm = KVM();
        let guest_stack = vec![0xCC; GUEST_STACK_SIZE];
        let host_stack = vec![0xCC; HOST_STACK_SIZE];
        let vcpu = Box::new(
            VmxVcpu::new(
                id, 
                current_kvm.clone(), 
                (host_stack.as_ptr() as u64) + HOST_STACK_SIZE  as u64,
                guest_stack.as_ptr() as u64 + GUEST_STACK_SIZE as u64, 
                guest_code as *const () as u64
            ).unwrap()
        );
        return Ok(vcpu);
    }
    
    pub fn kvm_arch_vcpu_setup(vcpu: &mut dyn Vcpu) -> Result<(), SystemError> {
        // TODO: kvm_vcpu_mtrr_init(vcpu);
        kvm_mmu_setup(vcpu);
        Ok(())
    }
    pub fn kvm_arch_create_memslot(slot: &mut KvmMemorySlot, npages: u64) {

    }

    pub fn kvm_arch_commit_memory_region(
        mem: &KvmUserspaceMemoryRegion, 
        new_slot: &KvmMemorySlot, 
        old_slot: &KvmMemorySlot,
        change: KvmMemoryChange) {
            let kvm = KVM();
            let mut num_mmu_pages = 0;
            if !kvm.arch.n_requested_mmu_pages {
		        num_mmu_pages = kvm_mmu_calculate_mmu_pages();
            }
            if num_mmu_pages {
                kvm_mmu_change_mmu_pages(num_mmu_pages);
            }
    }
}

#[no_mangle]
pub extern "C" fn guest_code(){
    kdebug!("guest_code");
    loop {
        unsafe {asm!(
            "mov rax, 0",
            "mov rcx, 0",
            "cpuid"
        );}
        unsafe {asm!("nop")};
        kdebug!("guest_code");
    }
}