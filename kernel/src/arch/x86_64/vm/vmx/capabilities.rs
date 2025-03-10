use raw_cpuid::CpuId;
use x86::{
    msr,
    vmx::vmcs::control::{
        EntryControls, ExitControls, PinbasedControls, PrimaryControls, SecondaryControls,
    },
};

use crate::{
    arch::vm::{
        mmu::kvm_mmu::PageLevel, CPU_BASED_ALWAYSON_WITHOUT_TRUE_MSR,
        PIN_BASED_ALWAYSON_WITHOUT_TRUE_MSR, VM_ENTRY_ALWAYSON_WITHOUT_TRUE_MSR,
        VM_EXIT_ALWAYSON_WITHOUT_TRUE_MSR,
    },
    virt::vm::kvm_host::vcpu::VirtCpu,
};

use super::{vmcs::feat::VmxFeat, Vmx};

#[derive(Debug)]
pub struct VmcsConfig {
    pub size: u32,
    pub basic_cap: u32,
    pub revision_id: u32,
    pub pin_based_exec_ctrl: PinbasedControls,
    pub cpu_based_exec_ctrl: PrimaryControls,
    pub cpu_based_2nd_exec_ctrl: SecondaryControls,
    pub cpu_based_3rd_exec_ctrl: u32,
    pub vmexit_ctrl: ExitControls,
    pub vmentry_ctrl: EntryControls,
    pub misc: u64,
    pub nested: NestedVmxMsrs,
}

impl Default for VmcsConfig {
    fn default() -> Self {
        Self {
            size: Default::default(),
            basic_cap: Default::default(),
            revision_id: Default::default(),
            pin_based_exec_ctrl: PinbasedControls::empty(),
            cpu_based_exec_ctrl: PrimaryControls::empty(),
            cpu_based_2nd_exec_ctrl: SecondaryControls::empty(),
            cpu_based_3rd_exec_ctrl: Default::default(),
            vmexit_ctrl: ExitControls::empty(),
            vmentry_ctrl: EntryControls::empty(),
            misc: Default::default(),
            nested: Default::default(),
        }
    }
}

#[derive(Debug, Default)]
pub struct NestedVmxMsrs {
    /// 主处理器基于控制，分为低32位和高32位
    pub procbased_ctls_low: u32,
    /// 主处理器基于控制，分为低32位和高32位
    pub procbased_ctls_high: u32,
    /// 次要处理器控制，分为低32位和高32位
    pub secondary_ctls_low: u32,
    /// 次要处理器控制，分为低32位和高32位
    pub secondary_ctls_high: u32,
    /// VMX 的针脚基于控制，分为低32位和高32位
    pub pinbased_ctls_low: u32,
    /// VMX 的针脚基于控制，分为低32位和高32位
    pub pinbased_ctls_high: u32,
    /// VM退出控制，分为低32位和高32位
    pub exit_ctls_low: u32,
    /// VM退出控制，分为低32位和高32位
    pub exit_ctls_high: u32,
    /// VM进入控制，分为低32位和高32位
    pub entry_ctls_low: u32,
    /// VM进入控制，分为低32位和高32位
    pub entry_ctls_high: u32,
    /// VMX 的其他杂项控制，分为低32位和高32位
    pub misc_low: u32,
    /// VMX 的其他杂项控制，分为低32位和高32位
    pub misc_high: u32,
    /// 扩展页表（EPT）的能力信息
    pub ept_caps: u32,
    /// 虚拟处理器标识（VPID）的能力信息
    pub vpid_caps: u32,
    ///  基本能力
    pub basic: u64,
    ///  VMX 控制的CR0寄存器的固定位
    pub cr0_fixed0: u64,
    ///  VMX 控制的CR0寄存器的固定位
    pub cr0_fixed1: u64,
    ///  VMX 控制的CR4寄存器的固定位
    pub cr4_fixed0: u64,
    ///  VMX 控制的CR4寄存器的固定位
    pub cr4_fixed1: u64,
    /// VMX 控制的VMCS寄存器的编码
    pub vmcs_enum: u64,
    /// VM功能控制
    pub vmfunc_controls: u64,
}

impl NestedVmxMsrs {
    pub fn control_msr(low: u32, high: u32) -> u64 {
        (high as u64) << 32 | low as u64
    }

