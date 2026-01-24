use x86::cpuid::{cpuid, CpuId, Hypervisor, HypervisorInfo};

pub const KVM_CPUID_FEATURES: u32 = 0x4000_0001;

pub const KVM_FEATURE_CLOCKSOURCE: u32 = 0;
pub const KVM_FEATURE_CLOCKSOURCE2: u32 = 3;
pub const KVM_FEATURE_CLOCKSOURCE_STABLE_BIT: u32 = 24;

pub const MSR_KVM_WALL_CLOCK: u32 = 0x11;
pub const MSR_KVM_SYSTEM_TIME: u32 = 0x12;
pub const MSR_KVM_WALL_CLOCK_NEW: u32 = 0x4b56_4d00;
pub const MSR_KVM_SYSTEM_TIME_NEW: u32 = 0x4b56_4d01;

pub fn kvm_para_get_hypervisor_info() -> Option<HypervisorInfo> {
    let c = CpuId::new().get_hypervisor_info()?;

    if c.identify() != Hypervisor::KVM {
        return None;
    }
    Some(c)
}

pub fn kvm_para_available() -> bool {
    kvm_para_get_hypervisor_info().is_some()
}

pub fn kvm_para_has_feature(bit: u32) -> bool {
    if !kvm_para_available() {
        return false;
    }

    let res = cpuid!(KVM_CPUID_FEATURES);
    (res.eax & (1u32 << bit)) != 0
}

pub fn kvm_clock_msrs() -> Option<(u32, u32)> {
    if kvm_para_has_feature(KVM_FEATURE_CLOCKSOURCE2) {
        return Some((MSR_KVM_SYSTEM_TIME_NEW, MSR_KVM_WALL_CLOCK_NEW));
    }
    if kvm_para_has_feature(KVM_FEATURE_CLOCKSOURCE) {
        return Some((MSR_KVM_SYSTEM_TIME, MSR_KVM_WALL_CLOCK));
    }

    None
}
