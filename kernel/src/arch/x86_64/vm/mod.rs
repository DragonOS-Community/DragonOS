use alloc::vec::Vec;
use log::{error, warn};
use raw_cpuid::CpuId;
use system_error::SystemError;
use x86::{
    controlregs::{cr4, xcr0, Cr0, Cr4, Xcr0},
    msr::{self, rdmsr, wrmsr},
};
use x86_64::registers::control::{Efer, EferFlags};

use crate::{
    arch::vm::vmx::{VmxL1dFlushState, L1TF_VMX_MITIGATION},
    libs::once::Once,
    mm::percpu::{PerCpu, PerCpuVar},
};

use self::{
    asm::{hyperv::*, kvm_msr::*, ArchCapabilities, VmxMsrEntry},
    kvm_host::{KvmFunc, KvmInitFunc},
};

use super::driver::tsc::TSCManager;

mod asm;
mod cpuid;
pub(super) mod exit;
pub mod kvm_host;
pub mod mem;
pub mod mmu;
pub mod mtrr;
pub mod uapi;
pub mod vmx;

static mut KVM_X86_MANAGER: Option<KvmArchManager> = None;

pub fn x86_kvm_ops() -> &'static dyn KvmFunc {
    unsafe { KVM_X86_MANAGER.as_ref().unwrap().funcs() }
}

pub fn x86_kvm_manager() -> &'static KvmArchManager {
    unsafe { KVM_X86_MANAGER.as_ref().unwrap() }
}

pub fn x86_kvm_manager_mut() -> &'static mut KvmArchManager {
    unsafe { KVM_X86_MANAGER.as_mut().unwrap() }
}

pub fn init_kvm_arch() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        KVM_X86_MANAGER = Some(KvmArchManager::init());

        let mut user_return_msrs = Vec::new();
        user_return_msrs.resize(PerCpu::MAX_CPU_NUM as usize, KvmUserReturnMsrs::default());
        USER_RETURN_MSRS = Some(PerCpuVar::new(user_return_msrs).unwrap());
    })
}

/// fixme：这些成员是否需要加锁呢？?
#[derive(Debug)]
pub struct KvmArchManager {
    funcs: Option<&'static dyn KvmFunc>,
    host_xcr0: Xcr0,
    host_efer: EferFlags,
    host_xss: u64,
    host_arch_capabilities: u64,
    kvm_uret_msrs_list: Vec<u32>,
    kvm_caps: KvmCapabilities,
    max_tsc_khz: u64,
    msrs_to_save: Vec<u32>,
    emulated_msrs: Vec<u32>,
    msr_based_features: Vec<u32>,

    has_noapic_vcpu: bool,

    enable_pmu: bool,

    // 只读
    possible_cr0_guest: Cr0,
    possible_cr4_guest: Cr4,
    cr4_tlbflush_bits: Cr4,
    cr4_pdptr_bits: Cr4,
}

impl KvmArchManager {
    pub fn init() -> Self {
        Self {
            possible_cr0_guest: Cr0::CR0_TASK_SWITCHED | Cr0::CR0_WRITE_PROTECT,
            possible_cr4_guest: Cr4::CR4_VIRTUAL_INTERRUPTS
                | Cr4::CR4_DEBUGGING_EXTENSIONS
                | Cr4::CR4_ENABLE_PPMC
                | Cr4::CR4_ENABLE_SSE
                | Cr4::CR4_UNMASKED_SSE
                | Cr4::CR4_ENABLE_GLOBAL_PAGES
                | Cr4::CR4_TIME_STAMP_DISABLE
                | Cr4::CR4_ENABLE_FSGSBASE,

            cr4_tlbflush_bits: Cr4::CR4_ENABLE_GLOBAL_PAGES
                | Cr4::CR4_ENABLE_PCID
                | Cr4::CR4_ENABLE_PAE
                | Cr4::CR4_ENABLE_SMEP,

            cr4_pdptr_bits: Cr4::CR4_ENABLE_GLOBAL_PAGES
                | Cr4::CR4_ENABLE_PSE
                | Cr4::CR4_ENABLE_PAE
                | Cr4::CR4_ENABLE_SMEP,

            host_xcr0: Xcr0::empty(),

            funcs: Default::default(),
            host_efer: EferFlags::empty(),
            host_xss: Default::default(),
            host_arch_capabilities: Default::default(),
            kvm_uret_msrs_list: Default::default(),
            kvm_caps: Default::default(),
            max_tsc_khz: Default::default(),
            msrs_to_save: Default::default(),
            emulated_msrs: Default::default(),
            msr_based_features: Default::default(),
            has_noapic_vcpu: Default::default(),
            enable_pmu: Default::default(),
        }
    }

