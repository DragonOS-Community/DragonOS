use core::intrinsics::likely;
use core::{arch::x86_64::_xsetbv, intrinsics::unlikely};

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use bitmap::{traits::BitMapOps, AllocBitmap, BitMapCore};
use log::warn;
use raw_cpuid::CpuId;
use system_error::SystemError;
use x86::vmx::vmcs::guest;
use x86::{
    bits64::rflags::RFlags,
    controlregs::{Cr0, Cr4, Xcr0},
    dtables::DescriptorTablePointer,
    msr::{self, wrmsr},
    vmx::vmcs::control::SecondaryControls,
};
use x86_64::registers::control::EferFlags;

use crate::arch::vm::asm::VmxAsm;
use crate::arch::vm::vmx::exit::ExitFastpathCompletion;
use crate::virt::vm::kvm_host::mem::KvmMmuMemoryCache;
use crate::virt::vm::kvm_host::vcpu::VcpuMode;
use crate::{
    arch::{
        kvm_arch_ops,
        mm::barrier,
        vm::{
            asm::{hyperv, kvm_msr, KvmX86Asm, MiscEnable, MsrData, VcpuSegment},
            cpuid::KvmCpuidEntry2,
            kvm_host::KvmReg,
            mmu::kvm_mmu::LockedKvmMmu,
            uapi::{UapiKvmSegmentRegs, KVM_SYNC_X86_VALID_FIELDS},
            vmx::{vmcs::ControlsType, vmx_info},
            x86_kvm_manager, x86_kvm_manager_mut, x86_kvm_ops,
        },
    },
    mm::VirtAddr,
    smp::{core::smp_get_processor_id, cpu::ProcessorId},
    virt::vm::{
        kvm_host::{
            mem::GfnToHvaCache,
            vcpu::{GuestDebug, VirtCpu},
            MutilProcessorState, Vm,
        },
        user_api::{UapiKvmRun, UapiKvmSegment},
    },
};

use super::{lapic::KvmLapic, HFlags, KvmCommonRegs, KvmIrqChipMode};
const MSR_IA32_CR_PAT_DEFAULT: u64 = 0x0007_0406_0007_0406;
#[allow(dead_code)]
#[derive(Debug)]
pub struct X86VcpuArch {
    /// 最近一次尝试进入虚拟机的主机cpu
    pub last_vmentry_cpu: ProcessorId,
    /// 可用寄存器位图
    pub regs_avail: AllocBitmap,
    /// 脏寄存器位图
    pub regs_dirty: AllocBitmap,
    /// 多处理器状态
    mp_state: MutilProcessorState,
    pub apic_base: u64,
    /// apic
    pub apic: Option<KvmLapic>,
    /// 主机pkru寄存器
    host_pkru: u32,
    pkru: u32,
    /// hflag
    hflags: HFlags,

    pub microcode_version: u64,

    arch_capabilities: u64,

    perf_capabilities: u64,

    ia32_xss: u64,

    pub guest_state_protected: bool,

    pub cpuid_entries: Vec<KvmCpuidEntry2>,

    pub exception: KvmQueuedException,
    pub exception_vmexit: KvmQueuedException,
    pub apf: KvmAsyncPageFault,

    pub emulate_regs_need_sync_from_vcpu: bool,
    pub emulate_regs_need_sync_to_vcpu: bool,

    pub smbase: u64,

    pub interrupt: KvmQueuedInterrupt,

    pub tsc_offset_adjustment: u64,

    pub mmu: Option<Arc<LockedKvmMmu>>,
    pub root_mmu: Option<Arc<LockedKvmMmu>>,
    pub guset_mmu: Option<Arc<LockedKvmMmu>>,
    pub walk_mmu: Option<Arc<LockedKvmMmu>>,
    pub nested_mmu: Option<Arc<LockedKvmMmu>>,

    pub mmu_pte_list_desc_cache: KvmMmuMemoryCache,
    pub mmu_shadow_page_cache: KvmMmuMemoryCache,
    pub mmu_shadowed_info_cache: KvmMmuMemoryCache,
    pub mmu_page_header_cache: KvmMmuMemoryCache,

    pub max_phyaddr: usize,

    pub pat: u64,

    pub regs: [u64; KvmReg::NrVcpuRegs as usize],

    pub cr0: Cr0,
    pub cr0_guest_owned_bits: Cr0,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: Cr4,
    pub cr4_guest_owned_bits: Cr4,
    pub cr4_guest_rsvd_bits: Cr4,
    pub cr8: u64,
    pub efer: EferFlags,

    pub xcr0: Xcr0,

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

    pub handling_intr_from_guest: KvmIntrType,

    pub xfd_no_write_intercept: bool,

    pub l1tf_flush_l1d: bool,

    pub at_instruction_boundary: bool,

    pub db: [usize; Self::KVM_NR_DB_REGS],

    /* set at EPT violation at this point */
    pub exit_qual: u64,
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

