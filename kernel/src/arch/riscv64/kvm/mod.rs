use system_error::SystemError;

#[derive(Debug, Clone, Default)]
pub struct RiscV64KVMArch {}

impl RiscV64KVMArch {
    /// @brief 查看CPU是否支持虚拟化
    pub fn kvm_arch_cpu_supports_vm() -> Result<(), SystemError> {
        unimplemented!("RiscV64KVMArch::kvm_arch_cpu_supports_vm")
    }

    /// @brief 初始化KVM
    pub fn kvm_arch_init() -> Result<(), SystemError> {
        Ok(())
    }

    pub fn kvm_arch_dev_ioctl(cmd: u32, _arg: usize) -> Result<usize, SystemError> {
        unimplemented!("RiscV64KVMArch::kvm_arch_dev_ioctl")
    }
}
