use core::intrinsics::unlikely;

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use bitmap::{traits::BitMapOps, AllocBitmap, StaticBitmap};
use raw_cpuid::CpuId;
use system_error::SystemError;
use x86::{
    bits64::rflags::RFlags,
    controlregs::{Cr0, Cr4},
    msr::{
        IA32_APIC_BASE, IA32_CSTAR, IA32_FS_BASE, IA32_GS_BASE, IA32_KERNEL_GSBASE, IA32_LSTAR,
        IA32_SYSENTER_EIP, IA32_SYSENTER_ESP, IA32_TSC_AUX,
    },
};
use x86_64::registers::control::EferFlags;

use crate::{
    arch::{
        kvm_arch_ops,
        vm::{
            asm::{KvmX86Asm, MiscEnable, MsrData},
            cpuid::KvmCpuidEntry2,
            kvm_host::KvmReg,
            mmu::{KvmMmu, LockedKvmMmu},
            vmx::vmcs::LoadedVmcs,
            x86_kvm_manager, x86_kvm_manager_mut, x86_kvm_ops,
        },
    },
    kdebug, kerror,
    mm::{PhysAddr, VirtAddr},
    smp::{core::smp_get_processor_id, cpu::ProcessorId},
    virt::vm::kvm_host::{
        mem::GfnToHvaCache,
        vcpu::{GuestDebug, VirtCpu},
        LockedVm, MutilProcessorState, Vm,
    },
};

use super::{HFlags, KvmCommonRegs, KvmIrqChipMode};

#[derive(Debug)]
pub struct X86VcpuArch {
    /// 最近一次尝试进入虚拟机的主机cpu
    last_vmentry_cpu: ProcessorId,
    /// 可用寄存器数量
    regs_avail: AllocBitmap,
    /// 脏寄存器数量
    regs_dirty: AllocBitmap,
    /// 多处理器状态
    mp_state: MutilProcessorState,
    pub apic_base: u64,
    /// apic
    pub apic: Option<()>,
    /// 主机pkru寄存器
    host_pkru: u32,
    /// hflag
    hflags: HFlags,

    pub guest_state_protected: bool,

    pub cpuid_entries: Vec<KvmCpuidEntry2>,

    pub exception: KvmQueuedException,
    pub exception_vmexit: KvmQueuedException,
    pub apf: KvmAsyncPageFault,

    pub smbase: u64,

    pub interrupt: KvmQueuedInterrupt,

    pub tsc_offset_adjustment: u64,

    pub mmu: Option<Arc<LockedKvmMmu>>,
    pub root_mmu: Option<Arc<LockedKvmMmu>>,
    pub guset_mmu: Option<Arc<LockedKvmMmu>>,
    pub walk_mmu: Option<Arc<LockedKvmMmu>>,
    pub nested_mmu: Option<Arc<LockedKvmMmu>>,

    pub max_phyaddr: usize,

    pub regs: [u64; KvmReg::NrVcpuRegs as usize],

    pub cr0: Cr0,
    pub cr0_guest_owned_bits: Cr0,
    pub cr2: usize,
    pub cr3: usize,
    pub cr4: Cr4,
    pub cr4_guest_owned_bits: Cr4,
    pub cr4_guest_rsvd_bits: usize,
    pub cr8: usize,
    pub efer: EferFlags,

    pub dr6: usize,
    pub dr7: usize,

    pub single_step_rip: usize,

    pub msr_misc_features_enables: u64,
    pub ia32_misc_enable_msr: MiscEnable,

    pub smi_pending: bool,
    pub smi_count: u64,
    pub nmi_queued: usize,
    /// 待注入的 NMI 数量，不包括硬件 vNMI。
    pub nmi_pending: u32,
    pub nmi_injected: bool,

    pub db: [usize; Self::KVM_NR_DB_REGS],
}

impl X86VcpuArch {
    const KVM_NR_DB_REGS: usize = 4;