    #[inline]
    pub fn set_runtime_func(&mut self, funcs: &'static dyn KvmFunc) {
        self.funcs = Some(funcs);
    }

    #[inline]
    pub fn funcs(&self) -> &'static dyn KvmFunc {
        self.funcs.unwrap()
    }

    pub fn find_user_return_msr_idx(&self, msr: u32) -> Option<usize> {
        for (i, val) in self.kvm_uret_msrs_list.iter().enumerate() {
            if *val == msr {
                return Some(i);
            }
        }

        None
    }

    pub fn mpx_supported(&self) -> bool {
        self.kvm_caps.supported_xcr0 & (Xcr0::XCR0_BNDREG_STATE | Xcr0::XCR0_BNDCSR_STATE)
            == (Xcr0::XCR0_BNDREG_STATE | Xcr0::XCR0_BNDCSR_STATE)
    }

    pub const KVM_MAX_VCPUS: usize = 1024;
    pub const KVM_MAX_NR_USER_RETURN_MSRS: usize = 7;

    const MSRS_TO_SAVE_BASE: &[u32] = &[
        msr::IA32_SYSENTER_CS,
        msr::IA32_SYSENTER_ESP,
        msr::IA32_SYSENTER_EIP,
        msr::IA32_STAR,
        msr::IA32_CSTAR,
        msr::IA32_KERNEL_GSBASE,
        msr::IA32_FMASK,
        msr::IA32_LSTAR,
        msr::IA32_TIME_STAMP_COUNTER,
        msr::IA32_PAT,
        0xc0010117, // MSR_VM_HSAVE_PA?
        msr::IA32_FEATURE_CONTROL,
        msr::MSR_C1_PMON_EVNT_SEL0,
        msr::IA32_TSC_AUX,
        0x48, // MSR_IA32_SPEC_CTRL
        msr::MSR_IA32_TSX_CTRL,
        msr::MSR_IA32_RTIT_CTL,
        msr::MSR_IA32_RTIT_STATUS,
        msr::MSR_IA32_CR3_MATCH,
        msr::MSR_IA32_RTIT_OUTPUT_BASE,
        msr::MSR_IA32_RTIT_OUTPUT_MASK_PTRS,
        msr::MSR_IA32_ADDR0_START,
        msr::MSR_IA32_ADDR0_END,
        msr::MSR_IA32_ADDR1_START,
        msr::MSR_IA32_ADDR1_END,
        msr::MSR_IA32_ADDR2_START,
        msr::MSR_IA32_ADDR2_END,
        msr::MSR_IA32_ADDR3_START,
        msr::MSR_IA32_ADDR3_END,
        0xe1,  // MSR_IA32_UMWAIT_CONTROL
        0x1c4, // MSR_IA32_XFD
        0x1c5, // MSR_IA32_XFD_ERR
    ];

    const EMULATED_MSRS_ALL: &[u32] = &[
        MSR_KVM_SYSTEM_TIME,
        MSR_KVM_WALL_CLOCK,
        MSR_KVM_SYSTEM_TIME_NEW,
        MSR_KVM_WALL_CLOCK_NEW,
        HV_X64_MSR_GUEST_OS_ID,
        HV_X64_MSR_HYPERCALL,
        HV_REGISTER_TIME_REF_COUNT,
        HV_REGISTER_REFERENCE_TSC,
        HV_X64_MSR_TSC_FREQUENCY,
        HV_X64_MSR_APIC_FREQUENCY,
        HV_REGISTER_CRASH_P0,
        HV_REGISTER_CRASH_P1,
        HV_REGISTER_CRASH_P2,
        HV_REGISTER_CRASH_P3,
        HV_REGISTER_CRASH_P4,
        HV_REGISTER_CRASH_CTL,
        HV_X64_MSR_RESET,
        HV_REGISTER_VP_INDEX,
        HV_X64_MSR_VP_RUNTIME,
        HV_REGISTER_SCONTROL,
        HV_REGISTER_STIMER0_CONFIG,
        HV_X64_MSR_VP_ASSIST_PAGE,
        HV_X64_MSR_REENLIGHTENMENT_CONTROL,
        HV_X64_MSR_TSC_EMULATION_CONTROL,
        HV_X64_MSR_TSC_EMULATION_STATUS,
        HV_X64_MSR_TSC_INVARIANT_CONTROL,
        HV_X64_MSR_SYNDBG_OPTIONS,
        HV_X64_MSR_SYNDBG_CONTROL,
        HV_X64_MSR_SYNDBG_STATUS,
        HV_X64_MSR_SYNDBG_SEND_BUFFER,
        HV_X64_MSR_SYNDBG_RECV_BUFFER,
        HV_X64_MSR_SYNDBG_PENDING_BUFFER,
        MSR_KVM_ASYNC_PF_EN,
        MSR_KVM_STEAL_TIME,
        MSR_KVM_PV_EOI_EN,
        MSR_KVM_ASYNC_PF_INT,
        MSR_KVM_ASYNC_PF_ACK,
        msr::IA32_TSC_ADJUST,
        msr::IA32_TSC_DEADLINE,
        msr::IA32_PERF_CAPABILITIES,
        0x10a, // MSR_IA32_ARCH_CAPABILITIES,
        msr::IA32_MISC_ENABLE,
        msr::IA32_MCG_STATUS,
        msr::IA32_MCG_CTL,
        0x4d0, // MSR_IA32_MCG_EXT_CTL,
        msr::IA32_SMBASE,
        msr::MSR_SMI_COUNT,
        msr::MSR_PLATFORM_INFO,
        0x140,      // MSR_MISC_FEATURES_ENABLES,
        0xc001011f, // MSR_AMD64_VIRT_SPEC_CTRL,
        0xc0000104, // MSR_AMD64_TSC_RATIO,
        msr::MSR_POWER_CTL,
        msr::IA32_BIOS_SIGN_ID, // MSR_IA32_UCODE_REV,
        /*
         * KVM always supports the "true" VMX control MSRs, even if the host
         * does not.  The VMX MSRs as a whole are considered "emulated" as KVM
         * doesn't strictly require them to exist in the host (ignoring that
         * KVM would refuse to load in the first place if the core set of MSRs
         * aren't supported).
         */
        msr::IA32_VMX_BASIC,
        msr::IA32_VMX_TRUE_PINBASED_CTLS,
        msr::IA32_VMX_TRUE_PROCBASED_CTLS,
        msr::IA32_VMX_TRUE_EXIT_CTLS,
        msr::IA32_VMX_TRUE_ENTRY_CTLS,
        msr::IA32_VMX_MISC,
        msr::IA32_VMX_CR0_FIXED0,
        msr::IA32_VMX_CR4_FIXED0,
        msr::IA32_VMX_VMCS_ENUM,
        msr::IA32_VMX_PROCBASED_CTLS2,
        msr::IA32_VMX_EPT_VPID_CAP,
        msr::IA32_VMX_VMFUNC,
        0xc0010015, // MSR_K7_HWCR,
        MSR_KVM_POLL_CONTROL,
    ];

    const MSR_BASED_FEATURES_ALL_EXCEPT_VMX: &[u32] = &[
        0xc0011029,             // MSR_AMD64_DE_CFG
        msr::IA32_BIOS_SIGN_ID, // MSR_IA32_UCODE_REV
        0x10a,                  // MSR_IA32_ARCH_CAPABILITIES,
        msr::IA32_PERF_CAPABILITIES,
    ];

    pub fn arch_hardware_enable(&self) -> Result<(), SystemError> {
        self.online_user_return_msr();

        x86_kvm_ops().hardware_enable()?;

        // TODO: 这里需要对TSC进行一系列检测

        Ok(())
    }

    /// ## 初始化当前cpu的kvm msr寄存器
    fn online_user_return_msr(&self) {
        let user_return_msrs = user_return_msrs().get_mut();

        for (idx, msr) in self.kvm_uret_msrs_list.iter().enumerate() {
            let val = unsafe { rdmsr(*msr) };
            user_return_msrs.values[idx].host = val;
            user_return_msrs.values[idx].curr = val;
        }
    }

    /// 厂商相关的init工作
    pub fn vendor_init(&mut self, init_ops: &'static dyn KvmInitFunc) -> Result<(), SystemError> {
        let cpuid = CpuId::new();
        let cpu_feature = cpuid.get_feature_info().ok_or(SystemError::ENOSYS)?;
        let cpu_extend = cpuid.get_extended_state_info().ok_or(SystemError::ENOSYS)?;
        let extend_features = cpuid
            .get_extended_feature_info()
            .ok_or(SystemError::ENOSYS)?;

        let kvm_x86_ops = &self.funcs;

        // 是否已经设置过
        if kvm_x86_ops.is_some() {
            error!(
                "[KVM] already loaded vendor module {}",
                kvm_x86_ops.unwrap().name()
            );
            return Err(SystemError::EEXIST);
        }

        // 确保cpu支持fpu浮点数处理器
        if !cpu_feature.has_fpu() || !cpu_feature.has_fxsave_fxstor() {
            error!("[KVM] inadequate fpu");
            return Err(SystemError::ENOSYS);
        }

        // TODO：实时内核需要判断tsc
        // https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/x86.c#9472

        // 读取主机page attribute table（页属性表）
        let host_pat = unsafe { rdmsr(msr::IA32_PAT) };
        // PAT[0]是否为write back类型，即判断低三位是否为0b110(0x06)
        if host_pat & 0b111 != 0b110 {
            error!("[KVM] host PAT[0] is not WB");
            return Err(SystemError::EIO);
        }

        // TODO：mmu vendor init
        if cpu_feature.has_xsave() && unsafe { cr4() }.contains(Cr4::CR4_ENABLE_OS_XSAVE) {
            self.host_xcr0 = unsafe { xcr0() };
            self.kvm_caps.supported_xcr0 = self.host_xcr0;
        }

        // 保存efer
        self.host_efer = Efer::read();

        // 保存xss
        if cpu_extend.has_xsaves_xrstors() {
            self.host_xss = unsafe { rdmsr(msr::MSR_C5_PMON_BOX_CTRL) };
        }

        // TODO: 初始化性能监视单元（PMU）
        // https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/x86.c#9518
        if extend_features.has_sha() {
            self.host_arch_capabilities = unsafe {
                // MSR_IA32_ARCH_CAPABILITIES
                rdmsr(0x10a)
            }
        }

        init_ops.hardware_setup()?;

        self.set_runtime_func(init_ops.runtime_funcs());

        self.kvm_timer_init()?;

        // TODO: https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/x86.c#9544

        let kvm_caps = &mut self.kvm_caps;
        if !cpu_extend.has_xsaves_xrstors() {
            kvm_caps.supported_xss = 0;
        }

        if kvm_caps.has_tsc_control {
            kvm_caps.max_guest_tsc_khz = 0x7fffffff.min(
                ((kvm_caps.max_tsc_scaling_ratio as i128 * TSCManager::tsc_khz() as i128)
                    >> kvm_caps.tsc_scaling_ratio_frac_bits) as u32,
            );
        }

        kvm_caps.default_tsc_scaling_ratio = 1 << kvm_caps.tsc_scaling_ratio_frac_bits;
        self.kvm_init_msr_lists();

        warn!("vendor init over");
        Ok(())
    }

    fn kvm_init_msr_lists(&mut self) {
        self.msrs_to_save.clear();
        self.emulated_msrs.clear();
        self.msr_based_features.clear();

        for msr in Self::MSRS_TO_SAVE_BASE {
            self.kvm_probe_msr_to_save(*msr);
        }

        if self.enable_pmu {
            todo!()
        }

        for msr in Self::EMULATED_MSRS_ALL {
            if !x86_kvm_ops().has_emulated_msr(*msr) {
                continue;
            }
            self.emulated_msrs.push(*msr);
        }

        for msr in msr::IA32_VMX_BASIC..=msr::IA32_VMX_VMFUNC {
            self.kvm_prove_feature_msr(msr)
        }

        for msr in Self::MSR_BASED_FEATURES_ALL_EXCEPT_VMX {
            self.kvm_prove_feature_msr(*msr);
        }
    }

    fn kvm_probe_msr_to_save(&mut self, msr: u32) {
        let cpuid = CpuId::new();
        let cpu_feat = cpuid.get_feature_info().unwrap();
        let cpu_extend = cpuid.get_extended_feature_info().unwrap();

        match msr {
            msr::MSR_C1_PMON_EVNT_SEL0 => {
                if !cpu_extend.has_mpx() {
                    return;
                }
            }

            msr::IA32_TSC_AUX => {
                if !cpu_feat.has_tsc() {
                    return;
                }
            }
            // MSR_IA32_UNWAIT_CONTROL
            0xe1 => {
                if !cpu_extend.has_waitpkg() {
                    return;
                }
            }
            msr::MSR_IA32_RTIT_CTL | msr::MSR_IA32_RTIT_STATUS => {
                if !cpu_extend.has_processor_trace() {
                    return;
                }
            }
            msr::MSR_IA32_CR3_MATCH => {
                // TODO: 判断intel_pt_validate_hw_cap(PT_CAP_cr3_filtering)
                if !cpu_extend.has_processor_trace() {
                    return;
                }
            }
            msr::MSR_IA32_RTIT_OUTPUT_BASE | msr::MSR_IA32_RTIT_OUTPUT_MASK_PTRS => {
                // TODO: 判断!intel_pt_validate_hw_cap(PT_CAP_topa_output) &&!intel_pt_validate_hw_cap(PT_CAP_single_range_output)
                if !cpu_extend.has_processor_trace() {
                    return;
                }
            }
            msr::MSR_IA32_ADDR0_START..msr::MSR_IA32_ADDR3_END => {
                // TODO: 判断msr_index - MSR_IA32_RTIT_ADDR0_A >= intel_pt_validate_hw_cap(PT_CAP_num_address_ranges) * 2)
                if !cpu_extend.has_processor_trace() {
                    return;
                }
            }
            msr::IA32_PMC0..msr::IA32_PMC7 => {
                // TODO: 判断msr是否符合配置
            }
            msr::IA32_PERFEVTSEL0..msr::IA32_PERFEVTSEL7 => {
                // TODO: 判断msr是否符合配置
            }
            msr::MSR_PERF_FIXED_CTR0..msr::MSR_PERF_FIXED_CTR2 => {
                // TODO: 判断msr是否符合配置
            }
            msr::MSR_IA32_TSX_CTRL => {
                // TODO: !(kvm_get_arch_capabilities() & ARCH_CAP_TSX_CTRL_MSR)
                // 这个寄存器目前不支持，现在先return
                // return;
            }
            _ => {}
        }

        self.msrs_to_save.push(msr);
    }

    fn kvm_prove_feature_msr(&mut self, index: u32) {
        let mut msr = VmxMsrEntry {
            index,
            reserved: Default::default(),
            data: Default::default(),
        };

        if self.get_msr_feature(&mut msr) {
            return;
        }

        self.msr_based_features.push(index);
    }

    fn get_msr_feature(&self, msr: &mut VmxMsrEntry) -> bool {
        match msr.index {
            0x10a => {
                // MSR_IA32_ARCH_CAPABILITIES,
                msr.data = self.get_arch_capabilities();
            }
            msr::IA32_PERF_CAPABILITIES => {
                msr.data = self.kvm_caps.supported_perf_cap;
            }
            msr::IA32_BIOS_SIGN_ID => {
                // MSR_IA32_UCODE_REV
                msr.data = unsafe { rdmsr(msr.index) };
            }
            _ => {
                return x86_kvm_ops().get_msr_feature(msr);
            }
        }

        return true;
    }

    fn get_arch_capabilities(&self) -> u64 {
        let mut data = ArchCapabilities::from_bits_truncate(self.host_arch_capabilities)
            & ArchCapabilities::KVM_SUPPORTED_ARCH_CAP;
        data.insert(ArchCapabilities::ARCH_CAP_PSCHANGE_MC_NO);

        if *L1TF_VMX_MITIGATION.read() != VmxL1dFlushState::Never {
            data.insert(ArchCapabilities::ARCH_CAP_SKIP_VMENTRY_L1DFLUSH);
        }

        // fixme:这里是直接赋值，这里应该是需要判断cpu是否存在某些bug

        data.insert(
            ArchCapabilities::ARCH_CAP_RDCL_NO
                | ArchCapabilities::ARCH_CAP_SSB_NO
                | ArchCapabilities::ARCH_CAP_MDS_NO
                | ArchCapabilities::ARCH_CAP_GDS_NO,
        );

        return data.bits();
    }

    pub fn add_user_return_msr(&mut self, msr: u32) {
        assert!(self.kvm_uret_msrs_list.len() < Self::KVM_MAX_NR_USER_RETURN_MSRS);
        self.kvm_uret_msrs_list.push(msr)
    }

    fn kvm_timer_init(&mut self) -> Result<(), SystemError> {
        let cpuid = CpuId::new();
        let cpu_feature = cpuid.get_feature_info().ok_or(SystemError::ENOSYS)?;
        if cpu_feature.has_tsc() {
            self.max_tsc_khz = TSCManager::tsc_khz();
        }

        // TODO:此处未完成
        Ok(())
    }

    pub fn kvm_set_user_return_msr(&self, slot: usize, mut value: u64, mask: u64) {
        let msrs = user_return_msrs().get_mut();

        value = (value & mask) | (msrs.values[slot].host & !mask);
        if value == msrs.values[slot].curr {
            return;
        }

        unsafe { wrmsr(self.kvm_uret_msrs_list[slot], value) };

        msrs.values[slot].curr = value;

        if !msrs.registered {
            msrs.registered = true;
        }
    }
}