        ret.apic = None;
        //max_phyaddr=?? fztodo
        *ret
    }

    pub fn clear_dirty(&mut self) {
        self.regs_dirty.set_all(false);
    }

    pub fn vcpu_apicv_active(&self) -> bool {
        self.lapic_in_kernel() && self.lapic().apicv_active
    }

    pub fn lapic_in_kernel(&self) -> bool {
        if x86_kvm_manager().has_noapic_vcpu {
            return self.apic.is_some();
        }
        true
    }

    pub fn is_bsp(&self) -> bool {
        return self.apic_base & msr::IA32_APIC_BASE as u64 != 0;
    }

    #[inline]
    pub fn lapic(&self) -> &KvmLapic {
        self.apic.as_ref().unwrap()
    }

    pub fn queue_interrupt(&mut self, vec: u8, soft: bool) {
        self.interrupt.injected = true;
        self.interrupt.soft = soft;
        self.interrupt.nr = vec;
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

    pub fn get_cr8(&self) -> u64 {
        if self.lapic_in_kernel() {
            todo!()
        } else {
            return self.cr8;
        }
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
    pub fn is_long_mode(&self) -> bool {
        self.efer.contains(EferFlags::LONG_MODE_ACTIVE)
    }

    #[inline]
    #[allow(dead_code)]
    pub fn is_pae_paging(&mut self) -> bool {
        let flag1 = self.is_long_mode();
        let flag2 = self.is_pae();
        let flag3 = self.is_paging();

        !flag1 && flag2 && flag3
    }

    #[inline]
    pub fn is_pae(&mut self) -> bool {
        !self.read_cr4_bits(Cr4::CR4_ENABLE_PAE).is_empty()
    }
    #[inline]
    pub fn is_paging(&mut self) -> bool {
        //return likely(kvm_is_cr0_bit_set(vcpu, X86_CR0_PG));
        !self.read_cr0_bits(Cr0::CR0_ENABLE_PAGING).is_empty()
    }

    #[inline]
    pub fn is_portected_mode(&mut self) -> bool {
        !self.read_cr0_bits(Cr0::CR0_PROTECTED_MODE).is_empty()
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

    #[allow(dead_code)]
    pub fn update_cpuid_runtime(&mut self, entries: &Vec<KvmCpuidEntry2>) {
        let cpuid = CpuId::new();
        let feat = cpuid.get_feature_info().unwrap();
        let base = KvmCpuidEntry2::find(entries, 1, None);
        if let Some(_base) = base {
            if feat.has_xsave() {}
        }

        todo!()
    }

    #[inline]
    pub fn test_and_mark_available(&mut self, reg: KvmReg) -> bool {
        let old = self.regs_avail.get(reg as usize).unwrap_or_default();
        self.regs_avail.set(reg as usize, true);
        return old;
    }

    #[inline]
    pub fn mark_register_dirty(&mut self, reg: KvmReg) {
        self.regs_avail.set(reg as usize, true);
        self.regs_dirty.set(reg as usize, true);
    }

    #[inline]
    pub fn mark_register_available(&mut self, reg: KvmReg) {
        self.regs_avail.set(reg as usize, true);
    }

    #[inline]
    pub fn is_register_dirty(&self, reg: KvmReg) -> bool {
        self.regs_dirty.get(reg as usize).unwrap()
    }

    #[inline]
    pub fn is_register_available(&self, reg: KvmReg) -> bool {
        self.regs_avail.get(reg as usize).unwrap()
    }

    #[inline]
    pub fn write_reg(&mut self, reg: KvmReg, data: u64) {
        self.regs[reg as usize] = data;
    }

    #[inline]
    pub fn write_reg_raw(&mut self, reg: KvmReg, data: u64) {
        self.regs[reg as usize] = data;
        self.mark_register_dirty(reg);
    }

    #[inline]
    pub fn read_reg(&self, reg: KvmReg) -> u64 {
        return self.regs[reg as usize];
    }

    #[inline]
    pub fn read_reg_raw(&mut self, reg: KvmReg) -> u64 {
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

    pub fn set_msr_common(&mut self, msr_info: &MsrData) {
        let msr = msr_info.index;
        let data = msr_info.data;

        match msr {
            // MSR_AMD64_NB_CFG
            0xc001001f => {
                return;
            }
            // MSR_VM_HSAVE_PA
            0xc0010117 => {
                return;
            }
            // MSR_AMD64_PATCH_LOADER
            0xc0010020 => {
                return;
            }
            // MSR_AMD64_BU_CFG2
            0xc001102a => {
                return;
            }
            // MSR_AMD64_DC_CFG
            0xc0011022 => {
                return;
            }
            // MSR_AMD64_TW_CFG
            0xc0011023 => {
                return;
            }
            // MSR_F15H_EX_CFG
            0xc001102c => {
                return;
            }
            msr::IA32_BIOS_UPDT_TRIG => {
                return;
            }
            msr::IA32_BIOS_SIGN_ID => {
                // MSR_IA32_UCODE_REV
                if msr_info.host_initiated {
                    self.microcode_version = data;
                }
                return;
            }
            // MSR_IA32_ARCH_CAPABILITIES
            0x0000010a => {
                if !msr_info.host_initiated {
                    return;
                }

                self.arch_capabilities = data;
            }
            msr::MSR_PERF_CAPABILITIES => {
                if !msr_info.host_initiated {
                    return;
                }

                if data & (!x86_kvm_manager().kvm_caps.supported_perf_cap) != 0 {
                    return;
                }

                if self.perf_capabilities == data {
                    return;
                }

                self.perf_capabilities = data;
                // todo: kvm_pmu_refresh
                return;
            }
            // MSR_IA32_FLUSH_CMD
            0x0000010b => {
                todo!()
            }
            msr::IA32_EFER => {
                todo!()
            }
            // MSR_K7_HWCR
            0xc0010015 => {
                todo!()
            }
            // MSR_FAM10H_MMIO_CONF_BASE
            0xc0010058 => {
                todo!()
            }
            msr::IA32_PAT => {
                todo!()
            }
            // MTRRphysBase_MSR(0) ... MSR_MTRRfix4K_F8000 | MSR_MTRRdefType
            0x200..=0x26f | 0x2ff => {
                todo!()
            }
            msr::APIC_BASE => {
                todo!()
            }
            // APIC_BASE_MSR ... APIC_BASE_MSR + 0xff
            0x800..=0x8ff => {
                todo!()
            }
            msr::IA32_TSC_DEADLINE => {
                todo!()
            }
            msr::IA32_TSC_ADJUST => {
                todo!()
            }
            msr::IA32_MISC_ENABLE => {
                todo!()
            }
            msr::IA32_SMBASE => {
                todo!()
            }
            msr::TSC => {
                todo!()
            }
            // MSR_IA32_XSS
            msr::MSR_C5_PMON_BOX_CTRL => {
                if !msr_info.host_initiated {
                    return;
                }
                if data & (!x86_kvm_manager().kvm_caps.supported_xss) != 0 {
                    return;
                }

                self.ia32_xss = data;
                // TODO:kvm_update_cpuid_runtime
                return;
            }
            msr::MSR_SMI_COUNT => {
                todo!()
            }
            kvm_msr::MSR_KVM_WALL_CLOCK_NEW => {
                todo!()
            }
            kvm_msr::MSR_KVM_WALL_CLOCK => {
                todo!()
            }
            kvm_msr::MSR_KVM_SYSTEM_TIME => {
                todo!()
            }
            kvm_msr::MSR_KVM_ASYNC_PF_EN => {
                todo!()
            }
            kvm_msr::MSR_KVM_ASYNC_PF_INT => {
                todo!()
            }
            kvm_msr::MSR_KVM_ASYNC_PF_ACK => {
                todo!()
            }
            kvm_msr::MSR_KVM_STEAL_TIME => {
                todo!()
            }
            kvm_msr::MSR_KVM_PV_EOI_EN => {
                todo!()
            }
            kvm_msr::MSR_KVM_POLL_CONTROL => {
                todo!()
            }
            msr::MCG_CTL
            | msr::MCG_STATUS
            | msr::MC0_CTL..=msr::MSR_MC26_MISC
            | msr::IA32_MC0_CTL2..=msr::IA32_MC21_CTL2 => {
                todo!()
            }
            // MSR_K7_PERFCTR0 ... MSR_K7_PERFCTR3
            // MSR_K7_PERFCTR0 ... MSR_K7_PERFCTR3
            // MSR_K7_EVNTSEL0 ... MSR_K7_EVNTSEL3
            // MSR_P6_EVNTSEL0 ... MSR_P6_EVNTSEL1
            0xc0010004..=0xc0010007
            | 0xc1..=0xc2
            | 0xc0010000..=0xc0010003
            | 0x00000186..=0x00000187 => {
                todo!()
            }

            // MSR_K7_CLK_CTL
            0xc001001b => {
                return;
            }

            hyperv::HV_X64_MSR_GUEST_OS_ID..=hyperv::HV_REGISTER_SINT15
            | hyperv::HV_X64_MSR_SYNDBG_CONTROL..=hyperv::HV_X64_MSR_SYNDBG_PENDING_BUFFER
            | hyperv::HV_X64_MSR_SYNDBG_OPTIONS
            | hyperv::HV_REGISTER_CRASH_P0..=hyperv::HV_REGISTER_CRASH_P4
            | hyperv::HV_REGISTER_CRASH_CTL
            | hyperv::HV_REGISTER_STIMER0_CONFIG..=hyperv::HV_REGISTER_STIMER3_COUNT
            | hyperv::HV_X64_MSR_REENLIGHTENMENT_CONTROL
            | hyperv::HV_X64_MSR_TSC_EMULATION_CONTROL
            | hyperv::HV_X64_MSR_TSC_EMULATION_STATUS
            | hyperv::HV_X64_MSR_TSC_INVARIANT_CONTROL => {
                todo!()
            }

            msr::MSR_BBL_CR_CTL3 => {
                todo!()
            }

            // MSR_AMD64_OSVW_ID_LENGTH
            0xc0010140 => {
                todo!()
            }
            // MSR_AMD64_OSVW_STATUS
            0xc0010141 => {
                todo!()
            }

            msr::MSR_PLATFORM_INFO => {
                todo!()
            }
            // MSR_MISC_FEATURES_ENABLES
            0x00000140 => {
                todo!()
            }
            // MSR_IA32_XFD
            0x000001c4 => {
                todo!()
            }
            // MSR_IA32_XFD_ERR
            0x000001c5 => {
                todo!()
            }
            _ => {
                todo!()
            }
        }
    }

    pub fn kvm_before_interrupt(&mut self, intr: KvmIntrType) {
        barrier::mfence();
        self.handling_intr_from_guest = intr;
        barrier::mfence();
    }

    pub fn kvm_after_interrupt(&mut self) {
        barrier::mfence();
        self.handling_intr_from_guest = KvmIntrType::None;
        barrier::mfence();
    }
}

impl VirtCpu {
    pub fn init_arch(&mut self, vm: &mut Vm, id: usize) -> Result<(), SystemError> {
        //kvm_arch_vcpu_create
        vm.vcpu_precreate(id)?;

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

        //lots of todo!!!

        self.arch.pat = MSR_IA32_CR_PAT_DEFAULT;

        self.load();
        self.vcpu_reset(vm, false)?;
        self.arch.kvm_init_mmu();

        Ok(())
    }

    #[inline]
    pub fn kvm_run(&self) -> &UapiKvmRun {
        self.run.as_ref().unwrap()
    }

    #[inline]
    pub fn kvm_run_mut(&mut self) -> &mut Box<UapiKvmRun> {
        self.run.as_mut().unwrap()
    }

    pub fn run(&mut self) -> Result<usize, SystemError> {
        self.load();

        if unlikely(self.arch.mp_state == MutilProcessorState::Uninitialized) {
            todo!()
        }

        if self.kvm_run().kvm_valid_regs & !KVM_SYNC_X86_VALID_FIELDS != 0
            || self.kvm_run().kvm_dirty_regs & !KVM_SYNC_X86_VALID_FIELDS != 0
        {
            return Err(SystemError::EINVAL);
        }

        if self.kvm_run().kvm_dirty_regs != 0 {
            todo!()
        }

        if !self.arch.lapic_in_kernel() {
            self.kvm_set_cr8(self.kvm_run().cr8);
        }

        // TODO: https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/x86.c#11174 - 11196

        if self.kvm_run().immediate_exit != 0 {
            return Err(SystemError::EINTR);
        }

        // vmx_vcpu_pre_run

        self.vcpu_run(&self.kvm().lock())?;

        Ok(0)
    }

    fn vcpu_run(&mut self, vm: &Vm) -> Result<(), SystemError> {
        self.arch.l1tf_flush_l1d = true;

        loop {
            self.arch.at_instruction_boundary = false;
            if self.can_running() {
                self.enter_guest(vm)?;
            } else {
                todo!()
            };
        }
    }

    fn enter_guest(&mut self, vm: &Vm) -> Result<(), SystemError> {
        let req_immediate_exit = false;

        warn!("request {:?}", self.request);
        if !self.request.is_empty() {
            if self.check_request(VirtCpuRequest::KVM_REQ_VM_DEAD) {
                return Err(SystemError::EIO);
            }

            // TODO: kvm_dirty_ring_check_request

            if self.check_request(VirtCpuRequest::KVM_REQ_MMU_FREE_OBSOLETE_ROOTS) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_MIGRATE_TIMER) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_MASTERCLOCK_UPDATE) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_GLOBAL_CLOCK_UPDATE) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_CLOCK_UPDATE) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_MMU_SYNC) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_LOAD_MMU_PGD) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_TLB_FLUSH) {
                self.flush_tlb_all();
            }

            self.service_local_tlb_flush_requests();

            // TODO: KVM_REQ_HV_TLB_FLUSH) && kvm_hv_vcpu_flush_tlb(vcpu)

            if self.check_request(VirtCpuRequest::KVM_REQ_REPORT_TPR_ACCESS) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_TRIPLE_FAULT) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_STEAL_UPDATE) {
                // todo!()
                warn!("VirtCpuRequest::KVM_REQ_STEAL_UPDATE TODO!");
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_SMI) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_NMI) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_PMU) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_PMI) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_IOAPIC_EOI_EXIT) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_SCAN_IOAPIC) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_LOAD_EOI_EXITMAP) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_APIC_PAGE_RELOAD) {
                // todo!()
                warn!("VirtCpuRequest::KVM_REQ_APIC_PAGE_RELOAD TODO!");
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_HV_CRASH) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_HV_RESET) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_HV_EXIT) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_HV_STIMER) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_APICV_UPDATE) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_APF_READY) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_MSR_FILTER_CHANGED) {
                todo!()
            }

            if self.check_request(VirtCpuRequest::KVM_REQ_UPDATE_CPU_DIRTY_LOGGING) {
                todo!()
            }
        }

        // TODO: https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/x86.c#10661
        if self.check_request(VirtCpuRequest::KVM_REQ_EVENT) {
            // TODO
        }

        self.kvm_mmu_reload(vm)?;

        x86_kvm_ops().prepare_switch_to_guest(self);
        // warn!(
        //     "mode {:?} req {:?} mode_cond {} !is_empty {} cond {}",
        //     self.mode,
        //     self.request,
        //     self.mode == VcpuMode::ExitingGuestMode,
        //     !self.request.is_empty(),
        //     (self.mode == VcpuMode::ExitingGuestMode) || (!self.request.is_empty())
        // );
        warn!(
            "req bit {} empty bit {}",
            self.request.bits,
            VirtCpuRequest::empty().bits
        );
        // TODO: https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/x86.c#10730
        if self.mode == VcpuMode::ExitingGuestMode || !self.request.is_empty() {
            self.mode = VcpuMode::OutsideGuestMode;
            return Err(SystemError::EINVAL);
        }

        if req_immediate_exit {
            self.request(VirtCpuRequest::KVM_REQ_EVENT);
            todo!();
        }

        // TODO: https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/x86.c#10749 - 10766

        let exit_fastpath;
        loop {
            exit_fastpath = x86_kvm_ops().vcpu_run(self);
            if likely(exit_fastpath != ExitFastpathCompletion::ExitHandled) {
                break;
            }

            todo!();
        }

        // TODO: https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/x86.c#10799 - 10814

        self.arch.last_vmentry_cpu = self.cpu;

        // TODO: last_guest_tsc

        self.mode = VcpuMode::OutsideGuestMode;

        barrier::mfence();

        // TODO: xfd

        x86_kvm_ops().handle_exit_irqoff(self);

        // todo: xfd

        // TODO: 一些中断或者tsc操作

        match x86_kvm_ops().handle_exit(self, vm, exit_fastpath) {
            Err(err) => return Err(err),
            Ok(_) => Ok(()),
        }
    }

    fn flush_tlb_all(&mut self) {
        x86_kvm_ops().flush_tlb_all(self);
        self.clear_request(VirtCpuRequest::KVM_REQ_TLB_FLUSH_CURRENT);
    }

    fn service_local_tlb_flush_requests(&mut self) {
        if self.check_request(VirtCpuRequest::KVM_REQ_TLB_FLUSH_CURRENT) {
            todo!()
        }

        if self.check_request(VirtCpuRequest::KVM_REQ_TLB_FLUSH_GUEST) {
            todo!()
        }
    }

    pub fn request(&mut self, req: VirtCpuRequest) {
        // self.request.set(
        //     (req.bits() & VirtCpuRequest::KVM_REQUEST_MASK.bits()) as usize,
        //     true,
        // );
        self.request.insert(req);
    }

    fn check_request(&mut self, req: VirtCpuRequest) -> bool {
        if self.test_request(req) {
            self.clear_request(req);

            barrier::mfence();
            return true;
        }

        return false;
    }

    fn test_request(&self, req: VirtCpuRequest) -> bool {
        // self.request
        //     .get((req.bits & VirtCpuRequest::KVM_REQUEST_MASK.bits) as usize)
        //     .unwrap_or_default()
        self.request.contains(req)
    }

    fn clear_request(&mut self, req: VirtCpuRequest) {
        // self.request.set(
        //     (req.bits & VirtCpuRequest::KVM_REQUEST_MASK.bits) as usize,
        //     false,
        // );
        self.request.remove(req);
    }

    pub fn can_running(&self) -> bool {
        return self.arch.mp_state == MutilProcessorState::Runnable && !self.arch.apf.halted;
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

        self.request(VirtCpuRequest::KVM_REQ_STEAL_UPDATE)
    }

    pub fn set_msr(
        &mut self,
        index: u32,
        data: u64,
        host_initiated: bool,
    ) -> Result<(), SystemError> {
        match index {
            msr::IA32_FS_BASE
            | msr::IA32_GS_BASE
            | msr::IA32_KERNEL_GSBASE
            | msr::IA32_CSTAR
            | msr::IA32_LSTAR => {
                if VirtAddr::new(data as usize).is_canonical() {
                    return Ok(());
                }
            }

            msr::IA32_SYSENTER_EIP | msr::IA32_SYSENTER_ESP => {
                // 需要将Data转为合法地址，但是现在先这样写
                assert!(VirtAddr::new(data as usize).is_canonical());
            }
            msr::IA32_TSC_AUX => {
                if x86_kvm_manager()
                    .find_user_return_msr_idx(msr::IA32_TSC_AUX)
                    .is_none()
                {
                    return Ok(());
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

    pub fn vcpu_reset(&mut self, vm: &Vm, init_event: bool) -> Result<(), SystemError> {
        let old_cr0 = self.arch.read_cr0_bits(Cr0::all());

        if self.arch.is_guest_mode() {
            todo!()
        }

        self.lapic_reset(vm, init_event);

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

        self.request(VirtCpuRequest::KVM_REQ_EVENT);

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
            self.set_msr(0xda0, 0, true)?;
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

        kvm_arch_ops().vcpu_reset(self, vm, init_event);

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

        kvm_arch_ops().set_cr0(vm, self, new_cr0);
        kvm_arch_ops().set_cr4(self, Cr4::empty());
        kvm_arch_ops().set_efer(self, EferFlags::empty());
        kvm_arch_ops().update_exception_bitmap(self);

        if old_cr0.contains(Cr0::CR0_ENABLE_PAGING) {
            self.request(VirtCpuRequest::MAKE_KVM_REQ_TLB_FLUSH_GUEST);
            self.arch.reset_mmu_context();
        }

        if init_event {
            self.request(VirtCpuRequest::MAKE_KVM_REQ_TLB_FLUSH_GUEST);
        }

        Ok(())
    }

    fn set_rflags(&mut self, rflags: RFlags) {
        self._set_rflags(rflags);
        self.request(VirtCpuRequest::KVM_REQ_EVENT);
    }

    fn _set_rflags(&mut self, mut rflags: RFlags) {
        if self.guest_debug.contains(GuestDebug::SINGLESTEP)
            && self.is_linear_rip(self.arch.single_step_rip)
        {
            rflags.insert(RFlags::FLAGS_TF);
        }

        kvm_arch_ops().set_rflags(self, rflags);
    }

    fn get_rflags(&mut self) -> RFlags {
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

    fn _get_regs(&mut self) -> KvmCommonRegs {
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

    pub fn get_segment_regs(&mut self) -> UapiKvmSegmentRegs {
        self.load();
        return self._get_segment_regs();
    }

    fn _get_segment_regs(&mut self) -> UapiKvmSegmentRegs {
        let mut sregs = self._get_segment_regs_common();

        if self.arch.guest_state_protected {
            return sregs;
        }

        if self.arch.interrupt.injected && !self.arch.interrupt.soft {
            BitMapCore::new().set(
                sregs.interrupt_bitmap.len() * core::mem::size_of::<u64>(),
                &mut sregs.interrupt_bitmap,
                self.arch.interrupt.nr as usize,
                true,
            );
        }

        return sregs;
    }

    fn read_cr3(&mut self) -> u64 {
        if !self.arch.is_register_available(KvmReg::VcpuExregCr3) {
            x86_kvm_ops().cache_reg(&mut self.arch, KvmReg::VcpuExregCr3);
        }
        return self.arch.cr3;
    }

    fn kvm_get_segment(&mut self, segment: &mut UapiKvmSegment, seg: VcpuSegment) {
        *segment = x86_kvm_ops().get_segment(self, *segment, seg);
    }

    fn _get_segment_regs_common(&mut self) -> UapiKvmSegmentRegs {
        let mut sregs = UapiKvmSegmentRegs::default();

        if !self.arch.guest_state_protected {
            let mut dt = DescriptorTablePointer::default();

            self.kvm_get_segment(&mut sregs.cs, VcpuSegment::CS);
            self.kvm_get_segment(&mut sregs.ds, VcpuSegment::DS);
            self.kvm_get_segment(&mut sregs.es, VcpuSegment::ES);
            self.kvm_get_segment(&mut sregs.fs, VcpuSegment::FS);
            self.kvm_get_segment(&mut sregs.gs, VcpuSegment::GS);
            self.kvm_get_segment(&mut sregs.ss, VcpuSegment::SS);

            self.kvm_get_segment(&mut sregs.tr, VcpuSegment::TR);
            self.kvm_get_segment(&mut sregs.ldt, VcpuSegment::LDTR);

            x86_kvm_ops().get_idt(self, &mut dt);
            sregs.idt.limit = dt.limit;
            sregs.idt.base = dt.base as usize as u64;

            x86_kvm_ops().get_gdt(self, &mut dt);
            sregs.gdt.limit = dt.limit;
            sregs.gdt.base = dt.base as usize as u64;

            sregs.cr2 = self.arch.cr2;
            sregs.cr3 = self.read_cr3();
        }

        sregs.cr0 = self.arch.read_cr0_bits(Cr0::all()).bits() as u64;
        sregs.cr4 = self.arch.read_cr4_bits(Cr4::all()).bits() as u64;
        sregs.cr8 = self.arch.get_cr8();
        sregs.efer = self.arch.efer.bits();
        sregs.apic_base = self.arch.apic_base;

        return sregs;
    }

    pub fn set_segment_regs(&mut self, sregs: &mut UapiKvmSegmentRegs) -> Result<(), SystemError> {
        self.load();
        self._set_segmenet_regs(&self.kvm().lock(), sregs)?;
        Ok(())
    }

    fn _set_segmenet_regs(
        &mut self,
        vm: &Vm,
        sregs: &mut UapiKvmSegmentRegs,
    ) -> Result<(), SystemError> {
        let mut mmu_reset_needed = false;
        self._set_segmenet_regs_common(vm, sregs, &mut mmu_reset_needed, true)?;

        if mmu_reset_needed {
            todo!()
        }

        // KVM_NR_INTERRUPTS
        let max_bits = 256;

        let pending_vec = BitMapCore::new().first_index(&sregs.interrupt_bitmap);
        if let Some(pending) = pending_vec {
            if pending < max_bits {
                self.arch.queue_interrupt(pending as u8, false);

                self.request(VirtCpuRequest::KVM_REQ_EVENT);
            }
        }

        Ok(())
    }

    /// 设置段寄存器
    fn _set_segmenet_regs_common(
        &mut self,
        vm: &Vm,
        sregs: &mut UapiKvmSegmentRegs,
        mmu_reset_needed: &mut bool,
        update_pdptrs: bool,
    ) -> Result<(), SystemError> {
        let mut apic_base_msr = MsrData::default();

        if !self.is_valid_segment_regs(sregs) {
            return Err(SystemError::EINVAL);
        }

        apic_base_msr.data = sregs.apic_base;
        apic_base_msr.host_initiated = true;

        // TODO: kvm_set_apic_base

        if self.arch.guest_state_protected {
            return Ok(());
        }

        let mut dt: DescriptorTablePointer<u8> = DescriptorTablePointer {
            limit: sregs.idt.limit,
            base: sregs.idt.base as usize as *const u8,
        };

        x86_kvm_ops().set_idt(self, &dt);

        dt.limit = sregs.gdt.limit;
        dt.base = sregs.gdt.base as usize as *const u8;
        x86_kvm_ops().set_gdt(self, &dt);

        self.arch.cr2 = sregs.cr2;
        *mmu_reset_needed |= self.read_cr3() != sregs.cr3;

        self.arch.cr3 = sregs.cr3;

        self.arch.mark_register_dirty(KvmReg::VcpuExregCr3);

        x86_kvm_ops().post_set_cr3(self, sregs.cr3);

        //debug!("_set_segmenet_regs_common 2:: cr3: {:#x}", self.arch.cr3);

        self.kvm_set_cr8(sregs.cr8);

        let efer = EferFlags::from_bits_truncate(sregs.efer);
        *mmu_reset_needed |= self.arch.efer != efer;
        x86_kvm_ops().set_efer(self, efer);

        let cr0 = Cr0::from_bits_truncate(sregs.cr0 as usize);
        *mmu_reset_needed |= self.arch.cr0 != cr0;
        x86_kvm_ops().set_cr0(vm, self, cr0);
        self.arch.cr0 = cr0;

        let cr4 = Cr4::from_bits_truncate(sregs.cr4 as usize);
        *mmu_reset_needed |= self.arch.read_cr4_bits(Cr4::all()) != cr4;
        x86_kvm_ops().set_cr4(self, cr4);

        if update_pdptrs {
            //todo!()
        }

        x86_kvm_ops().set_segment(self, &mut sregs.cs, VcpuSegment::CS);
        x86_kvm_ops().set_segment(self, &mut sregs.ds, VcpuSegment::DS);
        x86_kvm_ops().set_segment(self, &mut sregs.es, VcpuSegment::ES);
        x86_kvm_ops().set_segment(self, &mut sregs.fs, VcpuSegment::FS);
        x86_kvm_ops().set_segment(self, &mut sregs.gs, VcpuSegment::GS);
        x86_kvm_ops().set_segment(self, &mut sregs.ss, VcpuSegment::SS);

        x86_kvm_ops().set_segment(self, &mut sregs.tr, VcpuSegment::TR);
        x86_kvm_ops().set_segment(self, &mut sregs.ldt, VcpuSegment::LDTR);

        // TODO: update_cr8_intercept

        if self.arch.is_bsp()
            && self.arch.read_reg_raw(KvmReg::VcpuRegsRip) == 0xfff0
            && sregs.cs.selector == 0xf000
            && sregs.cs.base == 0xffff0000
            && !self.arch.is_portected_mode()
        {
            self.arch.mp_state = MutilProcessorState::Runnable;
        }

        Ok(())
    }

    pub fn kvm_set_cr8(&mut self, cr8: u64) {
        // 先这样写
        self.arch.cr8 = cr8;
    }

    fn is_valid_segment_regs(&self, sregs: &UapiKvmSegmentRegs) -> bool {
        let efer = EferFlags::from_bits_truncate(sregs.efer);
        let cr4 = Cr4::from_bits_truncate(sregs.cr4 as usize);
        let cr0 = Cr0::from_bits_truncate(sregs.cr0 as usize);

        if efer.contains(EferFlags::LONG_MODE_ENABLE) && cr0.contains(Cr0::CR0_ENABLE_PAGING) {
            if !cr4.contains(Cr4::CR4_ENABLE_PAE) || !efer.contains(EferFlags::LONG_MODE_ACTIVE) {
                return false;
            }

            // TODO: legal gpa?
        } else if efer.contains(EferFlags::LONG_MODE_ACTIVE) || sregs.cs.l != 0 {
            return false;
        }
        let ret = self.kvm_is_vaild_cr0(cr0) && self.kvm_is_vaild_cr4(cr4);
        return ret;
    }

    fn kvm_is_vaild_cr0(&self, cr0: Cr0) -> bool {
        if cr0.contains(Cr0::CR0_NOT_WRITE_THROUGH) && !cr0.contains(Cr0::CR0_CACHE_DISABLE) {
            return false;
        }

        if cr0.contains(Cr0::CR0_ENABLE_PAGING) && !cr0.contains(Cr0::CR0_PROTECTED_MODE) {
            return false;
        }
        let ret = x86_kvm_ops().is_vaild_cr0(self, cr0);
        return ret;
    }

    fn __kvm_is_valid_cr4(&self, cr4: Cr4) -> bool {
        if cr4.contains(self.arch.cr4_guest_rsvd_bits) {
            //debug!("__kvm_is_valid_cr4::here");
            //return false;
        }

        return true;
    }

    fn kvm_is_vaild_cr4(&self, cr4: Cr4) -> bool {
        return self.__kvm_is_valid_cr4(cr4) && x86_kvm_ops().is_vaild_cr4(self, cr4);
    }

    pub fn is_unrestricted_guest(&self) -> bool {
        let guard = self.vmx().loaded_vmcs();
        return vmx_info().enable_unrestricted_guest
            && (!self.arch.is_guest_mode()
                || SecondaryControls::from_bits_truncate(
                    guard.controls_get(ControlsType::SecondaryExec) as u32,
                )
                .contains(SecondaryControls::UNRESTRICTED_GUEST));
    }

    pub fn set_regs(&mut self, regs: &KvmCommonRegs) -> Result<(), SystemError> {
        self.load();
        self._set_regs(regs);
        Ok(())
    }

    fn _set_regs(&mut self, regs: &KvmCommonRegs) {
        self.arch.emulate_regs_need_sync_from_vcpu = true;
        self.arch.emulate_regs_need_sync_to_vcpu = false;

        self.arch.write_reg(KvmReg::VcpuRegsRax, regs.rax);
        self.arch.write_reg(KvmReg::VcpuRegsRbx, regs.rbx);
        self.arch.write_reg(KvmReg::VcpuRegsRcx, regs.rcx);
        self.arch.write_reg(KvmReg::VcpuRegsRdx, regs.rdx);
        self.arch.write_reg(KvmReg::VcpuRegsRsi, regs.rsi);
        self.arch.write_reg(KvmReg::VcpuRegsRdi, regs.rdi);
        self.arch.write_reg(KvmReg::VcpuRegsRsp, regs.rsp);
        self.arch.write_reg(KvmReg::VcpuRegsRbp, regs.rbp);

        self.arch.write_reg(KvmReg::VcpuRegsR8, regs.r8);
        self.arch.write_reg(KvmReg::VcpuRegsR9, regs.r9);
        self.arch.write_reg(KvmReg::VcpuRegsR10, regs.r10);
        self.arch.write_reg(KvmReg::VcpuRegsR11, regs.r11);
        self.arch.write_reg(KvmReg::VcpuRegsR12, regs.r12);
        self.arch.write_reg(KvmReg::VcpuRegsR13, regs.r13);
        self.arch.write_reg(KvmReg::VcpuRegsR14, regs.r14);
        self.arch.write_reg(KvmReg::VcpuRegsR15, regs.r15);

        self.arch.write_reg_raw(KvmReg::VcpuRegsRip, regs.rip);

        self.set_rflags(RFlags::from_bits_truncate(regs.rflags) | RFlags::FLAGS_A1);

        self.arch.exception.pending = false;
        self.arch.exception_vmexit.pending = false;

        self.request(VirtCpuRequest::KVM_REQ_EVENT);
    }

    pub fn load_guest_xsave_state(&mut self) {
        if self.arch.guest_state_protected {
            return;
        }

        if !self.arch.read_cr4_bits(Cr4::CR4_ENABLE_OS_XSAVE).is_empty() {
            if self.arch.xcr0 != x86_kvm_manager().host_xcr0 {
                unsafe { _xsetbv(0, self.arch.xcr0.bits()) };
            }

            if self.arch.ia32_xss != x86_kvm_manager().host_xss {
                // XSS
                unsafe { wrmsr(0xda0, self.arch.ia32_xss) };
            }
        }

        if CpuId::new().get_extended_feature_info().unwrap().has_pku()
            && self.arch.pkru != self.arch.host_pkru
            && (self.arch.xcr0.contains(Xcr0::XCR0_PKRU_STATE)
                || !self
                    .arch
                    .read_cr4_bits(Cr4::CR4_ENABLE_PROTECTION_KEY)
                    .is_empty())
        {
            KvmX86Asm::write_pkru(self.arch.pkru);
        }
    }

    pub fn load_pdptrs(&mut self) {
        //let mmu = self.arch.mmu();
        if !self.arch.is_register_dirty(KvmReg::VcpuExregCr3) {
            return;
        }
        //if self.arch.is_pae_paging(){
        let mmu = self.arch.mmu();

        VmxAsm::vmx_vmwrite(guest::PDPTE0_FULL, mmu.pdptrs[0]);
        VmxAsm::vmx_vmwrite(guest::PDPTE0_FULL, mmu.pdptrs[1]);
        VmxAsm::vmx_vmwrite(guest::PDPTE0_FULL, mmu.pdptrs[2]);
        VmxAsm::vmx_vmwrite(guest::PDPTE0_FULL, mmu.pdptrs[3]);
        //}else{
        // debug!("load_pdptrs: not pae paging");
        //}
    }
}

bitflags! {
    // pub struct VirtCpuRequest: u64 {
    //     const KVM_REQUEST_MASK = 0xFF;

    //     const KVM_REQ_TLB_FLUSH = 0 | Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits;
    //     const KVM_REQ_VM_DEAD = 1 | Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits;

    //     const KVM_REQUEST_NO_WAKEUP = 1 << 8;
    //     const KVM_REQUEST_WAIT = 1 << 9;
    //     const KVM_REQUEST_NO_ACTION = 1 << 10;

    //     const KVM_REQ_MIGRATE_TIMER = kvm_arch_req(0);
    //     const KVM_REQ_REPORT_TPR_ACCESS = kvm_arch_req(1);
    //     const KVM_REQ_TRIPLE_FAULT = kvm_arch_req(2);
    //     const KVM_REQ_MMU_SYNC = kvm_arch_req(3);
    //     const KVM_REQ_CLOCK_UPDATE = kvm_arch_req(4);
    //     const KVM_REQ_LOAD_MMU_PGD = kvm_arch_req(5);
    //     const KVM_REQ_EVENT = kvm_arch_req(6);
    //     const KVM_REQ_APF_HALT = kvm_arch_req(7);
    //     const KVM_REQ_STEAL_UPDATE = kvm_arch_req(8);
    //     const KVM_REQ_NMI = kvm_arch_req(9);
    //     const KVM_REQ_PMU = kvm_arch_req(10);
    //     const KVM_REQ_PMI = kvm_arch_req(11);
    //     const KVM_REQ_SMI = kvm_arch_req(12);

    //     const KVM_REQ_MASTERCLOCK_UPDATE = kvm_arch_req(13);
    //     const KVM_REQ_MCLOCK_INPROGRESS = kvm_arch_req_flags(14, Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits);
    //     const KVM_REQ_SCAN_IOAPIC = kvm_arch_req_flags(15, Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits);
    //     const KVM_REQ_GLOBAL_CLOCK_UPDATE = kvm_arch_req(16);
    //     const KVM_REQ_APIC_PAGE_RELOAD = kvm_arch_req_flags(17, Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits);
    //     const KVM_REQ_HV_CRASH = kvm_arch_req(18);
    //     const KVM_REQ_IOAPIC_EOI_EXIT = kvm_arch_req(19);
    //     const KVM_REQ_HV_RESET = kvm_arch_req(20);
    //     const KVM_REQ_HV_EXIT = kvm_arch_req(21);
    //     const KVM_REQ_HV_STIMER = kvm_arch_req(22);
    //     const KVM_REQ_LOAD_EOI_EXITMAP = kvm_arch_req(23);
    //     const KVM_REQ_GET_NESTED_STATE_PAGES = kvm_arch_req(24);
    //     const KVM_REQ_APICV_UPDATE = kvm_arch_req_flags(25, Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits);
    //     const KVM_REQ_TLB_FLUSH_CURRENT = kvm_arch_req(26);

    //     const KVM_REQ_TLB_FLUSH_GUEST = kvm_arch_req_flags(27, Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits);
    //     const KVM_REQ_APF_READY = kvm_arch_req(28);
    //     const KVM_REQ_MSR_FILTER_CHANGED = kvm_arch_req(29);
    //     const KVM_REQ_UPDATE_CPU_DIRTY_LOGGING  = kvm_arch_req_flags(30, Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits);
    //     const KVM_REQ_MMU_FREE_OBSOLETE_ROOTS = kvm_arch_req_flags(31, Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits);
    //     const KVM_REQ_HV_TLB_FLUSH = kvm_arch_req_flags(32, Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits);
    // }

    pub struct VirtCpuRequest: u64 {
        // const KVM_REQUEST_MASK = 0xFF;

        const KVM_REQ_TLB_FLUSH = Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits;
        const KVM_REQ_VM_DEAD = 1;

        const KVM_REQUEST_NO_WAKEUP = 1 << 8;
        const KVM_REQUEST_WAIT = 1 << 9;
        const KVM_REQUEST_NO_ACTION = 1 << 10;

        const KVM_REQ_MIGRATE_TIMER = kvm_arch_req(0);
        const KVM_REQ_REPORT_TPR_ACCESS = kvm_arch_req(1);
        const KVM_REQ_TRIPLE_FAULT = kvm_arch_req(2);
        const KVM_REQ_MMU_SYNC = kvm_arch_req(3);
        const KVM_REQ_CLOCK_UPDATE = kvm_arch_req(4);
        const KVM_REQ_LOAD_MMU_PGD = kvm_arch_req(5);
        const KVM_REQ_EVENT = kvm_arch_req(6);
        const KVM_REQ_APF_HALT = kvm_arch_req(7);
        const KVM_REQ_STEAL_UPDATE = kvm_arch_req(8);
        const KVM_REQ_NMI = kvm_arch_req(9);
        const KVM_REQ_PMU = kvm_arch_req(10);
        const KVM_REQ_PMI = kvm_arch_req(11);
        const KVM_REQ_SMI = kvm_arch_req(12);

        const KVM_REQ_MASTERCLOCK_UPDATE = kvm_arch_req(13);

        const KVM_REQ_MCLOCK_INPROGRESS = kvm_arch_req(14);
        const MAKE_KVM_REQ_MCLOCK_INPROGRESS = kvm_arch_req_flags(14, Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits);

        const KVM_REQ_SCAN_IOAPIC = kvm_arch_req(15);
        const MAKE_KVM_REQ_SCAN_IOAPIC = kvm_arch_req_flags(15, Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits);


        const KVM_REQ_GLOBAL_CLOCK_UPDATE = kvm_arch_req(16);

        const KVM_REQ_APIC_PAGE_RELOAD = kvm_arch_req(17);
        const MAKE_KVM_REQ_APIC_PAGE_RELOAD = kvm_arch_req_flags(17, Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits);

        const KVM_REQ_HV_CRASH = kvm_arch_req(18);
        const KVM_REQ_IOAPIC_EOI_EXIT = kvm_arch_req(19);
        const KVM_REQ_HV_RESET = kvm_arch_req(20);
        const KVM_REQ_HV_EXIT = kvm_arch_req(21);
        const KVM_REQ_HV_STIMER = kvm_arch_req(22);
        const KVM_REQ_LOAD_EOI_EXITMAP = kvm_arch_req(23);
        const KVM_REQ_GET_NESTED_STATE_PAGES = kvm_arch_req(24);

        const KVM_REQ_APICV_UPDATE = kvm_arch_req(25);
        const MAKE_KVM_REQ_APICV_UPDATE = kvm_arch_req_flags(25, Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits);

        const KVM_REQ_TLB_FLUSH_CURRENT = kvm_arch_req(26);

        const KVM_REQ_TLB_FLUSH_GUEST = kvm_arch_req(27);
        const MAKE_KVM_REQ_TLB_FLUSH_GUEST = kvm_arch_req_flags(27, Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits);

        const KVM_REQ_APF_READY = kvm_arch_req(28);
        const KVM_REQ_MSR_FILTER_CHANGED = kvm_arch_req(29);

        const KVM_REQ_UPDATE_CPU_DIRTY_LOGGING  = kvm_arch_req(30);
        const MAKE_KVM_REQ_UPDATE_CPU_DIRTY_LOGGING  = kvm_arch_req_flags(30, Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits);

        const KVM_REQ_MMU_FREE_OBSOLETE_ROOTS = kvm_arch_req(31);
        const MAKE_KVM_REQ_MMU_FREE_OBSOLETE_ROOTS = kvm_arch_req_flags(31, Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits);

        const KVM_REQ_HV_TLB_FLUSH = kvm_arch_req(32);
        const MAKE_KVM_REQ_HV_TLB_FLUSH = kvm_arch_req_flags(32, Self::KVM_REQUEST_WAIT.bits | Self::KVM_REQUEST_NO_WAKEUP.bits);
    }
}

// const KVM_REQUEST_ARCH_BASE: u64 = 8;
const KVM_REQUEST_ARCH_BASE: u64 = 11;

const fn kvm_arch_req(nr: u64) -> u64 {
    return kvm_arch_req_flags(nr, 0);
}

const fn kvm_arch_req_flags(nr: u64, flags: u64) -> u64 {
    1 << (nr + KVM_REQUEST_ARCH_BASE) | flags
}

#[derive(Debug, Default)]
pub struct KvmQueuedInterrupt {
    pub injected: bool,
    pub soft: bool,
    pub nr: u8,
}

#[derive(Debug, Default)]
#[allow(dead_code)]
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
#[allow(dead_code)]
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

#[derive(Debug)]
pub enum KvmIntrType {
    None,
    Irq,
    // Nmi,
}