    #[inline(never)]
    pub fn new() -> Self {
        let mut ret: Box<X86VcpuArch> = unsafe { Box::new_zeroed().assume_init() };
        ret.last_vmentry_cpu = ProcessorId::INVALID;
        ret.regs_avail = AllocBitmap::new(32);
        ret.regs_dirty = AllocBitmap::new(32);
        ret.mp_state = MutilProcessorState::Runnable;
        *ret
    }

    pub fn lapic_in_kernel(&self) -> bool {
        if x86_kvm_manager().has_noapic_vcpu {
            return self.apic.is_some();
        }
        true
    }

    pub fn read_cr0_bits(&mut self, mask: Cr0) -> Cr0 {
        let tmask = mask & (Cr0::CR0_TASK_SWITCHED | Cr0::CR0_WRITE_PROTECT);
        if tmask.contains(self.cr0_guest_owned_bits)
            && !self
                .regs_avail
                .get(KvmReg::VcpuExregCr0 as usize)
                .unwrap_or_default()
        {
            x86_kvm_ops().cache_reg(self, KvmReg::VcpuExregCr0);
        }

        return self.cr0 & mask;
    }

    pub fn read_cr4_bits(&mut self, mask: Cr4) -> Cr4 {
        let tmask = mask
            & (Cr4::CR4_VIRTUAL_INTERRUPTS
                | Cr4::CR4_DEBUGGING_EXTENSIONS
                | Cr4::CR4_ENABLE_PPMC
                | Cr4::CR4_ENABLE_SSE
                | Cr4::CR4_UNMASKED_SSE
                | Cr4::CR4_ENABLE_GLOBAL_PAGES
                | Cr4::CR4_TIME_STAMP_DISABLE
                | Cr4::CR4_ENABLE_FSGSBASE);

        if tmask.contains(self.cr4_guest_owned_bits)
            && !self
                .regs_avail
                .get(KvmReg::VcpuExregCr4 as usize)
                .unwrap_or_default()
        {
            x86_kvm_ops().cache_reg(self, KvmReg::VcpuExregCr4)
        }

        return self.cr4 & mask;
    }

    #[inline]
    pub fn is_smm(&self) -> bool {
        self.hflags.contains(HFlags::HF_SMM_MASK)
    }

    #[inline]
    pub fn is_guest_mode(&self) -> bool {
        self.hflags.contains(HFlags::HF_GUEST_MASK)
    }

    #[inline]
    fn clear_interrupt_queue(&mut self) {
        self.interrupt.injected = false;
    }

    #[inline]
    fn clear_exception_queue(&mut self) {
        self.exception.pending = false;
        self.exception.injected = false;
        self.exception_vmexit.pending = false;
    }

    pub fn set_msr(&mut self, index: u32, data: u64, host_initiated: bool) {
        match index {
            IA32_FS_BASE | IA32_GS_BASE | IA32_KERNEL_GSBASE | IA32_CSTAR | IA32_LSTAR => {
                if VirtAddr::new(data as usize).is_canonical() {
                    return;
                }
            }

            IA32_SYSENTER_EIP | IA32_SYSENTER_ESP => {
                // 需要将Data转为合法地址，但是现在先这样写
                assert!(VirtAddr::new(data as usize).is_canonical());
            }
            IA32_TSC_AUX => {
                if x86_kvm_manager()
                    .find_user_return_msr_idx(IA32_TSC_AUX)
                    .is_none()
                {
                    return;
                }

                todo!()
            }
            _ => {}
        }

        let msr_data = MsrData {
            host_initiated,
            index,
            data,
        };

        return kvm_arch_ops().set_msr(self, msr_data);
    }

    pub fn update_cpuid_runtime(&mut self, entries: &Vec<KvmCpuidEntry2>) {
        let cpuid = CpuId::new();
        let feat = cpuid.get_feature_info().unwrap();
        let base = KvmCpuidEntry2::find(entries, 1, None);
        if let Some(base) = base {
            if feat.has_xsave() {}
        }

        todo!()
    }