/// ### Kvm的功能特性
#[derive(Debug)]
pub struct KvmCapabilities {
    ///  是否支持控制客户机的 TSC（时间戳计数器）速率
    has_tsc_control: bool,
    /// 客户机可以使用的 TSC 的最大速率，以khz为单位
    max_guest_tsc_khz: u32,
    /// TSC 缩放比例的小数部分的位数
    tsc_scaling_ratio_frac_bits: u8,
    /// TSC 缩放比例的最大允许值
    max_tsc_scaling_ratio: u64,
    /// 默认的 TSC 缩放比例，其值为 1ull << tsc_scaling_ratio_frac_bits
    default_tsc_scaling_ratio: u64,
    /// 是否支持总线锁定的退出
    has_bus_lock_exit: bool,
    /// 是否支持 VM 退出通知
    has_notify_vmexit: bool,
    /// 支持的 MCE（机器检查异常）功能的位掩码
    supported_mce_cap: McgCap,
    /// 支持的 XCR0 寄存器的位掩码
    supported_xcr0: Xcr0,
    /// 支持的 XSS（XSAVE Extended State）寄存器的位掩码
    supported_xss: u64,
    /// 支持的性能监控功能的位掩码
    supported_perf_cap: u64,
}

impl Default for KvmCapabilities {
    fn default() -> Self {
        Self {
            has_tsc_control: Default::default(),
            max_guest_tsc_khz: Default::default(),
            tsc_scaling_ratio_frac_bits: Default::default(),
            max_tsc_scaling_ratio: Default::default(),
            default_tsc_scaling_ratio: Default::default(),
            has_bus_lock_exit: Default::default(),
            has_notify_vmexit: Default::default(),
            supported_mce_cap: McgCap::MCG_CTL_P | McgCap::MCG_SER_P,
            supported_xcr0: Xcr0::empty(),
            supported_xss: Default::default(),
            supported_perf_cap: Default::default(),
        }
    }
}

