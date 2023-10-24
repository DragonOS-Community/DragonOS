use crate::arch::kvm::vmx::vmcs::VmcsFields;
use crate::arch::kvm::vmx::vmx_asm_wrapper::{vmx_vmlaunch, vmx_vmread};
use crate::libs::mutex::Mutex;
use crate::virt::kvm::vm;
use crate::{
    kdebug,
    kerror,
    // libs::spinlock::{SpinLock, SpinLockGuard},
    syscall::SystemError,
};
use alloc::sync::Arc;
use core::arch::asm;
use raw_cpuid::CpuId;
// use crate::virt::kvm::guest_code;
use self::vmx::mmu::{kvm_mmu_setup, kvm_vcpu_mtrr_init};
use self::vmx::vcpu::VmxVcpu;
pub mod vmx;

#[derive(Default, Debug, Clone)]
pub struct X86_64KVMArch {
    // n_used_mmu_pages: u32,
    // n_requested_mmu_pages: u32,
    // n_max_mmu_pages: u32,
    // mmu_valid_gen: u64,
    // // mmu_page_hash:[],
    // active_mmu_pages: LinkedList<KvmMmuPage>, // 所有分配的mmu page都挂到active_mmu_pages上
    // zapped_obsolete_pages: LinkedList<KvmMmuPage>, // 释放的mmu page都挂到zapped_obsolete_pages上,一个全局的invalid_list
}

impl X86_64KVMArch {
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
        if let Some(fi) = cpuid.get_feature_info() {
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

    pub fn kvm_arch_vcpu_create(id: u32) -> Result<Arc<Mutex<VmxVcpu>>, SystemError> {
        // let guest_rip = current_kvm.lock().memslots[0].memslots[0].userspace_addr;
        let vcpu = VmxVcpu::new(id, vm(0).unwrap()).unwrap();
        return Ok(Arc::new(Mutex::new(vcpu)));
    }

    pub fn kvm_arch_vcpu_setup(vcpu: &Mutex<VmxVcpu>) -> Result<(), SystemError> {
        kvm_vcpu_mtrr_init(vcpu)?;
        kvm_mmu_setup(vcpu);
        Ok(())
    }
    pub fn kvm_arch_vcpu_ioctl_run(_vcpu: &Mutex<VmxVcpu>) -> Result<(), SystemError> {
        match vmx_vmlaunch() {
            Ok(_) => {}
            Err(e) => {
                let vmx_err = vmx_vmread(VmcsFields::VMEXIT_INSTR_ERR as u32).unwrap();
                kdebug!("vmlaunch failed: {:?}", vmx_err);
                return Err(e);
            }
        }
        Ok(())
    }

    // pub fn kvm_arch_create_memslot(_slot: &mut KvmMemorySlot, _npages: u64) {

    // }

    // pub fn kvm_arch_commit_memory_region(
    //     _mem: &KvmUserspaceMemoryRegion,
    //     _new_slot: &KvmMemorySlot,
    //     _old_slot: &KvmMemorySlot,
    //     _change: KvmMemoryChange) {
    //         // let kvm = KVM();
    //         // let mut num_mmu_pages = 0;
    //         // if kvm.lock().arch.n_requested_mmu_pages == 0{
    // 	    //     num_mmu_pages = kvm_mmu_calculate_mmu_pages();
    //         // }
    //         // if num_mmu_pages != 0 {
    //         //     // kvm_mmu_change_mmu_pages(num_mmu_pages);
    //         // }
    // }
}

#[no_mangle]
pub extern "C" fn guest_code() {
    kdebug!("guest_code");
    loop {
        unsafe {
            asm!("mov rax, 0", "mov rcx, 0", "cpuid");
        }
        unsafe { asm!("nop") };
        kdebug!("guest_code");
    }
}