    #[inline]
    fn mark_register_dirty(&mut self, reg: KvmReg) {
        self.regs_avail.set(reg as usize, true);
        self.regs_dirty.set(reg as usize, true);
    }

    #[inline]
    fn write_reg(&mut self, reg: KvmReg, data: u64) {
        self.regs[reg as usize] = data;
    }

    #[inline]
    fn write_reg_raw(&mut self, reg: KvmReg, data: u64) {
        self.regs[reg as usize] = data;
        self.mark_register_dirty(reg);
    }

    #[inline]
    fn read_reg(&self, reg: KvmReg) -> u64 {
        return self.regs[reg as usize];
    }

    #[inline]
    fn read_reg_raw(&self, reg: KvmReg) -> u64 {
        if self.regs_avail.get(reg as usize) == Some(true) {
            kvm_arch_ops().cache_reg(self, reg);
        }

        return self.regs[reg as usize];
    }

    #[inline]
    fn get_linear_rip(&mut self) -> u64 {
        if self.guest_state_protected {
            return 0;
        }
        return self.read_reg_raw(KvmReg::VcpuRegsRip);
    }
}

impl VirtCpu {
    pub fn init_arch(&mut self, vm: &Vm) {
        self.arch.last_vmentry_cpu = ProcessorId::INVALID;
        self.arch.regs_avail.set_all(true);
        self.arch.regs_dirty.set_all(true);

        if vm.arch.irqchip_mode != KvmIrqChipMode::None || vm.arch.bsp_vcpu_id == self.vcpu_id {
            self.arch.mp_state = MutilProcessorState::Runnable;
        } else {
            self.arch.mp_state = MutilProcessorState::Uninitialized;
        }

        self.arch.vcpu_arch_mmu_create();

        if vm.arch.irqchip_mode != KvmIrqChipMode::None {
            todo!()
        } else {
            x86_kvm_manager_mut().has_noapic_vcpu = true;
        }

        x86_kvm_ops().vcpu_create(self, vm);

        self.load();
        self.vcpu_reset(false);
        self.arch.kvm_init_mmu();
    }

    pub fn run(&mut self) -> Result<usize, SystemError> {
        self.load();
        todo!()
    }

    #[inline]
    fn load(&mut self) {
        self.arch_vcpu_load(smp_get_processor_id())
    }

    fn arch_vcpu_load(&mut self, cpu: ProcessorId) {
        x86_kvm_ops().vcpu_load(self, cpu);

        self.arch.host_pkru = KvmX86Asm::read_pkru();

        // 下列两个TODO为处理时钟信息
        if unlikely(self.arch.tsc_offset_adjustment != 0) {
            todo!()
        }

        if unlikely(self.cpu != cpu) {
            // TODO: 设置tsc
            self.cpu = cpu;
        }

        self.request(VirCpuRequest::KVM_REQ_STEAL_UPDATE)
    }

    pub fn request(&mut self, req: VirCpuRequest) {
        self.request.insert(req);
    }

