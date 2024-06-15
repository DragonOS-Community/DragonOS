use core::fmt::Debug;

use alloc::{boxed::Box, vec::Vec};
use bit_field::BitField;
use bitmap::{traits::BitMapOps, AllocBitmap};
use system_error::SystemError;
use x86::{
    bits64::rflags::RFlags,
    controlregs::{Cr0, Cr4},
    dtables::DescriptorTablePointer,
};
use x86_64::registers::control::EferFlags;

use crate::{
    smp::cpu::ProcessorId,
    virt::vm::{
        kvm_host::{
            vcpu::VirtCpu, Vm, KVM_IRQFD_RESAMPLE_IRQ_SOURCE_ID, KVM_USERSAPCE_IRQ_SOURCE_ID,
        },
        user_api::UapiKvmSegment,
    },
};

use crate::arch::VirtCpuArch;

use super::{
    asm::{MsrData, VcpuSegment, VmxMsrEntry},
    vmx::{exit::ExitFastpathCompletion, vmx_info},
    x86_kvm_manager, x86_kvm_ops,
};

pub mod lapic;
pub mod vcpu;
#[allow(dead_code)]
pub const TSS_IOPB_BASE_OFFSET: usize = 0x66;
pub const TSS_BASE_SIZE: usize = 0x68;
pub const TSS_IOPB_SIZE: usize = 65536 / 8;
pub const TSS_REDIRECTION_SIZE: usize = 256 / 8;
pub const RMODE_TSS_SIZE: usize = TSS_BASE_SIZE + TSS_REDIRECTION_SIZE + TSS_IOPB_SIZE + 1;

#[derive(Debug, Default)]
pub struct X86KvmArch {
    /// 中断芯片模式
    pub irqchip_mode: KvmIrqChipMode,
    /// 负责引导(bootstrap)kvm的vcpu_id
    bsp_vcpu_id: usize,
    pub pause_in_guest: bool,
    pub cstate_in_guest: bool,
    pub mwait_in_guest: bool,
    pub hlt_in_guest: bool,
    pub bus_lock_detection_enabled: bool,
    irq_sources_bitmap: u64,
    default_tsc_khz: u64,
    guest_can_read_msr_platform_info: bool,
    apicv_inhibit_reasons: usize,

    pub max_vcpu_ids: usize,

    pub notify_vmexit_flags: NotifyVmExitFlags,
    pub notify_window: u32,

    msr_fliter: Option<Box<KvmX86MsrFilter>>,
}

impl X86KvmArch {
    pub fn init(kvm_type: usize) -> Result<Self, SystemError> {
        if kvm_type != 0 {
            return Err(SystemError::EINVAL);
        }
        let mut arch = x86_kvm_ops().vm_init();

        // 设置中断源位图
        arch.irq_sources_bitmap
            .set_bit(KVM_USERSAPCE_IRQ_SOURCE_ID, true)
            .set_bit(KVM_IRQFD_RESAMPLE_IRQ_SOURCE_ID, true);

        arch.default_tsc_khz = x86_kvm_manager().max_tsc_khz;
        arch.guest_can_read_msr_platform_info = true;

        arch.apicv_init();
        Ok(arch)
    }

    fn apicv_init(&mut self) {
        self.apicv_inhibit_reasons
            .set_bit(KvmApicvInhibit::ABSENT, true);

        if !vmx_info().enable_apicv {
            self.apicv_inhibit_reasons
                .set_bit(KvmApicvInhibit::DISABLE, true);
        }
    }

