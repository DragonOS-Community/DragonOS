use raw_cpuid::CpuId;
use crate::{
    kerror,
    // libs::spinlock::{SpinLock, SpinLockGuard},
    syscall::SystemError,
};

use alloc::sync::Arc;
use crate::virt::kvm::GUEST_STACK_SIZE;
use crate::virt::kvm::KVM;
use self::vmx::vcpu::VmxVcpu;
use crate::virt::kvm::vcpu::Vcpu;
use alloc::boxed::Box;
mod vmx;

pub struct X86_64KVMArch;

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


    pub fn kvm_arch_dev_ioctl(cmd: u32, arg: usize) -> Result<usize, SystemError> {
        match cmd {
            _ => {
                kerror!("unknown kvm ioctl cmd: {}", cmd);
                return Err(SystemError::EINVAL);
            }
        }
    }

    pub fn kvm_arch_vcpu_create(id:u32) -> Result<Box<dyn Vcpu>, SystemError> {
        let mut current_kvm = KVM();
        let guest_stack = vec![0xCC; GUEST_STACK_SIZE];
        let mut vcpu = Box::new(
            VmxVcpu::new(
                id, 
                current_kvm.clone(), 
                guest_stack.as_ptr() as u64 + GUEST_STACK_SIZE as u64, 
                current_kvm.lock().mem_slots
            ).unwrap()
        );
        return Ok(vcpu);
    }
    
    
}