bitflags! {
    pub struct McgCap: u64 {
        const MCG_BANKCNT_MASK	= 0xff;         /* Number of Banks */
        const MCG_CTL_P		= 1 << 8;   /* MCG_CTL register available */
        const MCG_EXT_P		= 1 << 9;   /* Extended registers available */
        const MCG_CMCI_P	= 1 << 10;  /* CMCI supported */
        const MCG_EXT_CNT_MASK	= 0xff0000;     /* Number of Extended registers */
        const MCG_EXT_CNT_SHIFT	= 16;
        const MCG_SER_P		= 1 << 24;  /* MCA recovery/new status bits */
        const MCG_ELOG_P	= 1 << 26;  /* Extended error log supported */
        const MCG_LMCE_P	= 1 << 27;  /* Local machine check supported */
    }
}

static mut USER_RETURN_MSRS: Option<PerCpuVar<KvmUserReturnMsrs>> = None;

fn user_return_msrs() -> &'static PerCpuVar<KvmUserReturnMsrs> {
    unsafe { USER_RETURN_MSRS.as_ref().unwrap() }
}

#[derive(Debug, Default, Clone)]
struct KvmUserReturnMsrs {
    pub registered: bool,
    pub values: [KvmUserReturnMsrsValues; KvmArchManager::KVM_MAX_NR_USER_RETURN_MSRS],
}

#[derive(Debug, Default, Clone)]
struct KvmUserReturnMsrsValues {
    pub host: u64,
    pub curr: u64,
}