    pub fn get_vmx_msr(&self, msr_index: u32) -> Option<u64> {
        match msr_index {
            msr::IA32_VMX_BASIC => {
                return Some(self.basic);
            }
            msr::IA32_VMX_TRUE_PINBASED_CTLS | msr::IA32_VMX_PINBASED_CTLS => {
                let mut data =
                    NestedVmxMsrs::control_msr(self.pinbased_ctls_low, self.pinbased_ctls_high);
                if msr_index == msr::IA32_VMX_PINBASED_CTLS {
                    data |= PIN_BASED_ALWAYSON_WITHOUT_TRUE_MSR;
                }
                return Some(data);
            }
            msr::IA32_VMX_TRUE_PROCBASED_CTLS | msr::IA32_VMX_PROCBASED_CTLS => {
                let mut data =
                    NestedVmxMsrs::control_msr(self.procbased_ctls_low, self.procbased_ctls_high);
                if msr_index == msr::IA32_VMX_PROCBASED_CTLS {
                    data |= CPU_BASED_ALWAYSON_WITHOUT_TRUE_MSR;
                }
                return Some(data);
            }
            msr::IA32_VMX_TRUE_EXIT_CTLS | msr::IA32_VMX_EXIT_CTLS => {
                let mut data = NestedVmxMsrs::control_msr(self.exit_ctls_low, self.exit_ctls_high);
                if msr_index == msr::IA32_VMX_EXIT_CTLS {
                    data |= VM_EXIT_ALWAYSON_WITHOUT_TRUE_MSR;
                }
                return Some(data);
            }
            msr::IA32_VMX_TRUE_ENTRY_CTLS | msr::IA32_VMX_ENTRY_CTLS => {
                let mut data =
                    NestedVmxMsrs::control_msr(self.entry_ctls_low, self.entry_ctls_high);
                if msr_index == msr::IA32_VMX_ENTRY_CTLS {
                    data |= VM_ENTRY_ALWAYSON_WITHOUT_TRUE_MSR;
                }
                return Some(data);
            }
            msr::IA32_VMX_MISC => {
                return Some(NestedVmxMsrs::control_msr(self.misc_low, self.misc_high));
            }
            msr::IA32_VMX_CR0_FIXED0 => {
                return Some(self.cr0_fixed0);
            }
            msr::IA32_VMX_CR0_FIXED1 => {
                return Some(self.cr0_fixed1);
            }
            msr::IA32_VMX_CR4_FIXED0 => {
                return Some(self.cr4_fixed0);
            }
            msr::IA32_VMX_CR4_FIXED1 => {
                return Some(self.cr4_fixed1);
            }
            msr::IA32_VMX_VMCS_ENUM => {
                return Some(self.vmcs_enum);
            }
            msr::IA32_VMX_PROCBASED_CTLS2 => {
                return Some(NestedVmxMsrs::control_msr(
                    self.secondary_ctls_low,
                    self.secondary_ctls_high,
                ));
            }
            msr::IA32_VMX_EPT_VPID_CAP => {
                return Some(self.ept_caps as u64 | ((self.vpid_caps as u64) << 32));
            }
            msr::IA32_VMX_VMFUNC => {
                return Some(self.vmfunc_controls);
            }
            _ => {
                return None;
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct VmxCapability {
    pub ept: EptFlag,
    pub vpid: VpidFlag,
}

#[derive(Debug, PartialEq)]
pub enum ProcessorTraceMode {
    System,
    HostGuest,
}

bitflags! {
    #[derive(Default)]
    pub struct VpidFlag: u32 {
        /// 表示处理器支持 INVVPID 指令
        const INVVPID = 1 << 0; /* (32 - 32) */
        /// 表示 VPID 支持以单独地址方式进行范围
        const EXTENT_INDIVIDUAL_ADDR = 1 << 8; /* (40 - 32) */
        /// 表示 VPID 支持以单个上下文方式进行范围
        const EXTENT_SINGLE_CONTEXT = 1 << 9; /* (41 - 32) */
        /// 表示 VPID 支持以全局上下文方式进行范围
        const EXTENT_GLOBAL_CONTEXT = 1 << 10; /* (42 - 32) */
        /// 表示 VPID 支持以单个非全局方式进行范围
        const EXTENT_SINGLE_NON_GLOBAL = 1 << 11; /* (43 - 32) */
    }

    #[derive(Default)]
    pub struct EptFlag: u32 {
        /// EPT 条目是否允许执行
        const EPT_EXECUTE_ONLY = 1;
        /// 处理器是否支持 4 级页表
        const EPT_PAGE_WALK_4 = 1 << 6;
        /// 处理器是否支持 5 级页表
        const EPT_PAGE_WALK_5 = 1 << 7;
        /// EPT 表的内存类型是否为不可缓存（uncached）
        const EPTP_UC = 1 << 8;
        /// EPT 表的内存类型是否为写回（write-back）
        const EPTP_WB = 1 << 14;
        /// 处理器是否支持 2MB 大页
        const EPT_2MB_PAGE = 1 << 16;
        /// 处理器是否支持 1GB 大页
        const EPT_1GB_PAGE = 1 << 17;
        /// 处理器是否支持 INV-EPT 指令，用于刷新 EPT TLB
        const EPT_INVEPT = 1 << 20;
        /// EPT 表是否支持访问位（Access-Dirty）
        const EPT_AD = 1 << 21;
        /// 处理器是否支持上下文扩展
        const EPT_EXTENT_CONTEXT = 1 << 25;
        /// 处理器是否支持全局扩展
        const EPT_EXTENT_GLOBAL = 1 << 26;
    }
}

impl VmxCapability {
    pub fn set_val_from_msr_val(&mut self, val: u64) {
        self.ept = EptFlag::from_bits_truncate(val as u32);
        self.vpid = VpidFlag::from_bits_truncate((val >> 32) as u32);
    }
}

impl Vmx {
    /// 检查处理器是否支持VMX基本控制结构的输入输出功能
    #[inline]
    #[allow(dead_code)]
    pub fn has_basic_inout(&self) -> bool {
        return ((self.vmcs_config.basic_cap as u64) << 32) & VmxFeat::VMX_BASIC_INOUT != 0;
    }

    /// 检查处理器是否支持虚拟的非屏蔽中断（NMI）
    #[inline]
    pub fn has_virtual_nmis(&self) -> bool {
        return self
            .vmcs_config
            .pin_based_exec_ctrl
            .contains(PinbasedControls::VIRTUAL_NMIS)
            && self
                .vmcs_config
                .cpu_based_exec_ctrl
                .contains(PrimaryControls::NMI_WINDOW_EXITING);
    }

    /// 检查处理器是否支持VMX的抢占计时器功能
    #[inline]
    pub fn has_preemption_timer(&self) -> bool {
        return self
            .vmcs_config
            .pin_based_exec_ctrl
            .contains(PinbasedControls::VMX_PREEMPTION_TIMER);
    }

    /// 检查处理器是否支持VMX的posted interrupt功能
    #[inline]
    pub fn has_posted_intr(&self) -> bool {
        return self
            .vmcs_config
            .pin_based_exec_ctrl
            .contains(PinbasedControls::POSTED_INTERRUPTS);
    }

    /// 是否支持加载IA32_EFER寄存器
    #[inline]
    pub fn has_load_ia32_efer(&self) -> bool {
        return self
            .vmcs_config
            .vmentry_ctrl
            .contains(EntryControls::LOAD_IA32_EFER);
    }

    /// 是否支持加载IA32_PERF_GLOBAL_CTRL寄存器
    #[inline]
    pub fn has_load_perf_global_ctrl(&self) -> bool {
        return self
            .vmcs_config
            .vmentry_ctrl
            .contains(EntryControls::LOAD_IA32_PERF_GLOBAL_CTRL);
    }

    /// 是否支持加载边界检查配置寄存器（MPX）
    #[inline]
    pub fn has_mpx(&self) -> bool {
        return self
            .vmcs_config
            .vmentry_ctrl
            .contains(EntryControls::LOAD_IA32_BNDCFGS);
    }

    /// 是否支持虚拟处理器的任务优先级（TPR）影子
    #[inline]
    pub fn has_tpr_shadow(&self) -> bool {
        return self
            .vmcs_config
            .cpu_based_exec_ctrl
            .contains(PrimaryControls::USE_TPR_SHADOW);
    }

    /// 检查处理器是否支持 VMX中的 VPID（Virtual Processor ID）功能
    ///
    /// VPID 允许虚拟机监视器为每个虚拟处理器分配唯一的标识符，从而使得在不同的虚拟机之间进行快速的上下文切换和恢复成为可能。
    ///
    /// 通过使用 VPID，VMM 可以更快速地识别和恢复之前保存的虚拟处理器的状态，从而提高了虚拟化性能和效率。
    #[inline]
    pub fn has_vpid(&self) -> bool {
        return self
            .vmcs_config
            .cpu_based_2nd_exec_ctrl
            .contains(SecondaryControls::ENABLE_VPID);
    }

    /// 是否支持invvpid
    ///
    /// INVVPID 指令用于通知处理器无效化指定虚拟处理器标识符（VPID）相关的 TLB（Translation Lookaside Buffer）条目
    #[inline]
    pub fn has_invvpid(&self) -> bool {
        return self.vmx_cap.vpid.contains(VpidFlag::INVVPID);
    }

    /// VPID 是否支持以单独地址方式进行范围
    #[allow(dead_code)]
    #[inline]
    pub fn has_invvpid_individual_addr(&self) -> bool {
        return self.vmx_cap.vpid.contains(VpidFlag::EXTENT_INDIVIDUAL_ADDR);
    }

    /// VPID 是否支持以单个上下文方式进行范围
    #[inline]
    pub fn has_invvpid_single(&self) -> bool {
        return self.vmx_cap.vpid.contains(VpidFlag::EXTENT_SINGLE_CONTEXT);
    }

    /// VPID 是否支持以全局上下文方式进行范围
    #[inline]
    pub fn has_invvpid_global(&self) -> bool {
        return self.vmx_cap.vpid.contains(VpidFlag::EXTENT_GLOBAL_CONTEXT);
    }

    /// 是否启用EPT(Extended Page Tables)
    ///
    /// EPT:EPT 是一种硬件虚拟化技术，允许虚拟机管理程序(例如 Hypervisor) 控制客户操作系统中虚拟地址和物理地址之间的映射。
    ///
    /// 通过启用 EPT，处理器可以将虚拟地址直接映射到物理地址，从而提高虚拟机的性能和安全性。
    #[inline]
    pub fn has_ept(&self) -> bool {
        return self
            .vmcs_config
            .cpu_based_2nd_exec_ctrl
            .contains(SecondaryControls::ENABLE_EPT);
    }

    /// 是否支持4级页表
    #[inline]
    pub fn has_ept_4levels(&self) -> bool {
        return self.vmx_cap.ept.contains(EptFlag::EPT_PAGE_WALK_4);
    }

    /// 是否支持5级页表
    #[inline]
    pub fn has_ept_5levels(&self) -> bool {
        return self.vmx_cap.ept.contains(EptFlag::EPT_PAGE_WALK_5);
    }

    pub fn get_max_ept_level(&self) -> usize {
        if self.has_ept_5levels() {
            return 5;
        }
        return 4;
    }

    pub fn ept_cap_to_lpage_level(&self) -> PageLevel {
        if self.vmx_cap.ept.contains(EptFlag::EPT_1GB_PAGE) {
            return PageLevel::Level1G;
        }
        if self.vmx_cap.ept.contains(EptFlag::EPT_2MB_PAGE) {
            return PageLevel::Level2M;
        }

        return PageLevel::Level4K;
    }

    /// 判断mt(Memory type)是否为write back
    #[inline]
    pub fn has_ept_mt_wb(&self) -> bool {
        return self.vmx_cap.ept.contains(EptFlag::EPTP_WB);
    }

    #[inline]
    pub fn has_vmx_invept_context(&self) -> bool {
        self.vmx_cap.ept.contains(EptFlag::EPT_EXTENT_CONTEXT)
    }

    /// EPT是否支持全局拓展
    #[inline]
    pub fn has_invept_global(&self) -> bool {
        return self.vmx_cap.ept.contains(EptFlag::EPT_EXTENT_GLOBAL);
    }

    /// EPT是否支持访问位
    #[inline]
    pub fn has_ept_ad_bits(&self) -> bool {
        return self.vmx_cap.ept.contains(EptFlag::EPT_AD);
    }

    /// 是否支持 VMX 中的无限制客户（unrestricted guest）功能
    ///
    /// 无限制客户功能允许客户操作系统在未受到主机操作系统干预的情况下运行
    #[inline]
    pub fn has_unrestricted_guest(&self) -> bool {
        return self
            .vmcs_config
            .cpu_based_2nd_exec_ctrl
            .contains(SecondaryControls::UNRESTRICTED_GUEST);
    }

    /// 是否支持 VMX 中的 FlexPriority 功能
    ///
    /// FlexPriority 是一种功能，可以在 TPR shadow 和虚拟化 APIC 访问同时可用时启用。
    ///
    /// TPR shadow 允许虚拟机管理程序（VMM）跟踪虚拟机中处理器的 TPR 值，并在需要时拦截和修改。
    ///
    /// 虚拟化 APIC 访问允许 VMM 控制虚拟机中的 APIC 寄存器访问。
    #[inline]
    pub fn has_flexproirity(&self) -> bool {
        return self.has_tpr_shadow() && self.has_virtualize_apic_accesses();
    }

    /// 是否支持 VMX 中的虚拟化 APIC 访问功能。
    ///
    /// 当启用此功能时，虚拟机管理程序（VMM）可以控制虚拟机中的 APIC 寄存器访问。
    #[inline]
    pub fn has_virtualize_apic_accesses(&self) -> bool {
        return self
            .vmcs_config
            .cpu_based_2nd_exec_ctrl
            .contains(SecondaryControls::VIRTUALIZE_APIC);
    }

    /// 是否支持 VMX 中的 ENCLS 指令导致的 VM 退出功能
    #[inline]
    pub fn has_encls_vmexit(&self) -> bool {
        return self
            .vmcs_config
            .cpu_based_2nd_exec_ctrl
            .contains(SecondaryControls::ENCLS_EXITING);
    }

    /// 是否支持 VMX 中的 PLE (Pause Loop Exiting) 功能。
    #[inline]
    pub fn has_ple(&self) -> bool {
        return self
            .vmcs_config
            .cpu_based_2nd_exec_ctrl
            .contains(SecondaryControls::PAUSE_LOOP_EXITING);
    }

    /// 是否支持 VMX 中的 APICv 功能
    #[inline]
    pub fn has_apicv(&self) -> bool {
        return self.has_apic_register_virt()
            && self.has_posted_intr()
            && self.has_virtual_intr_delivery();
    }

    /// 是否支持虚拟化的 APIC 寄存器功能
    #[inline]
    pub fn has_apic_register_virt(&self) -> bool {
        return self
            .vmcs_config
            .cpu_based_2nd_exec_ctrl
            .contains(SecondaryControls::VIRTUALIZE_APIC_REGISTER);
    }

    /// 是否支持虚拟化的中断传递功能
    #[inline]
    pub fn has_virtual_intr_delivery(&self) -> bool {
        return self
            .vmcs_config
            .cpu_based_2nd_exec_ctrl
            .contains(SecondaryControls::VIRTUAL_INTERRUPT_DELIVERY);
    }

    /// 是否支持虚拟化的中断注入（Inter-Processor Interrupt Virtualization，IPIV）
    #[inline]
    pub fn has_ipiv(&self) -> bool {
        return false;
    }

    /// 是否支持虚拟化的 TSC 缩放功能
    #[inline]
    pub fn has_tsc_scaling(&self) -> bool {
        return self
            .vmcs_config
            .cpu_based_2nd_exec_ctrl
            .contains(SecondaryControls::USE_TSC_SCALING);
    }

    /// 是否支持虚拟化的页修改日志（Page Modification Logging）
    #[inline]
    pub fn has_pml(&self) -> bool {
        return self
            .vmcs_config
            .cpu_based_2nd_exec_ctrl
            .contains(SecondaryControls::ENABLE_PML);
    }

    /// 检查 CPU 是否支持使用 MSR 位图来控制 VMX
    #[inline]
    pub fn has_msr_bitmap(&self) -> bool {
        return self
            .vmcs_config
            .cpu_based_exec_ctrl
            .contains(PrimaryControls::USE_MSR_BITMAPS);
    }

    #[inline]
    pub fn has_sceondary_exec_ctrls(&self) -> bool {
        self.vmcs_config
            .cpu_based_exec_ctrl
            .contains(PrimaryControls::SECONDARY_CONTROLS)
    }

    #[inline]
    pub fn has_rdtscp(&self) -> bool {
        self.vmcs_config
            .cpu_based_2nd_exec_ctrl
            .contains(SecondaryControls::ENABLE_RDTSCP)
    }

    #[inline]
    pub fn has_vmfunc(&self) -> bool {
        self.vmcs_config
            .cpu_based_2nd_exec_ctrl
            .contains(SecondaryControls::ENABLE_VM_FUNCTIONS)
    }

    #[inline]
    pub fn has_xsaves(&self) -> bool {
        self.vmcs_config
            .cpu_based_2nd_exec_ctrl
            .contains(SecondaryControls::ENABLE_XSAVES_XRSTORS)
    }

    #[inline]
    pub fn vmx_umip_emulated(&self) -> bool {
        let feat = CpuId::new().get_extended_feature_info().unwrap().has_umip();

        return !feat
            && (self
                .vmcs_config
                .cpu_based_2nd_exec_ctrl
                .contains(SecondaryControls::DTABLE_EXITING));
    }

    #[inline]
    pub fn has_tertiary_exec_ctrls(&self) -> bool {
        false
    }

    #[inline]
    pub fn has_bus_lock_detection(&self) -> bool {
        false
    }

    #[inline]
    pub fn has_notify_vmexit(&self) -> bool {
        false
    }

    /// 是否需要拦截页面故障
    #[inline]
    pub fn vmx_need_pf_intercept(&self, _vcpu: &VirtCpu) -> bool {
        // if (!enable_ept)
        // return true;
        false
    }
}
