use raw_cpuid::CpuId;
use crate::{
    kerror,
    // libs::spinlock::{SpinLock, SpinLockGuard},
    syscall::SystemError,
};

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
}