    pub fn vcpu_reset(&mut self, init_event: bool) {
        let old_cr0 = self.arch.read_cr0_bits(Cr0::all());

        if self.arch.is_guest_mode() {
            todo!()
        }

        // ：TODO
        // self.lapic_reset(init_event);

        self.arch.hflags = HFlags::empty();

        self.arch.smi_pending = false;
        self.arch.smi_count = 0;
        self.arch.nmi_queued = 0;
        self.arch.nmi_pending = 0;
        self.arch.nmi_injected = false;

        self.arch.clear_exception_queue();
        self.arch.clear_interrupt_queue();

        for i in &mut self.arch.db {
            *i = 0;
        }

        // TODO: kvm_update_dr0123(vcpu);

        // DR6_ACTIVE_LOW
        self.arch.dr6 = 0xffff0ff0;
        // DR7_FIXED_1
        self.arch.dr7 = 0x00000400;

        // TODO: kvm_update_dr7(vcpu);

        self.arch.cr2 = 0;

        self.request(VirCpuRequest::KVM_REQ_EVENT);

        self.arch.apf.msr_en_val = 0;
        self.arch.apf.msr_int_val = 0;
        // TODO:st

        // TODO: kvmclock_reset(vcpu);

        // TODO: kvm_clear_async_pf_completion_queue(vcpu);

        for i in &mut self.arch.apf.gfns {
            *i = u64::MAX;
        }

        self.arch.apf.halted = false;

        // TODO: fpu

        if !init_event {
            // TODO:pmu
            self.arch.smbase = 0x30000;

            self.arch.msr_misc_features_enables = 0;
            self.arch.ia32_misc_enable_msr = MiscEnable::MSR_IA32_MISC_ENABLE_PEBS_UNAVAIL
                | MiscEnable::MSR_IA32_MISC_ENABLE_BTS_UNAVAIL;

            // TODO: __kvm_set_xcr(vcpu, 0, XFEATURE_MASK_FP);
            // 0xda0: MSR_IA32_XSS
            self.arch.set_msr(0xda0, 0, true);
        }

        for reg in &mut self.arch.regs {
            *reg = 0;
        }

        self.arch.mark_register_dirty(KvmReg::VcpuRegsRsp);

        let cpuid_0x1 = KvmCpuidEntry2::find(&self.arch.cpuid_entries, 1, None);
        let val = if let Some(cpuid) = cpuid_0x1 {
            cpuid.eax
        } else {
            0x600
        };
        self.arch.write_reg(KvmReg::VcpuRegsRdx, val as u64);

        kvm_arch_ops().vcpu_reset(self, init_event);

        self.set_rflags(RFlags::FLAGS_A1);
        self.arch.write_reg_raw(KvmReg::VcpuRegsRip, 0xfff0);

        self.arch.cr3 = 0;
        self.arch.mark_register_dirty(KvmReg::VcpuExregCr3);

        let mut new_cr0 = Cr0::CR0_EXTENSION_TYPE;
        if init_event {
            new_cr0.insert(old_cr0 & (Cr0::CR0_NOT_WRITE_THROUGH | Cr0::CR0_CACHE_DISABLE));
        } else {
            new_cr0.insert(Cr0::CR0_NOT_WRITE_THROUGH | Cr0::CR0_CACHE_DISABLE);
        }

        kvm_arch_ops().set_cr0(self, new_cr0);
        kvm_arch_ops().set_cr4(self, Cr4::empty());
        kvm_arch_ops().set_efer(self, EferFlags::empty());
        kvm_arch_ops().update_exception_bitmap(self);

        if old_cr0.contains(Cr0::CR0_ENABLE_PAGING) {
            self.request(VirCpuRequest::KVM_REQ_TLB_FLUSH_GUEST);
            self.arch.reset_mmu_context();
        }

        if init_event {
            self.request(VirCpuRequest::KVM_REQ_TLB_FLUSH_GUEST);
        }
    }

    fn set_rflags(&mut self, rflags: RFlags) {
        self._set_rflags(rflags);
        self.request(VirCpuRequest::KVM_REQ_EVENT);
    }

    fn _set_rflags(&mut self, mut rflags: RFlags) {
        if self.guest_debug.contains(GuestDebug::SINGLESTEP)
            && self.is_linear_rip(self.arch.single_step_rip)
        {
            rflags.insert(RFlags::FLAGS_TF);
        }

        kvm_arch_ops().set_rflags(self, rflags);
    }

    fn get_rflags(&self) -> RFlags {
        let mut rflags = kvm_arch_ops().get_rflags(self);
        if self.guest_debug.contains(GuestDebug::SINGLESTEP) {
            rflags.insert(RFlags::FLAGS_TF);
        }
        return rflags;
    }