    pub fn msr_allowed(&self, msr: u32, ftype: MsrFilterType) -> bool {
        // x2APIC MSRs
        if msr >= 0x800 && msr <= 0x8ff {
            return true;
        }

        if let Some(msr_filter) = &self.msr_fliter {
            let mut allowed = msr_filter.default_allow;

            for i in 0..msr_filter.count as usize {
                let range = &msr_filter.ranges[i];
                let start = range.base;
                let end = start + range.nmsrs;
                let flags = range.flags;
                let bitmap = &range.bitmap;
                if msr >= start && msr < end && flags.contains(ftype) {
                    allowed = bitmap.get((msr - start) as usize).unwrap_or(false);
                    break;
                }
            }

            return allowed;
        } else {
            return true;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum KvmIrqChipMode {
    None,
    Kernel,
    Split,
}

impl Default for KvmIrqChipMode {
    fn default() -> Self {
        Self::None
    }
}

pub trait KvmInitFunc {
    fn hardware_setup(&self) -> Result<(), SystemError>;
    fn handle_intel_pt_intr(&self) -> u32;
    fn runtime_funcs(&self) -> &'static dyn KvmFunc;
}

pub trait KvmFunc: Send + Sync + Debug {
    /// 返回该硬件支持的名字，例如“Vmx”
    fn name(&self) -> &'static str;

    /// 启用硬件支持
    fn hardware_enable(&self) -> Result<(), SystemError>;

    fn vm_init(&self) -> X86KvmArch;

    fn vcpu_precreate(&self, vm: &mut Vm) -> Result<(), SystemError>;

    fn vcpu_create(&self, vcpu: &mut VirtCpu, vm: &Vm);

    fn vcpu_load(&self, vcpu: &mut VirtCpu, cpu: ProcessorId);

    fn load_mmu_pgd(&self, vcpu: &mut VirtCpu, vm: &Vm, root_hpa: u64, root_level: u32);

    fn cache_reg(&self, vcpu: &mut VirtCpuArch, reg: KvmReg);

    fn apicv_pre_state_restore(&self, vcpu: &mut VirtCpu);

    fn set_msr(&self, vcpu: &mut VirtCpu, msr: MsrData) -> Result<(), SystemError>;

    fn set_rflags(&self, vcpu: &mut VirtCpu, rflags: RFlags);

    fn get_rflags(&self, vcpu: &mut VirtCpu) -> RFlags;

    fn set_cr0(&self, vm: &Vm, vcpu: &mut VirtCpu, cr0: Cr0);

    fn is_vaild_cr0(&self, vcpu: &VirtCpu, cr0: Cr0) -> bool;

    fn set_cr4(&self, vcpu: &mut VirtCpu, cr4: Cr4);

    fn post_set_cr3(&self, vcpu: &VirtCpu, cr3: u64);

    fn is_vaild_cr4(&self, vcpu: &VirtCpu, cr4: Cr4) -> bool;

    fn set_efer(&self, vcpu: &mut VirtCpu, efer: EferFlags);

    fn set_segment(&self, vcpu: &mut VirtCpu, var: &mut UapiKvmSegment, seg: VcpuSegment);

    fn get_segment(
        &self,
        vcpu: &mut VirtCpu,
        var: UapiKvmSegment,
        seg: VcpuSegment,
    ) -> UapiKvmSegment;

    /// 这个函数不会用到VCPU，这里拿到只是为了确保上一层拿到锁
    fn get_idt(&self, _vcpu: &mut VirtCpu, dt: &mut DescriptorTablePointer<u8>);

    fn set_idt(&self, _vcpu: &mut VirtCpu, dt: &DescriptorTablePointer<u8>);

    fn get_gdt(&self, _vcpu: &mut VirtCpu, dt: &mut DescriptorTablePointer<u8>);

    fn set_gdt(&self, _vcpu: &mut VirtCpu, dt: &DescriptorTablePointer<u8>);

    fn update_exception_bitmap(&self, vcpu: &mut VirtCpu);

    fn vcpu_reset(&self, vcpu: &mut VirtCpu, vm: &Vm, init_event: bool);

    fn has_emulated_msr(&self, msr: u32) -> bool;

    fn get_msr_feature(&self, msr: &mut VmxMsrEntry) -> bool;

    fn prepare_switch_to_guest(&self, vcpu: &mut VirtCpu);

    fn flush_tlb_all(&self, vcpu: &mut VirtCpu);

    fn vcpu_run(&self, vcpu: &mut VirtCpu) -> ExitFastpathCompletion;

    fn handle_exit_irqoff(&self, vcpu: &mut VirtCpu);

    fn handle_exit(
        &self,
        vcpu: &mut VirtCpu,
        fastpath: ExitFastpathCompletion,
    ) -> Result<(), SystemError>;
}

/// ## 中断抑制的原因位
#[derive(Debug)]
pub struct KvmApicvInhibit;

#[allow(dead_code)]
impl KvmApicvInhibit {
    // Intel与AMD共用

    /// APIC 加速功能被模块参数禁用，或者硬件不支持
    pub const DISABLE: usize = 0;

    /// Hyper-V 客户机正在使用 AutoEOI 功能，导致 APIC 加速被禁用。
    pub const HYPERV: usize = 1;

    /// 因为用户空间尚未启用内核或分裂的中断控制器，导致 APIC 加速被禁用。
    pub const ABSENT: usize = 2;

    /// KVM_GUESTDBG_BLOCKIRQ（一种调试措施，用于阻止该 vCPU 上的所有中断）被启用，以避免 AVIC/APICv 绕过此功能。
    pub const BLOCKIRQ: usize = 3;

    /// 当所有 vCPU 的 APIC ID 和 vCPU 的 1:1 映射被更改且 KVM 未应用其 x2APIC 热插拔修补程序时，APIC 加速被禁用。
    pub const PHYSICAL_ID_ALIASED: usize = 4;

    /// 当 vCPU 的 APIC ID 或 APIC 基址从其复位值更改时，首次禁用 APIC 加速。
    pub const APIC_ID_MODIFIED: usize = 5;
    /// 当 vCPU 的 APIC ID 或 APIC 基址从其复位值更改时，首次禁用 APIC 加速。
    pub const APIC_BASE_MODIFIED: usize = 6;

    // 仅仅对AMD适用

    /// 当 vCPU 运行嵌套客户机时，AVIC 被禁用。因为与 APICv 不同，当 vCPU 运行嵌套时，该 vCPU 的同级无法使用门铃机制通过 AVIC 信号中断。
    pub const NESTED: usize = 7;

    ///  在 SVM 上，等待 IRQ 窗口的实现使用挂起的虚拟中断，而在 KVM 等待 IRQ 窗口时无法注入这些虚拟中断，因此在等待 IRQ 窗口时 AVIC 被禁用。
    pub const IRQWIN: usize = 8;

    /// PIT（i8254）的“重新注入”模式依赖于 EOI 拦截，而 AVIC 不支持边沿触发中断的 EOI 拦截。
    pub const PIT_REINJ: usize = 9;

    /// SEV 不支持 AVIC，因此 AVIC 被禁用。
    pub const SEV: usize = 10;

    /// 当所有带有有效 LDR 的 vCPU 之间的逻辑 ID 和 vCPU 的 1:1 映射被更改时，AVIC 被禁用。
    pub const LOGICAL_ID_ALIASED: usize = 11;
}

#[derive(Debug)]
pub struct KvmX86MsrFilter {
    count: u8,
    default_allow: bool,
    ranges: Vec<KernelMsrRange>,
}

#[derive(Debug)]
pub struct KernelMsrRange {
    pub flags: MsrFilterType,
    pub nmsrs: u32,
    pub base: u32,
    pub bitmap: AllocBitmap,
}

#[repr(C)]
#[allow(dead_code)]
pub struct PosixMsrFilterRange {
    pub flags: u32,
    pub nmsrs: u32,
    pub base: u32,
    pub bitmap: *const u8,
}

bitflags! {
    pub struct MsrFilterType: u8 {
        const KVM_MSR_FILTER_READ  = 1 << 0;
        const KVM_MSR_FILTER_WRITE = 1 << 1;
    }

    pub struct NotifyVmExitFlags: u8 {
        const KVM_X86_NOTIFY_VMEXIT_ENABLED = 1 << 0;
        const KVM_X86_NOTIFY_VMEXIT_USER = 1 << 1;
    }
}

impl Default for NotifyVmExitFlags {
    fn default() -> Self {
        NotifyVmExitFlags::empty()
    }
}

#[derive(Debug, Clone, Copy)]
pub enum KvmReg {
    VcpuRegsRax = 0,
    VcpuRegsRcx = 1,
    VcpuRegsRdx = 2,
    VcpuRegsRbx = 3,
    VcpuRegsRsp = 4,
    VcpuRegsRbp = 5,
    VcpuRegsRsi = 6,
    VcpuRegsRdi = 7,

    VcpuRegsR8 = 8,
    VcpuRegsR9 = 9,
    VcpuRegsR10 = 10,
    VcpuRegsR11 = 11,
    VcpuRegsR12 = 12,
    VcpuRegsR13 = 13,
    VcpuRegsR14 = 14,
    VcpuRegsR15 = 15,

    VcpuRegsRip = 16,
    NrVcpuRegs = 17,

    VcpuExregCr0,
    VcpuExregCr3,
    VcpuExregCr4,
    VcpuExregRflags,
    VcpuExregSegments,
    VcpuExregExitInfo1,
    VcpuExregExitInfo2,
}

bitflags! {
    pub struct HFlags: u8 {
        const HF_GUEST_MASK = 1 << 0; /* VCPU is in guest-mode */
        const HF_SMM_MASK = 1 << 1;
        const HF_SMM_INSIDE_NMI_MASK = 1 << 2;
    }
}

/// ### 虚拟机的通用寄存器
#[derive(Debug, Default, Clone, Copy)]
#[repr(C)]
pub struct KvmCommonRegs {
    rax: u64,
    rbx: u64,
    rcx: u64,
    rdx: u64,
    rsi: u64,
    rdi: u64,
    rsp: u64,
    rbp: u64,
    r8: u64,
    r9: u64,
    r10: u64,
    r11: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    rip: u64,
    rflags: u64,
}

impl Vm {
    pub fn vcpu_precreate(&mut self, id: usize) -> Result<(), SystemError> {
        if self.arch.max_vcpu_ids == 0 {
            self.arch.max_vcpu_ids = 1024 * 4;
        }

        if id >= self.arch.max_vcpu_ids {
            return Err(SystemError::EINVAL);
        }

        return x86_kvm_ops().vcpu_precreate(self);
    }
}
