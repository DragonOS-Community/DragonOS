use raw_cpuid::CpuId;

pub struct Vmx;

impl Vmx {
    /// @brief 查看CPU是否支持虚拟化
    pub fn kvm_arch_cpu_supports_vm() -> bool {
        let cpuid = CpuId::new();
        // Check to see if CPU is Intel (“GenuineIntel”).
        if let Some(vi) = cpuid.get_vendor_info() {
            if vi.as_str() != "GenuineIntel" {
                return false;
            }
        }
        // Check processor supports for Virtual Machine Extension (VMX) technology
        // CPUID.1:ECX.VMX[bit 5] = 1 (Intel Manual: 24.6 Discovering Support for VMX)
        if let Some(fi) = cpuid.get_feature_info() {
            if !fi.has_vmx() {
                return false;
            }
        }
        return true;
    }
}

pub fn vmx_init() {}