    fn is_linear_rip(&mut self, linear_rip: usize) -> bool {
        return self.arch.get_linear_rip() == linear_rip as u64;
    }

    pub fn get_regs(&mut self) -> KvmCommonRegs {
        self.load();
        return self._get_regs();
    }

    fn _get_regs(&self) -> KvmCommonRegs {
        KvmCommonRegs {
            rax: self.arch.read_reg(KvmReg::VcpuRegsRax),
            rbx: self.arch.read_reg(KvmReg::VcpuRegsRbx),
            rcx: self.arch.read_reg(KvmReg::VcpuRegsRcx),
            rdx: self.arch.read_reg(KvmReg::VcpuRegsRdx),
            rsi: self.arch.read_reg(KvmReg::VcpuRegsRsi),
            rdi: self.arch.read_reg(KvmReg::VcpuRegsRdi),
            rsp: self.arch.read_reg(KvmReg::VcpuRegsRsp),
            rbp: self.arch.read_reg(KvmReg::VcpuRegsRbp),
            r8: self.arch.read_reg(KvmReg::VcpuRegsR8),
            r9: self.arch.read_reg(KvmReg::VcpuRegsR9),
            r10: self.arch.read_reg(KvmReg::VcpuRegsR10),
            r11: self.arch.read_reg(KvmReg::VcpuRegsR11),
            r12: self.arch.read_reg(KvmReg::VcpuRegsR12),
            r13: self.arch.read_reg(KvmReg::VcpuRegsR13),
            r14: self.arch.read_reg(KvmReg::VcpuRegsR14),
            r15: self.arch.read_reg(KvmReg::VcpuRegsR15),
            rip: self.arch.read_reg_raw(KvmReg::VcpuRegsRip),
            rflags: self.get_rflags().bits(),
        }
    }
}

bitflags! {
    pub struct VirCpuRequest: u32 {
        const KVM_REQUEST_NO_WAKEUP = 1 << 0;
        const KVM_REQUEST_WAIT = 1 << 1;
        const KVM_REQUEST_NO_ACTION = 1 << 2;
        const KVM_REQ_EVENT = 1 << 6;
        const KVM_REQ_STEAL_UPDATE = 1 << 8;
        const KVM_REQ_TLB_FLUSH_GUEST = 1 << 27 | Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits;
        const KVM_REQ_TLB_FLUSH = 1 | Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits;
    }
}

#[derive(Debug, Default)]
pub struct KvmQueuedInterrupt {
    pub injected: bool,
    pub soft: bool,
    pub nr: u8,
}

#[derive(Debug, Default)]
pub struct KvmQueuedException {
    pending: bool,
    injected: bool,
    has_error_code: bool,
    vector: u8,
    error_code: u32,
    payload: usize,
    has_payload: bool,
}

#[derive(Debug)]
pub struct KvmAsyncPageFault {
    /// 是否处于停止状态
    halted: bool,
    /// 存储异步页面错误的 GFN（Guest Frame Number）
    gfns: [u64; Self::ASYNC_PF_PER_VCPU],
    /// 用于 GFN 到 HVA（Host Virtual Address）的缓存
    data: GfnToHvaCache,
    /// MSR_KVM_ASYNC_PF_EN 寄存器的值
    msr_en_val: u64,
    /// MSR_KVM_ASYNC_PF_INT 寄存器的值
    msr_int_val: u64,
    /// 异步 PF 的向量
    vec: u16,
    /// 异步 PF 的 ID
    id: u32,
    /// 是否仅发送给用户空间
    send_user_only: bool,
    /// 主机 APF 标志
    host_apf_flags: u32,
    /// 是否作为页面错误 VMExit 传递
    delivery_as_pf_vmexit: bool,
    /// 是否处于页面就绪挂起状态
    pageready_pending: bool,
}

impl KvmAsyncPageFault {
    pub const ASYNC_PF_PER_VCPU: usize = 64;
}
