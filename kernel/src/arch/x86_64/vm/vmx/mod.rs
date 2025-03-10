use core::intrinsics::likely;
use core::intrinsics::unlikely;
use core::sync::atomic::{AtomicBool, Ordering};
use exit::VmxExitHandlers;
use log::debug;
use log::error;
use log::warn;
use x86_64::registers::control::Cr3Flags;
use x86_64::structures::paging::PhysFrame;

use crate::arch::process::table::USER_DS;
use crate::arch::vm::mmu::kvm_mmu::KvmMmu;
use crate::arch::vm::uapi::kvm_exit;
use crate::arch::vm::uapi::{
    AC_VECTOR, BP_VECTOR, DB_VECTOR, GP_VECTOR, MC_VECTOR, NM_VECTOR, PF_VECTOR, UD_VECTOR,
};
use crate::arch::vm::vmx::vmcs::VmcsIntrHelper;
use crate::libs::spinlock::SpinLockGuard;
use crate::mm::VirtAddr;
use crate::process::ProcessManager;
use crate::virt::vm::kvm_host::vcpu::GuestDebug;
use crate::{
    arch::{
        vm::{
            asm::KvmX86Asm,
            kvm_host::{vcpu::VirtCpuRequest, X86KvmArch},
            vmx::vmcs::vmx_area,
        },
        CurrentIrqArch, MMArch, VirtCpuArch,
    },
    exception::InterruptArch,
    libs::spinlock::SpinLock,
    mm::{
        percpu::{PerCpu, PerCpuVar},
        MemoryManagementArch,
    },
    smp::{core::smp_get_processor_id, cpu::ProcessorId},
    virt::vm::{kvm_dev::kvm_init, kvm_host::vcpu::VirtCpu, user_api::UapiKvmSegment},
};
use alloc::{alloc::Global, boxed::Box, collections::LinkedList, sync::Arc, vec::Vec};
use asm::VMX_EPTP_AD_ENABLE_BIT;
use asm::VMX_EPTP_MT_WB;
use asm::VMX_EPTP_PWL_4;
use asm::VMX_EPTP_PWL_5;
use bitfield_struct::bitfield;
use bitmap::{traits::BitMapOps, AllocBitmap};
use raw_cpuid::CpuId;
use system_error::SystemError;
use x86::controlregs::{cr2, cr2_write};
use x86::dtables::ldtr;
use x86::msr::wrmsr;
use x86::segmentation::load_ds;
use x86::segmentation::load_es;
use x86::segmentation::{ds, es, fs, gs};
use x86::vmx::vmcs::ro;
use x86::{
    bits64::rflags::RFlags,
    controlregs::{cr0, cr4, Cr0, Cr4, Xcr0},
    msr::{self, rdmsr},
    segmentation::{self},
    vmx::vmcs::{
        control::{
            self, EntryControls, ExitControls, PinbasedControls, PrimaryControls, SecondaryControls,
        },
        guest, host,
    },
};
use x86_64::registers::control::Cr3;
use x86_64::{instructions::tables::sidt, registers::control::EferFlags};

use crate::{
    arch::{
        vm::{vmx::vmcs::feat::VmxFeat, x86_kvm_manager_mut, McgCap},
        KvmArch,
    },
    libs::rwlock::RwLock,
    virt::vm::kvm_host::Vm,
};

use self::exit::ExitFastpathCompletion;
use self::exit::VmxExitReason;
use self::exit::VmxExitReasonBasic;
use self::vmcs::LoadedVmcs;
use self::{
    capabilities::{ProcessorTraceMode, VmcsConfig, VmxCapability},
    vmcs::{
        current_loaded_vmcs_list_mut, current_vmcs, current_vmcs_mut, ControlsType,
        LockedLoadedVmcs, VMControlStructure, VmxMsrBitmapAccess, VmxMsrBitmapAction,
        PERCPU_LOADED_VMCS_LIST, PERCPU_VMCS, VMXAREA,
    },
};

use super::asm::IntrInfo;
use super::asm::SegmentCacheField;
use super::kvm_host::vcpu::KvmIntrType;
use super::kvm_host::RMODE_TSS_SIZE;
use super::x86_kvm_ops;
use super::{
    asm::{VcpuSegment, VmxAsm, VmxMsrEntry},
    init_kvm_arch,
    kvm_host::{KvmFunc, KvmInitFunc, KvmIrqChipMode, KvmReg, MsrFilterType, NotifyVmExitFlags},
    x86_kvm_manager, KvmArchManager,
};

pub mod asm;
pub mod capabilities;
pub mod ept;
pub mod exit;
pub mod vmcs;

extern "C" {
    fn vmx_vmexit();
}

pub struct VmxKvmInitFunc;

impl VmxKvmInitFunc {
    pub fn setup_per_cpu(&self) {
        let mut vmcs_areas = Vec::new();
        vmcs_areas.resize(PerCpu::MAX_CPU_NUM as usize, VMControlStructure::new());
        unsafe { VMXAREA = PerCpuVar::new(vmcs_areas) };

        let mut percpu_current_vmcs = Vec::new();
        percpu_current_vmcs.resize(PerCpu::MAX_CPU_NUM as usize, None);
        unsafe { PERCPU_VMCS = PerCpuVar::new(percpu_current_vmcs) }

        let mut percpu_loaded_vmcs_lists = Vec::new();
        percpu_loaded_vmcs_lists.resize(PerCpu::MAX_CPU_NUM as usize, LinkedList::new());
        unsafe { PERCPU_LOADED_VMCS_LIST = PerCpuVar::new(percpu_loaded_vmcs_lists) }
    }
}

impl KvmInitFunc for VmxKvmInitFunc {
    #[allow(clippy::borrow_interior_mutable_const)]
    #[inline(never)]
    fn hardware_setup(&self) -> Result<(), SystemError> {
        let idt = sidt();
        let cpuid = CpuId::new();
        let cpu_extend_feature = cpuid
            .get_extended_processor_and_feature_identifiers()
            .ok_or(SystemError::ENOSYS)?;

        let mut vmx_init: Box<Vmx> = unsafe {
            Box::try_new_zeroed_in(Global)
                .map_err(|_| SystemError::ENOMEM)?
                .assume_init()
        };

        vmx_init.init();

        vmx_init.host_idt_base = idt.base.as_u64();
        Vmx::set_up_user_return_msrs();

        Vmx::setup_vmcs_config(&mut vmx_init.vmcs_config, &mut vmx_init.vmx_cap)?;

        let manager = x86_kvm_manager_mut();
        let kvm_cap = &mut manager.kvm_caps;

        if vmx_init.has_mpx() {
            kvm_cap.supported_xcr0 &= !(Xcr0::XCR0_BNDREG_STATE | Xcr0::XCR0_BNDCSR_STATE);
        }

        // 判断是否启用vpid
        if !vmx_init.has_vpid()
            || !vmx_init.has_invvpid()
            || !vmx_init.has_invvpid_single()
            || !vmx_init.has_invvpid_global()
        {
            vmx_init.enable_vpid = false;
        }

        if !vmx_init.has_ept()
            || !vmx_init.has_ept_4levels()
            || !vmx_init.has_ept_mt_wb()
            || !vmx_init.has_invept_global()
        {
            vmx_init.enable_ept = false;
        }

        // 是否启用了 EPT 并且检查 CPU 是否支持 Execute Disable（NX）功能
        // Execute Disable 是一种 CPU 功能，可以防止代码在数据内存区域上执行
        if !vmx_init.enable_ept && !cpu_extend_feature.has_execute_disable() {
            error!("[KVM] NX (Execute Disable) not supported");
            return Err(SystemError::ENOSYS);
        }

        if !vmx_init.has_ept_ad_bits() || !vmx_init.enable_ept {
            vmx_init.enable_ept_ad = false;
        }

        if !vmx_init.has_unrestricted_guest() || !vmx_init.enable_ept {
            vmx_init.enable_unrestricted_guest = false;
        }

        if !vmx_init.has_flexproirity() {
            vmx_init.enable_flexpriority = false;
        }

        if !vmx_init.has_virtual_nmis() {
            vmx_init.enable_vnmi = false;
        }

        if !vmx_init.has_encls_vmexit() {
            vmx_init.enable_sgx = false;
        }

        if !vmx_init.enable_flexpriority {
            VmxKvmFunc::CONFIG.write().have_set_apic_access_page_addr = false;
        }

        if !vmx_init.has_tpr_shadow() {
            VmxKvmFunc::CONFIG.write().have_update_cr8_intercept = false;
        }

        // TODO:https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/vmx/vmx.c#8501 - 8513

        if !vmx_init.has_ple() {
            vmx_init.ple_gap = 0;
            vmx_init.ple_window = 0;
            vmx_init.ple_window_grow = 0;
            vmx_init.ple_window_max = 0;
            vmx_init.ple_window_shrink = 0;
        }

        if !vmx_init.has_apicv() {
            vmx_init.enable_apicv = false;
        }

        if !vmx_init.enable_apicv {
            // TODO: 设置sync_pir_to_irr
        }

        if !vmx_init.enable_apicv || !vmx_init.has_ipiv() {
            vmx_init.enable_ipiv = false;
        }

        if vmx_init.has_tsc_scaling() {
            kvm_cap.has_tsc_control = true;
        }

        kvm_cap.max_tsc_scaling_ratio = 0xffffffffffffffff;
        kvm_cap.tsc_scaling_ratio_frac_bits = 48;
        kvm_cap.has_bus_lock_exit = vmx_init.has_bus_lock_detection();
        kvm_cap.has_notify_vmexit = vmx_init.has_notify_vmexit();

        // vmx_init.vpid_bitmap.lock().set_all(false);

        if vmx_init.enable_ept {
            // TODO: mmu_set_ept_masks
            warn!("mmu_set_ept_masks TODO!");
        }

        warn!("vmx_setup_me_spte_mask TODO!");

        KvmMmu::kvm_configure_mmu(
            vmx_init.enable_ept,
            0,
            vmx_init.get_max_ept_level(),
            vmx_init.ept_cap_to_lpage_level(),
        );

        if !vmx_init.enable_ept || !vmx_init.enable_ept_ad || !vmx_init.has_pml() {
            vmx_init.enable_pml = false;
        }

        if !vmx_init.enable_pml {
            // TODO: Set cpu dirty log size
        }

        if !vmx_init.has_preemption_timer() {
            vmx_init.enable_preemption_timer = false;
        }

        if vmx_init.enable_preemption_timer {
            // TODO
        }

        if !vmx_init.enable_preemption_timer {
            // TODO
        }

        kvm_cap
            .supported_mce_cap
            .insert(McgCap::MCG_LMCE_P | McgCap::MCG_CMCI_P);

        // TODO: pt_mode

        // TODO: setup_default_sgx_lepubkeyhash

        // TODO: nested

        // TODO: vmx_set_cpu_caps
        init_vmx(vmx_init);
        self.setup_per_cpu();

        warn!("hardware setup finish");
        Ok(())
    }

    fn handle_intel_pt_intr(&self) -> u32 {
        todo!()
    }

    fn runtime_funcs(&self) -> &'static dyn super::kvm_host::KvmFunc {
        &VmxKvmFunc
    }
}

#[derive(Debug)]
pub struct VmxKvmFunc;

pub struct VmxKvmFuncConfig {
    pub have_set_apic_access_page_addr: bool,
    pub have_update_cr8_intercept: bool,
}

impl VmxKvmFunc {
    #[allow(clippy::declare_interior_mutable_const)]
    pub const CONFIG: RwLock<VmxKvmFuncConfig> = RwLock::new(VmxKvmFuncConfig {
        have_set_apic_access_page_addr: true,
        have_update_cr8_intercept: true,
    });

    pub fn vcpu_load_vmcs(
        vcpu: &mut VirtCpu,
        cpu: ProcessorId,
        _buddy: Option<Arc<LockedLoadedVmcs>>,
    ) {
        let vmx = vcpu.vmx();
        let already_loaded = vmx.loaded_vmcs.lock().cpu == cpu;

        if !already_loaded {
            Self::loaded_vmcs_clear(&vmx.loaded_vmcs);
            let _irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

            current_loaded_vmcs_list_mut().push_back(vmx.loaded_vmcs.clone());
        }

        if let Some(prev) = current_vmcs() {
            let vmcs = vmx.loaded_vmcs.lock().vmcs.clone();
            if !Arc::ptr_eq(&vmcs, prev) {
                VmxAsm::vmcs_load(vmcs.phys_addr());
                *current_vmcs_mut() = Some(vmcs);

                // TODO:buddy barrier?
            }
        } else {
            let vmcs = vmx.loaded_vmcs.lock().vmcs.clone();
            VmxAsm::vmcs_load(vmcs.phys_addr());
            *current_vmcs_mut() = Some(vmcs);

            // TODO:buddy barrier?
        }

        if !already_loaded {
            let mut pseudo_descriptpr: x86::dtables::DescriptorTablePointer<u64> =
                Default::default();
            unsafe {
                x86::dtables::sgdt(&mut pseudo_descriptpr);
            };

            vmx.loaded_vmcs.lock().cpu = cpu;
            let id = vmx.loaded_vmcs.lock().vmcs.lock().revision_id();
            debug!(
                "revision_id {id} req {:?}",
                VirtCpuRequest::KVM_REQ_TLB_FLUSH
            );
            vcpu.request(VirtCpuRequest::KVM_REQ_TLB_FLUSH);

            VmxAsm::vmx_vmwrite(
                host::TR_BASE,
                KvmX86Asm::get_segment_base(
                    pseudo_descriptpr.base,
                    pseudo_descriptpr.limit,
                    unsafe { x86::task::tr().bits() },
                ),
            );

            VmxAsm::vmx_vmwrite(host::GDTR_BASE, pseudo_descriptpr.base as usize as u64);

            VmxAsm::vmx_vmwrite(host::IA32_SYSENTER_ESP, unsafe {
                rdmsr(msr::IA32_SYSENTER_ESP)
            });
        }
    }

    pub fn loaded_vmcs_clear(loaded_vmcs: &Arc<LockedLoadedVmcs>) {
        let mut guard = loaded_vmcs.lock();
        if guard.cpu == ProcessorId::INVALID {
            return;
        }

        if guard.cpu == smp_get_processor_id() {
            if let Some(vmcs) = current_vmcs() {
                if Arc::ptr_eq(vmcs, &guard.vmcs) {
                    *current_vmcs_mut() = None;
                }
            }

            VmxAsm::vmclear(guard.vmcs.phys_addr());

            if let Some(shadow) = &guard.shadow_vmcs {
                if guard.launched {
                    VmxAsm::vmclear(shadow.phys_addr());
                }
            }

            let _ = current_loaded_vmcs_list_mut().extract_if(|x| Arc::ptr_eq(x, loaded_vmcs));

            guard.cpu = ProcessorId::INVALID;
            guard.launched = false;
        } else {
            // 交由对应cpu处理
            todo!()
        }
    }

    pub fn seg_setup(&self, seg: VcpuSegment) {
        let seg_field = &KVM_VMX_SEGMENT_FIELDS[seg as usize];

        VmxAsm::vmx_vmwrite(seg_field.selector, 0);
        VmxAsm::vmx_vmwrite(seg_field.base, 0);
        VmxAsm::vmx_vmwrite(seg_field.limit, 0xffff);

        let mut ar = 0x93;
        if seg == VcpuSegment::CS {
            ar |= 0x08;
        }
        VmxAsm::vmx_vmwrite(seg_field.ar_bytes, ar);
    }
}

impl KvmFunc for VmxKvmFunc {
    fn name(&self) -> &'static str {
        "VMX"
    }

    fn hardware_enable(&self) -> Result<(), SystemError> {
        let vmcs = vmx_area().get().as_ref();

        debug!("vmcs idx {}", vmcs.abort);

        let phys_addr =
            unsafe { MMArch::virt_2_phys(VirtAddr::new(vmcs as *const _ as usize)).unwrap() };

        // TODO: intel_pt_handle_vmx(1);

        VmxAsm::kvm_cpu_vmxon(phys_addr)?;

        Ok(())
    }

    fn vm_init(&self) -> X86KvmArch {
        let vmx_init = vmx_info();

        let mut arch = X86KvmArch::default();
        if vmx_init.ple_gap == 0 {
            arch.pause_in_guest = true;
        }

        return arch;
    }

    fn vcpu_create(&self, vcpu: &mut VirtCpu, vm: &Vm) {
        VmxVCpuPriv::init(vcpu, vm);
    }

    fn vcpu_load(&self, vcpu: &mut VirtCpu, cpu: crate::smp::cpu::ProcessorId) {
        Self::vcpu_load_vmcs(vcpu, cpu, None);
        // TODO: vmx_vcpu_pi_load
    }

    fn cache_reg(&self, vcpu: &mut VirtCpuArch, reg: KvmReg) {
        vcpu.mark_register_available(reg);

        match reg {
            KvmReg::VcpuRegsRsp => {
                vcpu.regs[reg as usize] = VmxAsm::vmx_vmread(guest::RSP);
            }
            KvmReg::VcpuRegsRip => {
                vcpu.regs[reg as usize] = VmxAsm::vmx_vmread(guest::RIP);
            }
            // VCPU_EXREG_PDPTR
            KvmReg::NrVcpuRegs => {
                if vmx_info().enable_ept {
                    todo!()
                }
            }
            KvmReg::VcpuExregCr0 => {
                let guest_owned = vcpu.cr0_guest_owned_bits;

                vcpu.cr0.remove(guest_owned);
                vcpu.cr0.insert(
                    Cr0::from_bits_truncate(VmxAsm::vmx_vmread(guest::CR0) as usize) & guest_owned,
                );
            }
            KvmReg::VcpuExregCr3 => {
                //当拦截CR3加载时（例如用于影子分页），KVM（Kernel-based Virtual Machine）的CR3会被加载到硬件中，而不是客户机的CR3。
                //暂时先直接读寄存器
                vcpu.cr3 = VmxAsm::vmx_vmread(guest::CR3);
                //todo!()
            }
            KvmReg::VcpuExregCr4 => {
                let guest_owned = vcpu.cr4_guest_owned_bits;

                vcpu.cr4.remove(guest_owned);
                vcpu.cr4.insert(
                    Cr4::from_bits_truncate(VmxAsm::vmx_vmread(guest::CR4) as usize) & guest_owned,
                );
            }
            _ => {
                todo!()
            }
        }
    }

    fn apicv_pre_state_restore(&self, _vcpu: &mut VirtCpu) {
        // https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/vmx/vmx.c#6924
        // TODO: pi
        // todo!()
    }

    fn set_msr(&self, vcpu: &mut VirtCpu, msr: super::asm::MsrData) -> Result<(), SystemError> {
        let vmx = vcpu.vmx_mut();
        let msr_index = msr.index;
        let data = msr.data;

        match msr_index {
            msr::IA32_EFER => {
                todo!("IA32_EFER")
            }

            msr::IA32_FS_BASE => {
                todo!("IA32_FS_BASE")
            }

            msr::IA32_GS_BASE => {
                todo!("IA32_GS_BASE")
            }

            msr::IA32_KERNEL_GSBASE => {
                todo!("IA32_KERNEL_GSBASE")
            }

            0x000001c4 => {
                todo!("MSR_IA32_XFD")
            }

            msr::IA32_SYSENTER_CS => {
                todo!("IA32_SYSENTER_CS")
            }

            msr::IA32_SYSENTER_EIP => {
                todo!("IA32_SYSENTER_EIP")
            }

            msr::IA32_SYSENTER_ESP => {
                todo!("IA32_SYSENTER_ESP")
            }

            msr::IA32_DEBUGCTL => {
                todo!("IA32_DEBUGCTL")
            }

            msr::MSR_C1_PMON_EVNT_SEL0 => {
                todo!("MSR_IA32_BNDCFGS")
            }

            0xe1 => {
                todo!("MSR_IA32_UMWAIT_CONTROL	")
            }

            0x48 => {
                todo!("MSR_IA32_SPEC_CTRL")
            }

            msr::MSR_IA32_TSX_CTRL => {
                todo!("MSR_IA32_TSX_CTRL")
            }

            msr::IA32_PAT => {
                todo!("IA32_PAT")
            }

            0x4d0 => {
                todo!("MSR_IA32_MCG_EXT_CTL")
            }

            msr::IA32_FEATURE_CONTROL => {
                todo!("IA32_FEATURE_CONTROL")
            }

            0x8c..=0x8f => {
                todo!("MSR_IA32_SGXLEPUBKEYHASH0 ... MSR_IA32_SGXLEPUBKEYHASH3 {msr_index}")
            }

            msr::IA32_VMX_BASIC..=msr::IA32_VMX_VMFUNC => {
                todo!("msr::IA32_VMX_BASIC..=msr::IA32_VMX_VMFUNC")
            }

            msr::MSR_IA32_RTIT_CTL => {
                todo!("MSR_IA32_RTIT_CTL")
            }

            msr::MSR_IA32_RTIT_STATUS => {
                todo!("MSR_IA32_RTIT_STATUS")
            }

            msr::MSR_IA32_RTIT_OUTPUT_BASE => {
                todo!("MSR_IA32_RTIT_OUTPUT_BASE")
            }

            0x572 => {
                todo!("MSR_IA32_RTIT_CR3_MATCH")
            }

            msr::MSR_IA32_RTIT_OUTPUT_MASK_PTRS => {
                todo!("MSR_IA32_RTIT_OUTPUT_MASK_PTRS")
            }

            msr::MSR_IA32_ADDR0_START..=msr::MSR_IA32_ADDR3_END => {
                todo!("msr::MSR_IA32_ADDR0_START..=msr::MSR_IA32_ADDR3_END")
            }

            msr::MSR_PERF_CAPABILITIES => {
                todo!("MSR_PERF_CAPABILITIES")
            }

            _ => {
                let uret_msr = vmx.find_uret_msr(msr_index);

                if let Some((idx, _msr)) = uret_msr {
                    vmx.set_guest_uret_msr(idx, data)?;
                    vmx.set_uret_msr(msr_index, data);
                } else {
                    vcpu.arch.set_msr_common(&msr);
                };
            }
        }

        if msr_index == 0x10a {
            // MSR_IA32_ARCH_CAPABILITIES
            todo!()
        }

        Ok(())
    }

    fn vcpu_reset(&self, vcpu: &mut VirtCpu, vm: &Vm, init_event: bool) {
        if !init_event {
            vmx_info_mut().vmx_reset_vcpu(vcpu, vm)
        }
        vcpu.kvm_set_cr8(0);

        let vmx = vcpu.vmx_mut();
        vmx.rmode.vm86_active = false;
        vmx.spec_ctrl = 0;
        vmx.msr_ia32_umwait_control = 0;
        vmx.hv_deadline_tsc = u64::MAX;

        vmx.segment_cache_clear();

        vcpu.arch.mark_register_available(KvmReg::VcpuExregSegments);

        self.seg_setup(VcpuSegment::CS);
        VmxAsm::vmx_vmwrite(guest::CS_SELECTOR, 0xf000);
        VmxAsm::vmx_vmwrite(guest::CS_BASE, 0xffff0000);

        self.seg_setup(VcpuSegment::DS);
        self.seg_setup(VcpuSegment::ES);
        self.seg_setup(VcpuSegment::FS);
        self.seg_setup(VcpuSegment::GS);
        self.seg_setup(VcpuSegment::SS);

        VmxAsm::vmx_vmwrite(guest::TR_SELECTOR, 0);
        VmxAsm::vmx_vmwrite(guest::TR_BASE, 0);
        VmxAsm::vmx_vmwrite(guest::TR_LIMIT, 0xffff);
        VmxAsm::vmx_vmwrite(guest::TR_ACCESS_RIGHTS, 0x008b);

        VmxAsm::vmx_vmwrite(guest::LDTR_SELECTOR, 0);
        VmxAsm::vmx_vmwrite(guest::LDTR_BASE, 0);
        VmxAsm::vmx_vmwrite(guest::LDTR_LIMIT, 0xffff);
        VmxAsm::vmx_vmwrite(guest::LDTR_ACCESS_RIGHTS, 0x00082);

        VmxAsm::vmx_vmwrite(guest::GDTR_BASE, 0);
        VmxAsm::vmx_vmwrite(guest::GDTR_LIMIT, 0xffff);

        VmxAsm::vmx_vmwrite(guest::IDTR_BASE, 0);
        VmxAsm::vmx_vmwrite(guest::IDTR_LIMIT, 0xffff);

        VmxAsm::vmx_vmwrite(guest::ACTIVITY_STATE, 0);
        VmxAsm::vmx_vmwrite(guest::INTERRUPTIBILITY_STATE, 0);
        VmxAsm::vmx_vmwrite(guest::PENDING_DBG_EXCEPTIONS, 0);

        if x86_kvm_manager().mpx_supported() {
            VmxAsm::vmx_vmwrite(guest::IA32_BNDCFGS_FULL, 0);
        }

        VmxAsm::vmx_vmwrite(control::VMENTRY_INTERRUPTION_INFO_FIELD, 0);

        vcpu.request(VirtCpuRequest::MAKE_KVM_REQ_APIC_PAGE_RELOAD);

        vmx_info().vpid_sync_context(vcpu.vmx().vpid);

        warn!("TODO: vmx_update_fb_clear_dis");
    }

    fn set_rflags(&self, vcpu: &mut VirtCpu, mut rflags: x86::bits64::rflags::RFlags) {
        if vcpu.is_unrestricted_guest() {
            vcpu.arch.mark_register_available(KvmReg::VcpuExregRflags);
            vcpu.vmx_mut().rflags = rflags;
            VmxAsm::vmx_vmwrite(guest::RFLAGS, rflags.bits());
            return;
        }

        let old_rflags = self.get_rflags(vcpu);

        let vmx = vcpu.vmx_mut();

        vmx.rflags = rflags;
        if vmx.rmode.vm86_active {
            vmx.rmode.save_rflags = rflags;
            rflags.insert(RFlags::FLAGS_IOPL3 | RFlags::FLAGS_VM);
        }

        VmxAsm::vmx_vmwrite(guest::RFLAGS, rflags.bits());

        if (old_rflags ^ vmx.rflags).contains(RFlags::FLAGS_VM) {
            let emulation_required = vmx_info().emulation_required(vcpu);
            vcpu.vmx_mut().emulation_required = emulation_required;
        }
    }

    fn set_cr0(&self, vm: &Vm, vcpu: &mut VirtCpu, cr0: x86::controlregs::Cr0) {
        let old_cr0_pg = vcpu.arch.read_cr0_bits(Cr0::CR0_ENABLE_PAGING);
        let mut hw_cr0 = cr0 & (!(Cr0::CR0_NOT_WRITE_THROUGH | Cr0::CR0_CACHE_DISABLE));

        if vmx_info().enable_unrestricted_guest {
            hw_cr0.insert(Cr0::CR0_NUMERIC_ERROR);
        } else {
            hw_cr0
                .insert(Cr0::CR0_NUMERIC_ERROR | Cr0::CR0_ENABLE_PAGING | Cr0::CR0_PROTECTED_MODE);

            if !vmx_info().enable_ept {
                hw_cr0.insert(Cr0::CR0_WRITE_PROTECT);
            }

            if vcpu.vmx().rmode.vm86_active && cr0.contains(Cr0::CR0_PROTECTED_MODE) {
                vmx_info().enter_pmode(vcpu);
            }

            if !vcpu.vmx().rmode.vm86_active && !cr0.contains(Cr0::CR0_PROTECTED_MODE) {
                vmx_info().enter_rmode(vcpu, vm);
            }
        }

        VmxAsm::vmx_vmwrite(control::CR0_READ_SHADOW, cr0.bits() as u64);
        VmxAsm::vmx_vmwrite(guest::CR0, hw_cr0.bits() as u64);

        vcpu.arch.cr0 = cr0;

        vcpu.arch.mark_register_available(KvmReg::VcpuExregCr0);

        if vcpu.arch.efer.contains(EferFlags::LONG_MODE_ENABLE) {
            if old_cr0_pg.is_empty() && cr0.contains(Cr0::CR0_ENABLE_PAGING) {
                todo!("enter lmode todo");
            } else if !old_cr0_pg.is_empty() && !cr0.contains(Cr0::CR0_ENABLE_PAGING) {
                todo!("exit lmode todo");
            }
        }

        if vmx_info().enable_ept && !vmx_info().enable_unrestricted_guest {
            todo!()
        }

        vcpu.vmx_mut().emulation_required = vmx_info().emulation_required(vcpu);
    }

    fn set_cr4(&self, vcpu: &mut VirtCpu, cr4_flags: x86::controlregs::Cr4) {
        let old_cr4 = vcpu.arch.read_cr4_bits(Cr4::all());

        let mut hw_cr4 = (unsafe { cr4() } & Cr4::CR4_ENABLE_MACHINE_CHECK)
            | (cr4_flags & (!Cr4::CR4_ENABLE_MACHINE_CHECK));

        if vmx_info().enable_unrestricted_guest {
            hw_cr4.insert(Cr4::CR4_ENABLE_VMX);
        } else if vcpu.vmx().rmode.vm86_active {
            hw_cr4.insert(Cr4::CR4_ENABLE_PAE | Cr4::CR4_ENABLE_VMX | Cr4::CR4_ENABLE_VME);
        } else {
            hw_cr4.insert(Cr4::CR4_ENABLE_PAE | Cr4::CR4_ENABLE_VMX);
        }

        if vmx_info().vmx_umip_emulated() {
            if cr4_flags.contains(Cr4::CR4_ENABLE_UMIP) {
                vcpu.vmx().loaded_vmcs().controls_set(
                    ControlsType::SecondaryExec,
                    SecondaryControls::DTABLE_EXITING.bits() as u64,
                );
                hw_cr4.remove(Cr4::CR4_ENABLE_UMIP);
            } else if !vcpu.arch.is_guest_mode() {
                vcpu.vmx().loaded_vmcs().controls_clearbit(
                    ControlsType::SecondaryExec,
                    SecondaryControls::DTABLE_EXITING.bits() as u64,
                );
            }
        }

        vcpu.arch.cr4 = cr4_flags;
        vcpu.arch.mark_register_available(KvmReg::VcpuExregCr4);

        if !vmx_info().enable_unrestricted_guest {
            if vmx_info().enable_ept {
                if vcpu.arch.read_cr0_bits(Cr0::CR0_ENABLE_PAGING).is_empty() {
                    hw_cr4.remove(Cr4::CR4_ENABLE_PAE);
                    hw_cr4.insert(Cr4::CR4_ENABLE_PSE);
                } else if !cr4_flags.contains(Cr4::CR4_ENABLE_PAE) {
                    hw_cr4.remove(Cr4::CR4_ENABLE_PAE);
                }
            }

            if vcpu.arch.read_cr0_bits(Cr0::CR0_ENABLE_PAGING).is_empty() {
                hw_cr4.remove(
                    Cr4::CR4_ENABLE_SMEP | Cr4::CR4_ENABLE_SMAP | Cr4::CR4_ENABLE_PROTECTION_KEY,
                );
            }
        }

        VmxAsm::vmx_vmwrite(control::CR4_READ_SHADOW, cr4_flags.bits() as u64);
        VmxAsm::vmx_vmwrite(guest::CR4, hw_cr4.bits() as u64);

        if (cr4_flags ^ old_cr4).contains(Cr4::CR4_ENABLE_OS_XSAVE | Cr4::CR4_ENABLE_PROTECTION_KEY)
        {
            // TODO: update_cpuid_runtime
        }
    }

    fn set_efer(&self, vcpu: &mut VirtCpu, efer: x86_64::registers::control::EferFlags) {
        if vcpu.vmx().find_uret_msr(msr::IA32_EFER).is_none() {
            return;
        }

        vcpu.arch.efer = efer;
        if efer.contains(EferFlags::LONG_MODE_ACTIVE) {
            vcpu.vmx().loaded_vmcs().controls_setbit(
                ControlsType::VmEntry,
                EntryControls::IA32E_MODE_GUEST.bits().into(),
            );
        } else {
            vcpu.vmx().loaded_vmcs().controls_clearbit(
                ControlsType::VmEntry,
                EntryControls::IA32E_MODE_GUEST.bits().into(),
            );
        }

        vmx_info().setup_uret_msrs(vcpu);
    }

    fn update_exception_bitmap(&self, vcpu: &mut VirtCpu) {
        let mut eb = (1u32 << PF_VECTOR)
            | (1 << UD_VECTOR)
            | (1 << MC_VECTOR)
            | (1 << DB_VECTOR)
            | (1 << AC_VECTOR);

        if vmx_info().enable_vmware_backdoor {
            eb |= 1 << GP_VECTOR;
        }

        if vcpu.guest_debug & (GuestDebug::ENABLE | GuestDebug::USE_SW_BP)
            == (GuestDebug::ENABLE | GuestDebug::USE_SW_BP)
        {
            eb |= 1 << BP_VECTOR;
        }

        if vcpu.vmx().rmode.vm86_active {
            eb = !0;
        }

        if !vmx_info().vmx_need_pf_intercept(vcpu) {
            eb &= !(1 << PF_VECTOR);
        }

        if vcpu.arch.is_guest_mode() {
            todo!()
        } else {
            let mut mask = PageFaultErr::empty();
            let mut match_code = PageFaultErr::empty();
            if vmx_info().enable_ept && (eb & (1 << PF_VECTOR) != 0) {
                mask = PageFaultErr::PFERR_PRESENT | PageFaultErr::PFERR_RSVD;
                match_code = PageFaultErr::PFERR_PRESENT;
            }

            VmxAsm::vmx_vmwrite(control::PAGE_FAULT_ERR_CODE_MASK, mask.bits);
            VmxAsm::vmx_vmwrite(control::PAGE_FAULT_ERR_CODE_MATCH, match_code.bits);
        }

        if vcpu.arch.xfd_no_write_intercept {
            eb |= 1 << NM_VECTOR;
        }

        VmxAsm::vmx_vmwrite(control::EXCEPTION_BITMAP, eb as u64);
    }

    fn has_emulated_msr(&self, msr: u32) -> bool {
        match msr {
            msr::IA32_SMBASE => {
                return vmx_info().enable_unrestricted_guest
                    || vmx_info().emulate_invalid_guest_state;
            }

            msr::IA32_VMX_BASIC..=msr::IA32_VMX_VMFUNC => {
                return vmx_info().nested;
            }

            0xc001011f | 0xc0000104 => {
                // MSR_AMD64_VIRT_SPEC_CTRL | MSR_AMD64_TSC_RATIO
                return false;
            }

            _ => {
                return true;
            }
        }
    }

    fn get_msr_feature(&self, msr: &mut super::asm::VmxMsrEntry) -> bool {
        match msr.index {
            msr::IA32_VMX_BASIC..=msr::IA32_VMX_VMFUNC => {
                if !vmx_info().nested {
                    return false;
                }

                match vmx_info().vmcs_config.nested.get_vmx_msr(msr.index) {
                    Some(data) => {
                        msr.data = data;
                        return true;
                    }
                    None => {
                        return false;
                    }
                }
            }
            _ => {
                return false;
            }
        }
    }

    fn get_rflags(&self, vcpu: &mut VirtCpu) -> x86::bits64::rflags::RFlags {
        if !vcpu.arch.is_register_available(KvmReg::VcpuExregRflags) {
            vcpu.arch.mark_register_available(KvmReg::VcpuExregRflags);
            let mut rflags = RFlags::from_bits_truncate(VmxAsm::vmx_vmread(guest::RFLAGS));
            if vcpu.vmx_mut().rmode.vm86_active {
                rflags.remove(RFlags::FLAGS_IOPL3 | RFlags::FLAGS_VM);
                let save_rflags = vcpu.vmx_mut().rmode.save_rflags;
                rflags.insert(save_rflags & !(RFlags::FLAGS_IOPL3 | RFlags::FLAGS_VM));
            }

            vcpu.vmx_mut().rflags = rflags;
        }

        return vcpu.vmx_mut().rflags;
    }

    fn vcpu_precreate(&self, vm: &mut Vm) -> Result<(), SystemError> {
        if vm.arch.irqchip_mode != KvmIrqChipMode::None || !vmx_info().enable_ipiv {
            return Ok(());
        }

        let kvm_vmx = vm.kvm_vmx_mut();

        if kvm_vmx.pid_table.is_some() {
            return Ok(());
        }

        kvm_vmx.pid_table = Some(unsafe { Box::new_zeroed().assume_init() });
        Ok(())
    }

    fn set_segment(&self, vcpu: &mut VirtCpu, var: &mut UapiKvmSegment, seg: VcpuSegment) {
        vcpu.vmx_mut().emulation_required = vmx_info().emulation_required(vcpu);
        *var = vmx_info()._vmx_set_segment(vcpu, *var, seg);
    }

    fn get_segment(
        &self,
        vcpu: &mut VirtCpu,
        var: UapiKvmSegment,
        seg: VcpuSegment,
    ) -> UapiKvmSegment {
        return vmx_info().vmx_get_segment(vcpu, var, seg);
    }

    fn get_idt(&self, _vcpu: &mut VirtCpu, dt: &mut x86::dtables::DescriptorTablePointer<u8>) {
        dt.limit = VmxAsm::vmx_vmread(guest::IDTR_LIMIT) as u16;
        dt.base = VmxAsm::vmx_vmread(guest::IDTR_BASE) as usize as *const _;
    }

    fn set_idt(&self, _vcpu: &mut VirtCpu, dt: &x86::dtables::DescriptorTablePointer<u8>) {
        VmxAsm::vmx_vmwrite(guest::IDTR_LIMIT, dt.limit as u64);
        VmxAsm::vmx_vmwrite(guest::IDTR_BASE, dt.base as usize as u64);
    }

    fn get_gdt(&self, _vcpu: &mut VirtCpu, dt: &mut x86::dtables::DescriptorTablePointer<u8>) {
        dt.limit = VmxAsm::vmx_vmread(guest::GDTR_LIMIT) as u16;
        dt.base = VmxAsm::vmx_vmread(guest::GDTR_BASE) as usize as *const _;
    }

    fn set_gdt(&self, _vcpu: &mut VirtCpu, dt: &x86::dtables::DescriptorTablePointer<u8>) {
        VmxAsm::vmx_vmwrite(guest::GDTR_LIMIT, dt.limit as u64);
        VmxAsm::vmx_vmwrite(guest::GDTR_BASE, dt.base as usize as u64);
    }

    fn is_vaild_cr0(&self, vcpu: &VirtCpu, _cr0: Cr0) -> bool {
        if vcpu.arch.is_guest_mode() {
            todo!()
        }

        // TODO: 判断vmx->nested->vmxon

        true
    }

    fn is_vaild_cr4(&self, vcpu: &VirtCpu, cr4: Cr4) -> bool {
        if cr4.contains(Cr4::CR4_ENABLE_VMX) && vcpu.arch.is_smm() {
            return false;
        }

        // TODO: 判断vmx->nested->vmxon

        return true;
    }

    fn post_set_cr3(&self, _vcpu: &VirtCpu, _cr3: u64) {
        // Do Nothing
    }

    fn vcpu_run(&self, vcpu: &mut VirtCpu) -> ExitFastpathCompletion {
        if unlikely(vmx_info().enable_vnmi && vcpu.vmx().loaded_vmcs().soft_vnmi_blocked) {
            todo!()
        }

        if unlikely(vcpu.vmx().emulation_required) {
            todo!()
        }

        if vcpu.vmx().ple_window_dirty {
            vcpu.vmx_mut().ple_window_dirty = false;
            VmxAsm::vmx_vmwrite(control::PLE_WINDOW, vcpu.vmx().ple_window as u64);
        }

        if vcpu.arch.is_register_dirty(KvmReg::VcpuRegsRsp) {
            VmxAsm::vmx_vmwrite(guest::RSP, vcpu.arch.regs[KvmReg::VcpuRegsRsp as usize]);
        }
        if vcpu.arch.is_register_dirty(KvmReg::VcpuRegsRip) {
            VmxAsm::vmx_vmwrite(guest::RIP, vcpu.arch.regs[KvmReg::VcpuRegsRip as usize]);
        }

        vcpu.arch.clear_dirty();

        let cr3: (PhysFrame, Cr3Flags) = Cr3::read();
        if unlikely(cr3 != vcpu.vmx().loaded_vmcs().host_state.cr3) {
            let cr3_combined: u64 =
                (cr3.0.start_address().as_u64() & 0xFFFF_FFFF_FFFF_F000) | (cr3.1.bits() & 0xFFF);
            VmxAsm::vmx_vmwrite(host::CR3, cr3_combined);
            vcpu.vmx().loaded_vmcs().host_state.cr3 = cr3;
        }

        let cr4 = unsafe { cr4() };
        if unlikely(cr4 != vcpu.vmx().loaded_vmcs().host_state.cr4) {
            VmxAsm::vmx_vmwrite(host::CR4, cr4.bits() as u64);
            vcpu.vmx().loaded_vmcs().host_state.cr4 = cr4;
        }

        // TODO: set_debugreg

        if vcpu.guest_debug.contains(GuestDebug::SINGLESTEP) {
            todo!()
        }

        vcpu.load_guest_xsave_state();

        // TODO: pt_guest_enter

        // TODO: atomic_switch_perf_msrs

        if vmx_info().enable_preemption_timer {
            // todo!()
            warn!("vmx_update_hv_timer TODO");
        }

        Vmx::vmx_vcpu_enter_exit(vcpu, vcpu.vmx().vmx_vcpu_run_flags());

        unsafe {
            load_ds(USER_DS);
            load_es(USER_DS);
        };

        // TODO: pt_guest_exit

        // TODO: kvm_load_host_xsave_state

        if vcpu.arch.is_guest_mode() {
            todo!()
        }

        if unlikely(vcpu.vmx().fail != 0) {
            return ExitFastpathCompletion::None;
        }

        if unlikely(
            vcpu.vmx().exit_reason.basic()
                == VmxExitReasonBasic::VM_ENTRY_FAILURE_MACHINE_CHECK_EVENT as u16,
        ) {
            todo!()
        }

        if unlikely(vcpu.vmx().exit_reason.failed_vmentry()) {
            return ExitFastpathCompletion::None;
        }

        vcpu.vmx().loaded_vmcs().launched = true;

        // TODO: 处理中断

        if vcpu.arch.is_guest_mode() {
            return ExitFastpathCompletion::None;
        }

        return Vmx::vmx_exit_handlers_fastpath(vcpu);
    }

    fn prepare_switch_to_guest(&self, vcpu: &mut VirtCpu) {
        // let cpu = smp_get_processor_id();
        let vmx = vcpu.vmx_mut();
        vmx.req_immediate_exit = false;

        if !vmx.guest_uret_msrs_loaded {
            vmx.guest_uret_msrs_loaded = true;

            for (idx, msr) in vmx.guest_uret_msrs.iter().enumerate() {
                if msr.load_into_hardware {
                    x86_kvm_manager().kvm_set_user_return_msr(idx, msr.data, msr.mask);
                }
            }
        }

        // TODO: nested

        if vmx.guest_state_loaded {
            return;
        }

        // fixme: 这里读的是当前cpu的gsbase，正确安全做法应该为将gsbase设置为percpu变量
        let gs_base = unsafe { rdmsr(msr::IA32_KERNEL_GSBASE) };

        let current = ProcessManager::current_pcb();
        let mut pcb_arch = current.arch_info_irqsave();

        let fs_sel = fs().bits();
        let gs_sel = gs().bits();

        unsafe {
            pcb_arch.save_fsbase();
            pcb_arch.save_gsbase();
        }

        let fs_base = pcb_arch.fsbase();
        vmx.msr_host_kernel_gs_base = pcb_arch.gsbase() as u64;

        unsafe { wrmsr(msr::IA32_KERNEL_GSBASE, vmx.msr_guest_kernel_gs_base) };

        let mut loaded_vmcs = vmx.loaded_vmcs();
        let host_state = &mut loaded_vmcs.host_state;
        host_state.ldt_sel = unsafe { ldtr() }.bits();

        host_state.ds_sel = ds().bits();
        host_state.es_sel = es().bits();

        host_state.set_host_fsgs(fs_sel, gs_sel, fs_base, gs_base as usize);
        drop(loaded_vmcs);

        vmx.guest_state_loaded = true;
    }

    fn flush_tlb_all(&self, vcpu: &mut VirtCpu) {
        if vmx_info().enable_ept {
            VmxAsm::ept_sync_global();
        } else if vmx_info().has_invvpid_global() {
            VmxAsm::sync_vcpu_global();
        } else {
            VmxAsm::sync_vcpu_single(vcpu.vmx().vpid);
            // TODO: 嵌套：VmxAsm::sync_vcpu_single(vcpu.vmx().nested.vpid02);
        }
    }

    fn handle_exit_irqoff(&self, vcpu: &mut VirtCpu) {
        if vcpu.vmx().emulation_required {
            return;
        }

        let basic = VmxExitReasonBasic::from(vcpu.vmx().exit_reason.basic());

        if basic == VmxExitReasonBasic::EXTERNAL_INTERRUPT {
            Vmx::handle_external_interrupt_irqoff(vcpu);
        } else if basic == VmxExitReasonBasic::EXCEPTION_OR_NMI {
            //todo!()
        }
    }

    fn handle_exit(
        //vmx_handle_exit
        &self,
        vcpu: &mut VirtCpu,
        vm: &Vm,
        fastpath: ExitFastpathCompletion,
    ) -> Result<i32, SystemError> {
        let r = vmx_info().vmx_handle_exit(vcpu, vm, fastpath);

        if vcpu.vmx().exit_reason.bus_lock_detected() {
            todo!()
        }

        r
    }

    fn load_mmu_pgd(&self, vcpu: &mut VirtCpu, _vm: &Vm, root_hpa: u64, root_level: u32) {
        let guest_cr3;
        let eptp;

        if vmx_info().enable_ept {
            eptp = vmx_info().construct_eptp(vcpu, root_hpa, root_level);

            VmxAsm::vmx_vmwrite(control::EPTP_FULL, eptp);

            if !vmx_info().enable_unrestricted_guest
                && !vcpu.arch.cr0.contains(Cr0::CR0_ENABLE_PAGING)
            {
                todo!()
            } else if vcpu.arch.is_register_dirty(KvmReg::VcpuExregCr3) {
                guest_cr3 = vcpu.arch.cr3;
                debug!("load_mmu_pgd: guest_cr3 = {:#x}", guest_cr3);
            } else {
                return;
            }
        } else {
            todo!();
        }
        vcpu.load_pdptrs();
        VmxAsm::vmx_vmwrite(guest::CR3, guest_cr3);
    }
}

static mut VMX: Option<Vmx> = None;

#[inline]
pub fn vmx_info() -> &'static Vmx {
    unsafe { VMX.as_ref().unwrap() }
}

#[inline]
pub fn vmx_info_mut() -> &'static mut Vmx {
    unsafe { VMX.as_mut().unwrap() }
}

#[inline(never)]
pub fn init_vmx(vmx: Box<Vmx>) {
    static INIT_ONCE: AtomicBool = AtomicBool::new(false);
    if INIT_ONCE
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        unsafe { VMX = Some(*vmx) };
    } else {
        panic!("init_vmx can only be called once");
    }
}

#[derive(Debug)]
pub struct Vmx {
    pub host_idt_base: u64,
    pub vmcs_config: VmcsConfig,
    pub vmx_cap: VmxCapability,
    pub vpid_bitmap: SpinLock<AllocBitmap>,
    pub enable_vpid: bool,
    pub enable_ept: bool,
    pub enable_ept_ad: bool,
    pub enable_unrestricted_guest: bool,
    pub emulate_invalid_guest_state: bool,
    pub enable_flexpriority: bool,
    pub enable_vnmi: bool,
    pub enable_sgx: bool,
    pub enable_apicv: bool,
    pub enable_ipiv: bool,
    pub enable_pml: bool,
    pub enable_preemption_timer: bool,

    pub enable_vmware_backdoor: bool,

    pub nested: bool,

    pub ple_gap: u32,
    pub ple_window: u32,
    pub ple_window_grow: u32,
    pub ple_window_max: u32,
    pub ple_window_shrink: u32,

    pub pt_mode: ProcessorTraceMode,
}

impl Vmx {
    fn init(&mut self) {
        let mut bitmap = AllocBitmap::new(1 << 16);

        // 0为vpid的非法值
        bitmap.set(0, true);

        self.host_idt_base = Default::default();
        self.vmcs_config = Default::default();
        self.vmx_cap = Default::default();
        self.vpid_bitmap = SpinLock::new(bitmap);
        self.enable_vpid = true;
        self.enable_ept = true;
        self.enable_ept_ad = true;
        self.enable_unrestricted_guest = true;
        self.enable_flexpriority = true;
        self.enable_vnmi = true;
        self.enable_sgx = true;
        self.ple_gap = 128;
        self.ple_window = 4096;
        self.ple_window_grow = 2;
        self.ple_window_max = u32::MAX;
        self.ple_window_shrink = 0;
        self.enable_apicv = true;
        self.enable_ipiv = true;
        self.enable_pml = true;
        self.enable_preemption_timer = true;
        self.pt_mode = ProcessorTraceMode::System;
        self.emulate_invalid_guest_state = true;

        // 目前先不管嵌套虚拟化，后续再实现
        self.nested = false;
        self.enable_vmware_backdoor = false;
    }

    /*
     * Internal error codes that are used to indicate that MSR emulation encountered
     * an error that should result in #GP in the guest, unless userspace
     * handles it.
     */
    #[allow(dead_code)]
    pub const KVM_MSR_RET_INVALID: u32 = 2; /* in-kernel MSR emulation #GP condition */
    #[allow(dead_code)]
    pub const KVM_MSR_RET_FILTERED: u32 = 3; /* #GP due to userspace MSR filter */

    pub const MAX_POSSIBLE_PASSTHROUGH_MSRS: usize = 16;

    pub const VMX_POSSIBLE_PASSTHROUGH_MSRS: [u32; Self::MAX_POSSIBLE_PASSTHROUGH_MSRS] = [
        0x48,  // MSR_IA32_SPEC_CTRL
        0x49,  // MSR_IA32_PRED_CMD
        0x10b, // MSR_IA32_FLUSH_CMD
        msr::IA32_TIME_STAMP_COUNTER,
        msr::IA32_FS_BASE,
        msr::IA32_GS_BASE,
        msr::IA32_KERNEL_GSBASE,
        0x1c4, // MSR_IA32_XFD
        0x1c5, // MSR_IA32_XFD_ERR
        msr::IA32_SYSENTER_CS,
        msr::IA32_SYSENTER_ESP,
        msr::IA32_SYSENTER_EIP,
        msr::MSR_CORE_C1_RESIDENCY,
        msr::MSR_CORE_C3_RESIDENCY,
        msr::MSR_CORE_C6_RESIDENCY,
        msr::MSR_CORE_C7_RESIDENCY,
    ];

    /// ### 查看CPU是否支持虚拟化
    #[allow(dead_code)]
    pub fn check_vmx_support() -> bool {
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

    #[inline(never)]
    pub fn set_up_user_return_msrs() {
        const VMX_URET_MSRS_LIST: &[u32] = &[
            msr::IA32_FMASK,
            msr::IA32_LSTAR,
            msr::IA32_CSTAR,
            msr::IA32_EFER,
            msr::IA32_TSC_AUX,
            msr::IA32_STAR,
            // 这个寄存器会出错<,先注释掉
            // MSR_IA32_TSX_CTRL,
        ];

        let manager = x86_kvm_manager_mut();
        for msr in VMX_URET_MSRS_LIST {
            manager.add_user_return_msr(*msr);
        }
    }

    /// 初始化设置vmcs的config
    #[inline(never)]
    pub fn setup_vmcs_config(
        vmcs_config: &mut VmcsConfig,
        vmx_cap: &mut VmxCapability,
    ) -> Result<(), SystemError> {
        const VMCS_ENTRY_EXIT_PAIRS: &[VmcsEntryExitPair] = &[
            VmcsEntryExitPair::new(
                EntryControls::LOAD_IA32_PERF_GLOBAL_CTRL,
                ExitControls::LOAD_IA32_PERF_GLOBAL_CTRL,
            ),
            VmcsEntryExitPair::new(EntryControls::LOAD_IA32_PAT, ExitControls::LOAD_IA32_PAT),
            VmcsEntryExitPair::new(EntryControls::LOAD_IA32_EFER, ExitControls::LOAD_IA32_EFER),
            VmcsEntryExitPair::new(
                EntryControls::LOAD_IA32_BNDCFGS,
                ExitControls::CLEAR_IA32_BNDCFGS,
            ),
            VmcsEntryExitPair::new(
                EntryControls::LOAD_IA32_RTIT_CTL,
                ExitControls::CLEAR_IA32_RTIT_CTL,
            ),
        ];

        let mut cpu_based_exec_control = VmxFeat::adjust_primary_controls()?;

        let mut cpu_based_2nd_exec_control =
            if cpu_based_exec_control.contains(PrimaryControls::SECONDARY_CONTROLS) {
                VmxFeat::adjust_secondary_controls()?
            } else {
                SecondaryControls::empty()
            };

        if cpu_based_2nd_exec_control.contains(SecondaryControls::VIRTUALIZE_APIC) {
            cpu_based_exec_control.remove(PrimaryControls::USE_TPR_SHADOW)
        }

        if !cpu_based_exec_control.contains(PrimaryControls::USE_TPR_SHADOW) {
            cpu_based_2nd_exec_control.remove(
                SecondaryControls::VIRTUALIZE_APIC_REGISTER
                    | SecondaryControls::VIRTUALIZE_X2APIC
                    | SecondaryControls::VIRTUAL_INTERRUPT_DELIVERY,
            )
        }

        let cap = unsafe { rdmsr(msr::IA32_VMX_EPT_VPID_CAP) };
        vmx_cap.set_val_from_msr_val(cap);

        // 不支持ept但是读取到了值
        if !cpu_based_2nd_exec_control.contains(SecondaryControls::ENABLE_EPT)
            && !vmx_cap.ept.is_empty()
        {
            warn!("EPT CAP should not exist if not support. 1-setting enable EPT VM-execution control");
            return Err(SystemError::EIO);
        }

        if !cpu_based_2nd_exec_control.contains(SecondaryControls::ENABLE_VPID)
            && !vmx_cap.vpid.is_empty()
        {
            warn!("VPID CAP should not exist if not support. 1-setting enable VPID VM-execution control");
            return Err(SystemError::EIO);
        }

        let cpuid = CpuId::new();
        let cpu_extend_feat = cpuid
            .get_extended_feature_info()
            .ok_or(SystemError::ENOSYS)?;
        if !cpu_extend_feat.has_sgx() {
            cpu_based_2nd_exec_control.remove(SecondaryControls::ENCLS_EXITING);
        }

        let cpu_based_3rd_exec_control = 0;
        // if cpu_based_exec_control.contains(SecondaryControls::TERTIARY_CONTROLS) {
        //     // Self::adjust_vmx_controls64(VmxFeature::IPI_VIRT, IA32_CTLS3)
        //     todo!()
        // } else {
        //     0
        // };

        let vmxexit_control = VmxFeat::adjust_exit_controls()?;

        let pin_based_exec_control = VmxFeat::adjust_pin_based_controls()?;

        // TODO: broken timer?
        // https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/vmx/vmx.c#2676

        let vmentry_control = VmxFeat::adjust_entry_controls()?;

        for pair in VMCS_ENTRY_EXIT_PAIRS {
            let n_ctrl = pair.entry;
            let x_ctrl = pair.exit;

            // if !(vmentry_control.bits() & n_ctrl.bits) == !(vmxexit_control.bits() & x_ctrl.bits) {
            //     continue;
            // }
            if (vmentry_control.contains(n_ctrl)) == (vmxexit_control.contains(x_ctrl)) {
                continue;
            }

            warn!(
                "Inconsistent VM-Entry/VM-Exit pair, entry = {:?}, exit = {:?}",
                vmentry_control & n_ctrl,
                vmxexit_control & x_ctrl,
            );

            return Err(SystemError::EIO);
        }

        let basic = unsafe { rdmsr(msr::IA32_VMX_BASIC) };
        let vmx_msr_high = (basic >> 32) as u32;
        let vmx_msr_low = basic as u32;

        // 64位cpu，VMX_BASIC[48] == 0
        if vmx_msr_high & (1 << 16) != 0 {
            return Err(SystemError::EIO);
        }

        // 判断是否为写回(WB)
        if (vmx_msr_high >> 18) & 15 != 6 {
            return Err(SystemError::EIO);
        }

        let misc_msr = unsafe { rdmsr(msr::IA32_VMX_MISC) };

        vmcs_config.size = vmx_msr_high & 0x1fff;
        vmcs_config.basic_cap = vmx_msr_high & !0x1fff;
        vmcs_config.revision_id = vmx_msr_low;
        vmcs_config.pin_based_exec_ctrl = pin_based_exec_control;
        vmcs_config.cpu_based_exec_ctrl = cpu_based_exec_control;
        vmcs_config.cpu_based_2nd_exec_ctrl = cpu_based_2nd_exec_control;
        vmcs_config.cpu_based_3rd_exec_ctrl = cpu_based_3rd_exec_control;
        vmcs_config.vmentry_ctrl = vmentry_control;
        vmcs_config.vmexit_ctrl = vmxexit_control;
        vmcs_config.misc = misc_msr;

        Ok(())
    }

    fn adjust_vmx_controls(ctl_min: u32, ctl_opt: u32, msr: u32) -> Result<u32, SystemError> {
        let mut ctl = ctl_min | ctl_opt;
        let val = unsafe { rdmsr(msr) };
        let low = val as u32;
        let high = (val >> 32) as u32;

        ctl &= high;
        ctl |= low;

        if ctl_min & !ctl != 0 {
            return Err(SystemError::EIO);
        }

        return Ok(ctl);
    }
    #[allow(dead_code)]
    fn adjust_vmx_controls64(ctl_opt: u32, msr: u32) -> u32 {
        let allow = unsafe { rdmsr(msr) } as u32;
        ctl_opt & allow
    }

    pub fn alloc_vpid(&self) -> Option<usize> {
        if !self.enable_vpid {
            return None;
        }

        let mut bitmap_guard = self.vpid_bitmap.lock();

        let idx = bitmap_guard.first_false_index();
        if let Some(idx) = idx {
            bitmap_guard.set(idx, true);
        }

        return idx;
    }
    #[allow(dead_code)]
    pub fn free_vpid(&self, vpid: Option<usize>) {
        if !self.enable_vpid || vpid.is_none() {
            return;
        }

        self.vpid_bitmap.lock().set(vpid.unwrap(), false);
    }

    pub fn is_valid_passthrough_msr(msr: u32) -> bool {
        match msr {
            0x800..0x8ff => {
                // x2Apic msr寄存器
                return true;
            }
            msr::MSR_IA32_RTIT_STATUS
            | msr::MSR_IA32_RTIT_OUTPUT_BASE
            | msr::MSR_IA32_RTIT_OUTPUT_MASK_PTRS
            | msr::MSR_IA32_CR3_MATCH
            | msr::MSR_LBR_SELECT
            | msr::MSR_LASTBRANCH_TOS => {
                return true;
            }
            msr::MSR_IA32_ADDR0_START..msr::MSR_IA32_ADDR3_END => {
                return true;
            }
            0xdc0..0xddf => {
                // MSR_LBR_INFO_0 ... MSR_LBR_INFO_0 + 31
                return true;
            }
            0x680..0x69f => {
                // MSR_LBR_NHM_FROM ... MSR_LBR_NHM_FROM + 31
                return true;
            }
            0x6c0..0x6df => {
                // MSR_LBR_NHM_TO ... MSR_LBR_NHM_TO + 31
                return true;
            }
            0x40..0x48 => {
                // MSR_LBR_CORE_FROM ... MSR_LBR_CORE_FROM + 8
                return true;
            }
            0x60..0x68 => {
                // MSR_LBR_CORE_TO ... MSR_LBR_CORE_TO + 8
                return true;
            }
            _ => {
                return Self::possible_passthrough_msr_slot(msr).is_some();
            }
        }
    }

    pub fn vpid_sync_context(&self, vpid: u16) {
        if self.has_invvpid_single() {
            VmxAsm::sync_vcpu_single(vpid);
        } else if vpid != 0 {
            VmxAsm::sync_vcpu_global();
        }
    }

    pub fn possible_passthrough_msr_slot(msr: u32) -> Option<usize> {
        for (idx, val) in Self::VMX_POSSIBLE_PASSTHROUGH_MSRS.iter().enumerate() {
            if *val == msr {
                return Some(idx);
            }
        }

        return None;
    }

    pub fn tdp_enabled(&self) -> bool {
        self.enable_ept
    }

    fn setup_l1d_flush(&self) {
        // TODO:先这样写
        *L1TF_VMX_MITIGATION.write() = VmxL1dFlushState::NotRequired;
    }

    pub fn construct_eptp(&self, vcpu: &mut VirtCpu, root_hpa: u64, root_level: u32) -> u64 {
        let mut eptp = VMX_EPTP_MT_WB;

        eptp |= if root_level == 5 {
            VMX_EPTP_PWL_5
        } else {
            VMX_EPTP_PWL_4
        };

        if self.enable_ept_ad && !vcpu.arch.is_guest_mode() {
            eptp |= VMX_EPTP_AD_ENABLE_BIT;
        }

        eptp |= root_hpa;

        return eptp;
    }

    fn vmx_reset_vcpu(&mut self, vcpu: &mut VirtCpu, vm: &Vm) {
        self.init_vmcs(vcpu, vm);

        if self.nested {
            todo!()
        }

        // TODO: vcpu_setup_sgx_lepubkeyhash

        // TODO: nested

        vcpu.arch.microcode_version = 0x100000000;

        let vmx = vcpu.vmx_mut();
        vmx.msr_ia32_feature_control_valid_bits = 1 << 0;

        vmx.post_intr_desc.control.set_nv(0xf2);
        vmx.post_intr_desc.control.set_sn(true);
    }

    fn init_vmcs(&mut self, vcpu: &mut VirtCpu, vm: &Vm) {
        let kvm_vmx = vm.kvm_vmx();
        if vmx_info().nested {
            todo!()
        }

        if vmx_info().has_msr_bitmap() {
            debug!(
                "msr_bitmap addr 0x{:x}",
                vcpu.vmx().vmcs01.lock().msr_bitmap.phys_addr() as u64
            );
            VmxAsm::vmx_vmwrite(
                control::MSR_BITMAPS_ADDR_FULL,
                vcpu.vmx().vmcs01.lock().msr_bitmap.phys_addr() as u64,
            )
        }

        VmxAsm::vmx_vmwrite(guest::LINK_PTR_FULL, u64::MAX);

        let mut loaded_vmcs = vcpu.vmx().loaded_vmcs.lock();

        loaded_vmcs.controls_set(
            ControlsType::Pin,
            self.get_pin_based_exec_controls(vcpu).bits() as u64,
        );

        loaded_vmcs.controls_set(
            ControlsType::Exec,
            self.get_exec_controls(vcpu, &vm.arch).bits() as u64,
        );

        if self.has_sceondary_exec_ctrls() {
            loaded_vmcs.controls_set(
                ControlsType::SecondaryExec,
                self.get_secondary_exec_controls(vcpu, vm).bits() as u64,
            )
        }

        if self.has_tertiary_exec_ctrls() {
            todo!()
        }

        drop(loaded_vmcs);

        if self.enable_apicv && vcpu.arch.lapic_in_kernel() {
            VmxAsm::vmx_vmwrite(control::EOI_EXIT0_FULL, 0);
            VmxAsm::vmx_vmwrite(control::EOI_EXIT1_FULL, 0);
            VmxAsm::vmx_vmwrite(control::EOI_EXIT2_FULL, 0);
            VmxAsm::vmx_vmwrite(control::EOI_EXIT3_FULL, 0);

            VmxAsm::vmx_vmwrite(guest::INTERRUPT_STATUS, 0);

            VmxAsm::vmx_vmwrite(control::POSTED_INTERRUPT_NOTIFICATION_VECTOR, 0xf2);
            VmxAsm::vmx_vmwrite(control::POSTED_INTERRUPT_DESC_ADDR_FULL, unsafe {
                MMArch::virt_2_phys(VirtAddr::new(
                    &vcpu.vmx().post_intr_desc as *const _ as usize,
                ))
                .unwrap()
                .data() as u64
            })
        }

        if self.enable_apicv && vcpu.arch.lapic_in_kernel() {
            // PID_POINTER_TABLE
            VmxAsm::vmx_vmwrite(0x2042, unsafe {
                MMArch::virt_2_phys(VirtAddr::new(kvm_vmx.pid_table().as_ptr() as usize))
                    .unwrap()
                    .data() as u64
            });
            // LAST_PID_POINTER_INDEX
            VmxAsm::vmx_vmwrite(0x08, vm.arch.max_vcpu_ids as u64 - 1);
        }

        if !vm.arch.pause_in_guest {
            VmxAsm::vmx_vmwrite(control::PLE_GAP, self.ple_gap as u64);
            vcpu.vmx_mut().ple_window = self.ple_window;
            vcpu.vmx_mut().ple_window_dirty = true;
        }

        if vm
            .arch
            .notify_vmexit_flags
            .contains(NotifyVmExitFlags::KVM_X86_NOTIFY_VMEXIT_ENABLED)
        {
            // NOTIFY_WINDOW
            VmxAsm::vmx_vmwrite(0x4024, vm.arch.notify_window as u64);
        }

        VmxAsm::vmx_vmwrite(control::PAGE_FAULT_ERR_CODE_MASK, 0);
        VmxAsm::vmx_vmwrite(control::PAGE_FAULT_ERR_CODE_MATCH, 0);
        VmxAsm::vmx_vmwrite(control::CR3_TARGET_COUNT, 0);

        VmxAsm::vmx_vmwrite(host::FS_SELECTOR, 0);
        VmxAsm::vmx_vmwrite(host::GS_SELECTOR, 0);
        self.set_constant_host_state(vcpu);

        VmxAsm::vmx_vmwrite(host::FS_BASE, 0);
        VmxAsm::vmx_vmwrite(host::GS_BASE, 0);

        if self.has_vmfunc() {
            VmxAsm::vmx_vmwrite(control::VM_FUNCTION_CONTROLS_FULL, 0);
        }

        VmxAsm::vmx_vmwrite(control::VMEXIT_MSR_STORE_COUNT, 0);
        VmxAsm::vmx_vmwrite(control::VMEXIT_MSR_LOAD_COUNT, 0);
        VmxAsm::vmx_vmwrite(control::VMEXIT_MSR_LOAD_ADDR_FULL, unsafe {
            MMArch::virt_2_phys(VirtAddr::new(
                vcpu.vmx().msr_autoload.host.val.as_ptr() as *const _ as usize,
            ))
            .unwrap()
            .data() as u64
        });
        VmxAsm::vmx_vmwrite(control::VMENTRY_MSR_LOAD_COUNT, 0);
        VmxAsm::vmx_vmwrite(control::VMENTRY_MSR_LOAD_ADDR_FULL, unsafe {
            MMArch::virt_2_phys(VirtAddr::new(
                vcpu.vmx().msr_autoload.guest.val.as_ptr() as usize
            ))
            .unwrap()
            .data() as u64
        });

        if self
            .vmcs_config
            .vmentry_ctrl
            .contains(EntryControls::LOAD_IA32_PAT)
        {
            VmxAsm::vmx_vmwrite(guest::IA32_PAT_FULL, vcpu.arch.pat) //todo
        }

        let mut loaded_vmcs = vcpu.vmx().loaded_vmcs.lock();
        loaded_vmcs.controls_set(
            ControlsType::VmExit,
            self.get_vmexit_controls().bits() as u64,
        );

        loaded_vmcs.controls_set(
            ControlsType::VmEntry,
            self.get_vmentry_controls().bits() as u64,
        );

        drop(loaded_vmcs);

        vcpu.arch.cr0_guest_owned_bits = self.l1_guest_owned_cr0_bits();
        VmxAsm::vmx_vmwrite(
            control::CR0_GUEST_HOST_MASK,
            (!vcpu.arch.cr0_guest_owned_bits).bits() as u64,
        );

        self.set_cr4_guest_host_mask(&mut vcpu.arch);

        if vcpu.vmx().vpid != 0 {
            VmxAsm::vmx_vmwrite(control::VPID, vcpu.vmx().vpid as u64);
        }

        if self.has_xsaves() {
            VmxAsm::vmx_vmwrite(control::XSS_EXITING_BITMAP_FULL, 0);
        }

        if self.enable_pml {
            VmxAsm::vmx_vmwrite(control::PML_ADDR_FULL, unsafe {
                MMArch::virt_2_phys(VirtAddr::new(vcpu.vmx().pml_pg.as_ref().as_ptr() as usize))
                    .unwrap()
                    .data() as u64
            });

            VmxAsm::vmx_vmwrite(guest::PML_INDEX, VmxVCpuPriv::PML_ENTITY_NUM as u64 - 1);
        }

        // TODO: vmx_write_encls_bitmap

        if self.pt_mode == ProcessorTraceMode::HostGuest {
            todo!()
        }

        VmxAsm::vmx_vmwrite(guest::IA32_SYSENTER_CS, 0);
        VmxAsm::vmx_vmwrite(guest::IA32_SYSENTER_ESP, 0);
        VmxAsm::vmx_vmwrite(guest::IA32_SYSENTER_EIP, 0);
        VmxAsm::vmx_vmwrite(guest::IA32_DEBUGCTL_FULL, 0);

        if self.has_tpr_shadow() {
            VmxAsm::vmx_vmwrite(control::VIRT_APIC_ADDR_FULL, 0);
            if vcpu.arch.lapic_in_kernel() {
                VmxAsm::vmx_vmwrite(control::VIRT_APIC_ADDR_FULL, unsafe {
                    MMArch::virt_2_phys(VirtAddr::new(vcpu.arch.lapic().regs.as_ptr() as usize))
                        .unwrap()
                        .data() as u64
                });
            }

            VmxAsm::vmx_vmwrite(control::TPR_THRESHOLD, 0);
        }

        self.setup_uret_msrs(vcpu);
    }

    /// 打印VMCS信息用于debug
    pub fn dump_vmcs(&self, vcpu: &VirtCpu) {
        let vmentry_ctl = unsafe {
            EntryControls::from_bits_unchecked(self.vmread(control::VMENTRY_CONTROLS) as u32)
        };

        let vmexit_ctl = unsafe {
            ExitControls::from_bits_unchecked(self.vmread(control::VMEXIT_CONTROLS) as u32)
        };

        let cpu_based_exec_ctl = PrimaryControls::from_bits_truncate(
            self.vmread(control::PRIMARY_PROCBASED_EXEC_CONTROLS) as u32,
        );

        let pin_based_exec_ctl = PinbasedControls::from_bits_truncate(
            self.vmread(control::PINBASED_EXEC_CONTROLS) as u32,
        );

        // let cr4 = Cr4::from_bits_truncate(self.vmread(guest::CR4) as usize);

        let secondary_exec_control = if self.has_sceondary_exec_ctrls() {
            unsafe {
                SecondaryControls::from_bits_unchecked(
                    self.vmread(control::SECONDARY_PROCBASED_EXEC_CONTROLS) as u32,
                )
            }
        } else {
            SecondaryControls::empty()
        };

        if self.has_tertiary_exec_ctrls() {
            todo!()
        }

        error!(
            "VMCS addr: 0x{:x}, last attempted VM-entry on CPU {:?}",
            vcpu.vmx().loaded_vmcs().vmcs.lock().as_ref() as *const _ as usize,
            vcpu.arch.last_vmentry_cpu
        );

        error!("--- GUEST STATE ---");
        error!(
            "CR0: actual = 0x{:x}, shadow = 0x{:x}, gh_mask = 0x{:x}",
            self.vmread(guest::CR0),
            self.vmread(control::CR0_READ_SHADOW),
            self.vmread(control::CR0_GUEST_HOST_MASK)
        );
        error!(
            "CR4: actual = 0x{:x}, shadow = 0x{:x}, gh_mask = 0x{:x}",
            self.vmread(guest::CR4),
            self.vmread(control::CR4_READ_SHADOW),
            self.vmread(control::CR4_GUEST_HOST_MASK)
        );
        error!("CR3: actual = 0x{:x}", self.vmread(guest::CR3));

        if self.has_ept() {
            error!(
                "PDPTR0 = 0x{:x}, PDPTR1 = 0x{:x}",
                self.vmread(guest::PDPTE0_FULL),
                self.vmread(guest::PDPTE1_FULL)
            );
            error!(
                "PDPTR2 = 0x{:x}, PDPTR3 = 0x{:x}",
                self.vmread(guest::PDPTE2_FULL),
                self.vmread(guest::PDPTE3_FULL)
            );
        }
        error!(
            "RSP = 0x{:x}, RIP = 0x{:x}",
            self.vmread(guest::RSP),
            self.vmread(guest::RIP)
        );
        error!(
            "RFLAGS = 0x{:x}, DR7 = 0x{:x}",
            self.vmread(guest::RFLAGS),
            self.vmread(guest::DR7)
        );
        error!(
            "Sysenter RSP = 0x{:x}, CS:RIP = 0x{:x}:0x{:x}",
            self.vmread(guest::IA32_SYSENTER_ESP),
            self.vmread(guest::IA32_SYSENTER_CS),
            self.vmread(guest::IA32_SYSENTER_EIP),
        );

        self.dump_sel("CS: ", guest::CS_SELECTOR);
        self.dump_sel("DS: ", guest::DS_SELECTOR);
        self.dump_sel("SS: ", guest::SS_SELECTOR);
        self.dump_sel("ES: ", guest::ES_SELECTOR);
        self.dump_sel("FS: ", guest::FS_SELECTOR);
        self.dump_sel("GS: ", guest::GS_SELECTOR);

        self.dump_dtsel("GDTR: ", guest::GDTR_LIMIT);
        self.dump_sel("LDTR: ", guest::LDTR_SELECTOR);
        self.dump_dtsel("IDTR: ", guest::IDTR_LIMIT);
        self.dump_sel("TR: ", guest::TR_SELECTOR);

        let efer_slot = vcpu
            .vmx()
            .msr_autoload
            .guest
            .find_loadstore_msr_slot(msr::IA32_EFER);

        if vmentry_ctl.contains(EntryControls::LOAD_IA32_EFER) {
            error!("EFER = 0x{:x}", self.vmread(guest::IA32_EFER_FULL));
        } else if let Some(slot) = efer_slot {
            error!(
                "EFER = 0x{:x} (autoload)",
                vcpu.vmx().msr_autoload.guest.val[slot].data
            );
        } else if vmentry_ctl.contains(EntryControls::IA32E_MODE_GUEST) {
            error!(
                "EFER = 0x{:x} (effective)",
                vcpu.arch.efer | (EferFlags::LONG_MODE_ACTIVE | EferFlags::LONG_MODE_ENABLE)
            );
        } else {
            error!(
                "EFER = 0x{:x} (effective)",
                vcpu.arch.efer & !(EferFlags::LONG_MODE_ACTIVE | EferFlags::LONG_MODE_ENABLE)
            );
        }

        if vmentry_ctl.contains(EntryControls::LOAD_IA32_PAT) {
            error!("PAT = 0x{:x}", self.vmread(guest::IA32_PAT_FULL));
        }

        error!(
            "DebugCtl = 0x{:x}, DebugExceptions = 0x{:x}",
            self.vmread(guest::IA32_DEBUGCTL_FULL),
            self.vmread(guest::PENDING_DBG_EXCEPTIONS)
        );

        if self.has_load_perf_global_ctrl()
            && vmentry_ctl.contains(EntryControls::LOAD_IA32_PERF_GLOBAL_CTRL)
        {
            error!(
                "PerfGlobCtl = 0x{:x}",
                self.vmread(guest::IA32_PERF_GLOBAL_CTRL_FULL)
            );
        }

        if vmentry_ctl.contains(EntryControls::LOAD_IA32_BNDCFGS) {
            error!("BndCfgS = 0x{:x}", self.vmread(guest::IA32_BNDCFGS_FULL));
        }

        error!(
            "Interruptibility = 0x{:x}, ActivityState = 0x{:x}",
            self.vmread(guest::INTERRUPT_STATUS),
            self.vmread(guest::ACTIVITY_STATE)
        );

        if secondary_exec_control.contains(SecondaryControls::VIRTUAL_INTERRUPT_DELIVERY) {
            error!(
                "InterruptStatus = 0x{:x}",
                self.vmread(guest::INTERRUPT_STATUS)
            );
        }

        if self.vmread(control::VMENTRY_MSR_LOAD_COUNT) > 0 {
            self.dump_msrs("guest autoload", &vcpu.vmx().msr_autoload.guest);
        }
        if self.vmread(control::VMEXIT_MSR_LOAD_COUNT) > 0 {
            self.dump_msrs("guest autostore", &vcpu.vmx().msr_autostore);
        }

        error!("\n--- HOST STATE ---");
        error!(
            "RIP = 0x{:x}, RSP = 0x{:x}",
            self.vmread(host::RIP),
            self.vmread(host::RSP)
        );
        error!(
            "CS = 0x{:x}, SS = 0x{:x}, DS = 0x{:x}, ES = 0x{:x}, FS = 0x{:x}, GS = 0x{:x}, TR = 0x{:x}",
            self.vmread(host::CS_SELECTOR),
            self.vmread(host::SS_SELECTOR),
            self.vmread(host::DS_SELECTOR),
            self.vmread(host::ES_SELECTOR),
            self.vmread(host::FS_SELECTOR),
            self.vmread(host::GS_SELECTOR),
            self.vmread(host::TR_SELECTOR)
        );
        error!(
            "FSBase = 0x{:x}, GSBase = 0x{:x}, TRBase = 0x{:x}",
            self.vmread(host::FS_BASE),
            self.vmread(host::GS_BASE),
            self.vmread(host::TR_BASE),
        );
        error!(
            "GDTBase = 0x{:x}, IDTBase = 0x{:x}",
            self.vmread(host::GDTR_BASE),
            self.vmread(host::IDTR_BASE),
        );
        error!(
            "CR0 = 0x{:x}, CR3 = 0x{:x}, CR4 = 0x{:x}",
            self.vmread(host::CR0),
            self.vmread(host::CR3),
            self.vmread(host::CR4),
        );
        error!(
            "Sysenter RSP = 0x{:x}, CS:RIP=0x{:x}:0x{:x}",
            self.vmread(host::IA32_SYSENTER_ESP),
            self.vmread(host::IA32_SYSENTER_CS),
            self.vmread(host::IA32_SYSENTER_EIP),
        );

        if vmexit_ctl.contains(ExitControls::LOAD_IA32_EFER) {
            error!("EFER = 0x{:x}", self.vmread(host::IA32_EFER_FULL));
        }

        if vmexit_ctl.contains(ExitControls::LOAD_IA32_PAT) {
            error!("PAT = 0x{:x}", self.vmread(host::IA32_PAT_FULL));
        }

        if self.has_load_perf_global_ctrl()
            && vmexit_ctl.contains(ExitControls::LOAD_IA32_PERF_GLOBAL_CTRL)
        {
            error!(
                "PerfGlobCtl = 0x{:x}",
                self.vmread(host::IA32_PERF_GLOBAL_CTRL_FULL)
            );
        }

        if self.vmread(control::VMEXIT_MSR_LOAD_COUNT) > 0 {
            self.dump_msrs("host autoload", &vcpu.vmx().msr_autoload.host);
        }

        error!("\n--- CONTROL STATE ---");
        error!(
            "\nCPUBased = {:?},\nSecondaryExec = 0x{:x},\nTertiaryExec = 0(Unused)",
            cpu_based_exec_ctl, secondary_exec_control,
        );
        error!(
            "\nPinBased = {:?},\nEntryControls = {:?},\nExitControls = {:?}",
            pin_based_exec_ctl, vmentry_ctl, vmexit_ctl,
        );
        error!(
            "ExceptionBitmap = 0x{:x}, PFECmask = 0x{:x}, PFECmatch = 0x{:x}",
            self.vmread(control::EXCEPTION_BITMAP),
            self.vmread(control::PAGE_FAULT_ERR_CODE_MASK),
            self.vmread(control::PAGE_FAULT_ERR_CODE_MATCH),
        );
        error!(
            "VMEntry: intr_info = 0x{:x}, errcode = 0x{:x}, ilen = 0x{:x}",
            self.vmread(control::VMENTRY_INTERRUPTION_INFO_FIELD),
            self.vmread(control::VMENTRY_EXCEPTION_ERR_CODE),
            self.vmread(control::VMENTRY_INSTRUCTION_LEN),
        );
        error!(
            "VMExit: intr_info = 0x{:x}, errcode = 0x{:x}, ilen = 0x{:x}",
            self.vmread(ro::VMEXIT_INSTRUCTION_INFO),
            self.vmread(ro::VMEXIT_INTERRUPTION_ERR_CODE),
            self.vmread(ro::VMEXIT_INSTRUCTION_LEN),
        );
        error!(
            "        reason = 0x{:x}, qualification = 0x{:x}",
            self.vmread(ro::EXIT_REASON),
            self.vmread(ro::EXIT_QUALIFICATION),
        );
        error!(
            "IDTVectoring: info = 0x{:x}, errcode = 0x{:x}",
            self.vmread(ro::IDT_VECTORING_INFO),
            self.vmread(ro::IDT_VECTORING_ERR_CODE),
        );
        error!("TSC Offset = 0x{:x}", self.vmread(control::TSC_OFFSET_FULL));

        if secondary_exec_control.contains(SecondaryControls::USE_TSC_SCALING) {
            error!(
                "TSC Multiplier = 0x{:x}",
                self.vmread(control::TSC_MULTIPLIER_FULL)
            );
        }

        if cpu_based_exec_ctl.contains(PrimaryControls::USE_TPR_SHADOW) {
            if secondary_exec_control.contains(SecondaryControls::VIRTUAL_INTERRUPT_DELIVERY) {
                let status = self.vmread(guest::INTERRUPT_STATUS);
                error!("SVI|RVI = 0x{:x}|0x{:x}", status >> 8, status & 0xff);
            }

            error!(
                "TPR Threshold = 0x{:x}",
                self.vmread(control::TPR_THRESHOLD)
            );
            if secondary_exec_control.contains(SecondaryControls::VIRTUALIZE_APIC) {
                error!(
                    "APIC-access addr = 0x{:x}",
                    self.vmread(control::APIC_ACCESS_ADDR_FULL)
                );
            }
            error!(
                "virt-APIC addr = 0x{:x}",
                self.vmread(control::VIRT_APIC_ADDR_FULL)
            );
        }

        if pin_based_exec_ctl.contains(PinbasedControls::POSTED_INTERRUPTS) {
            error!(
                "PostedIntrVec = 0x{:x}",
                self.vmread(control::POSTED_INTERRUPT_NOTIFICATION_VECTOR)
            );
        }

        if secondary_exec_control.contains(SecondaryControls::ENABLE_EPT) {
            error!("EPT pointer = 0x{:x}", self.vmread(control::EPTP_FULL));
        }
        if secondary_exec_control.contains(SecondaryControls::PAUSE_LOOP_EXITING) {
            error!(
                "PLE Gap = 0x{:x}, Window = 0x{:x}",
                self.vmread(control::PLE_GAP),
                self.vmread(control::PLE_WINDOW)
            );
        }
        if secondary_exec_control.contains(SecondaryControls::ENABLE_VPID) {
            error!("Virtual processor ID = 0x{:x}", self.vmread(control::VPID));
        }
    }

    pub fn dump_sel(&self, name: &'static str, sel: u32) {
        error!(
            "{name} sel = 0x{:x}, attr = 0x{:x}, limit = 0x{:x}, base = 0x{:x}",
            self.vmread(sel),
            self.vmread(sel + guest::ES_ACCESS_RIGHTS - guest::ES_SELECTOR),
            self.vmread(sel + guest::ES_LIMIT - guest::ES_SELECTOR),
            self.vmread(sel + guest::ES_BASE - guest::ES_SELECTOR),
        );
    }

    pub fn dump_dtsel(&self, name: &'static str, limit: u32) {
        error!(
            "{name} limit = 0x{:x}, base = 0x{:x}",
            self.vmread(limit),
            self.vmread(limit + guest::GDTR_BASE - guest::GDTR_LIMIT)
        );
    }

    pub fn dump_msrs(&self, name: &'static str, msr: &VmxMsrs) {
        error!("MSR {name}:");
        for (idx, msr) in msr.val.iter().enumerate() {
            error!("{idx}: msr = 0x{:x}, value = 0x{:x}", msr.index, msr.data);
        }
    }

    #[inline]
    pub fn vmread(&self, field: u32) -> u64 {
        VmxAsm::vmx_vmread(field)
    }

    fn setup_uret_msrs(&self, vcpu: &mut VirtCpu) {
        // 是否加载syscall相关msr
        let load_syscall_msrs =
            vcpu.arch.is_long_mode() && vcpu.arch.efer.contains(EferFlags::SYSTEM_CALL_EXTENSIONS);

        self.setup_uret_msr(vcpu, msr::IA32_STAR, load_syscall_msrs);
        self.setup_uret_msr(vcpu, msr::IA32_LSTAR, load_syscall_msrs);
        self.setup_uret_msr(vcpu, msr::IA32_FMASK, load_syscall_msrs);

        let load_efer = self.update_transition_efer(vcpu);
        self.setup_uret_msr(vcpu, msr::IA32_EFER, load_efer);

        // TODO: MSR_TSC_AUX

        self.setup_uret_msr(
            vcpu,
            msr::MSR_IA32_TSX_CTRL,
            CpuId::default()
                .get_extended_feature_info()
                .unwrap()
                .has_rtm(),
        );

        vcpu.vmx_mut().guest_uret_msrs_loaded = false;
    }

    fn setup_uret_msr(&self, vcpu: &mut VirtCpu, msr: u32, load_into_hardware: bool) {
        let uret_msr = vcpu.vmx_mut().find_uret_msr_mut(msr);

        if let Some((_idx, msr)) = uret_msr {
            msr.load_into_hardware = load_into_hardware;
        }
    }

    fn update_transition_efer(&self, vcpu: &mut VirtCpu) -> bool {
        let mut guest_efer = vcpu.arch.efer;
        let mut ignore_efer = EferFlags::empty();
        if !self.enable_ept {
            guest_efer.insert(EferFlags::NO_EXECUTE_ENABLE);
        }

        ignore_efer.insert(EferFlags::SYSTEM_CALL_EXTENSIONS);

        ignore_efer.insert(EferFlags::LONG_MODE_ACTIVE | EferFlags::LONG_MODE_ENABLE);

        if guest_efer.contains(EferFlags::LONG_MODE_ACTIVE) {
            ignore_efer.remove(EferFlags::SYSTEM_CALL_EXTENSIONS);
        }

        if self.has_load_ia32_efer()
            || (self.enable_ept
                && (vcpu.arch.efer ^ x86_kvm_manager().host_efer)
                    .contains(EferFlags::NO_EXECUTE_ENABLE))
        {
            if !guest_efer.contains(EferFlags::LONG_MODE_ACTIVE) {
                guest_efer.remove(EferFlags::LONG_MODE_ENABLE);
            }

            if guest_efer != x86_kvm_manager().host_efer {
                vcpu.vmx_mut().add_atomic_switch_msr(
                    msr::IA32_EFER,
                    guest_efer.bits(),
                    x86_kvm_manager().host_efer.bits(),
                    false,
                );
            } else {
                vcpu.vmx_mut().clear_atomic_switch_msr(msr::IA32_EFER);
            }

            return false;
        }

        let idx = x86_kvm_manager().find_user_return_msr_idx(msr::IA32_EFER);
        if let Some(i) = idx {
            vcpu.vmx_mut().clear_atomic_switch_msr(msr::IA32_EFER);

            guest_efer.remove(ignore_efer);
            guest_efer.insert(x86_kvm_manager().host_efer & ignore_efer);

            vcpu.vmx_mut().guest_uret_msrs[i].data = guest_efer.bits();
            vcpu.vmx_mut().guest_uret_msrs[i].mask = (!ignore_efer).bits();
            return true;
        } else {
            return false;
        }
    }

    fn set_cr4_guest_host_mask(&self, arch: &mut VirtCpuArch) {
        arch.cr4_guest_owned_bits =
            x86_kvm_manager().possible_cr4_guest & (!arch.cr4_guest_rsvd_bits);

        if !self.enable_ept {
            arch.cr4_guest_owned_bits
                .remove(x86_kvm_manager().cr4_tlbflush_bits);
            arch.cr4_guest_owned_bits
                .remove(x86_kvm_manager().cr4_pdptr_bits);
        }

        if arch.is_guest_mode() {
            // 嵌套todo
            todo!()
        }

        VmxAsm::vmx_vmwrite(
            control::CR4_GUEST_HOST_MASK,
            (!arch.cr4_guest_owned_bits).bits() as u64,
        );
    }

    fn l1_guest_owned_cr0_bits(&self) -> Cr0 {
        let mut cr0 = x86_kvm_manager().possible_cr0_guest;

        if !self.enable_ept {
            cr0.remove(Cr0::CR0_WRITE_PROTECT)
        }

        return cr0;
    }

    /// 设置在guest生命周期中host不变的部分
    fn set_constant_host_state(&self, vcpu: &mut VirtCpu) {
        let loaded_vmcs_host_state = &mut vcpu.vmx().loaded_vmcs.lock().host_state;

        VmxAsm::vmx_vmwrite(host::CR0, unsafe { cr0() }.bits() as u64);

        let cr3: (PhysFrame, Cr3Flags) = Cr3::read();
        let cr3_combined: u64 =
            (cr3.0.start_address().as_u64() & 0xFFFF_FFFF_FFFF_F000) | (cr3.1.bits() & 0xFFF);
        VmxAsm::vmx_vmwrite(host::CR3, cr3_combined);
        loaded_vmcs_host_state.cr3 = cr3;

        let cr4 = unsafe { cr4() };
        VmxAsm::vmx_vmwrite(host::CR4, cr4.bits() as u64);
        loaded_vmcs_host_state.cr4 = cr4;

        VmxAsm::vmx_vmwrite(
            host::CS_SELECTOR,
            (segmentation::cs().bits() & (!0x07)).into(),
        );

        VmxAsm::vmx_vmwrite(host::DS_SELECTOR, 0);
        VmxAsm::vmx_vmwrite(host::ES_SELECTOR, 0);

        VmxAsm::vmx_vmwrite(
            host::SS_SELECTOR,
            (segmentation::ds().bits() & (!0x07)).into(),
        );
        VmxAsm::vmx_vmwrite(
            host::TR_SELECTOR,
            (unsafe { x86::task::tr().bits() } & (!0x07)).into(),
        );

        VmxAsm::vmx_vmwrite(host::IDTR_BASE, self.host_idt_base);
        VmxAsm::vmx_vmwrite(host::RIP, vmx_vmexit as usize as u64);

        let val = unsafe { rdmsr(msr::IA32_SYSENTER_CS) };

        // low32
        VmxAsm::vmx_vmwrite(host::IA32_SYSENTER_CS, (val << 32) >> 32);

        // VmxAsm::vmx_vmwrite(host::IA32_SYSENTER_ESP, 0);

        let tmp = unsafe { rdmsr(msr::IA32_SYSENTER_EIP) };
        VmxAsm::vmx_vmwrite(host::IA32_SYSENTER_EIP, (tmp << 32) >> 32);

        if self
            .vmcs_config
            .vmexit_ctrl
            .contains(ExitControls::LOAD_IA32_PAT)
        {
            VmxAsm::vmx_vmwrite(host::IA32_PAT_FULL, unsafe { rdmsr(msr::IA32_PAT) });
        }

        if self.has_load_ia32_efer() {
            VmxAsm::vmx_vmwrite(
                host::IA32_EFER_FULL,
                x86_kvm_manager().host_efer.bits() as u64,
            );
        }
    }

    fn get_pin_based_exec_controls(&self, vcpu: &VirtCpu) -> PinbasedControls {
        let mut ctrls = self.vmcs_config.pin_based_exec_ctrl;

        if !vcpu.arch.vcpu_apicv_active() {
            ctrls.remove(PinbasedControls::POSTED_INTERRUPTS);
        }

        if !self.enable_vnmi {
            ctrls.remove(PinbasedControls::VIRTUAL_NMIS);
        }

        if !self.enable_preemption_timer {
            ctrls.remove(PinbasedControls::VMX_PREEMPTION_TIMER);
        }

        return ctrls;
    }

    fn get_exec_controls(&self, vcpu: &VirtCpu, vmarch: &KvmArch) -> PrimaryControls {
        let mut ctrls = self.vmcs_config.cpu_based_exec_ctrl;

        ctrls.remove(
            PrimaryControls::RDTSC_EXITING
                | PrimaryControls::USE_IO_BITMAPS
                | PrimaryControls::MONITOR_TRAP_FLAG
                | PrimaryControls::PAUSE_EXITING,
        );

        ctrls.remove(
            PrimaryControls::NMI_WINDOW_EXITING | PrimaryControls::INTERRUPT_WINDOW_EXITING,
        );

        ctrls.remove(PrimaryControls::MOV_DR_EXITING);

        if vcpu.arch.lapic_in_kernel() && self.has_tpr_shadow() {
            ctrls.remove(PrimaryControls::USE_TPR_SHADOW);
        }

        if ctrls.contains(PrimaryControls::USE_TPR_SHADOW) {
            ctrls.remove(PrimaryControls::CR8_LOAD_EXITING | PrimaryControls::CR8_STORE_EXITING);
        } else {
            ctrls.insert(PrimaryControls::CR8_LOAD_EXITING | PrimaryControls::CR8_STORE_EXITING);
        }

        if self.enable_ept {
            ctrls.remove(
                PrimaryControls::CR3_LOAD_EXITING
                    | PrimaryControls::CR3_STORE_EXITING
                    | PrimaryControls::INVLPG_EXITING,
            );
        }

        if vmarch.mwait_in_guest {
            ctrls.remove(PrimaryControls::MWAIT_EXITING | PrimaryControls::MONITOR_EXITING);
        }

        if vmarch.hlt_in_guest {
            ctrls.remove(PrimaryControls::HLT_EXITING);
        }

        return ctrls;
    }

    fn get_secondary_exec_controls(&mut self, vcpu: &VirtCpu, vm: &Vm) -> SecondaryControls {
        let mut ctrls = self.vmcs_config.cpu_based_2nd_exec_ctrl;

        if self.pt_mode == ProcessorTraceMode::System {
            ctrls.remove(
                SecondaryControls::INTEL_PT_GUEST_PHYSICAL | SecondaryControls::CONCEAL_VMX_FROM_PT,
            );
        }

        if !(self.enable_flexpriority && vcpu.arch.lapic_in_kernel()) {
            ctrls.remove(SecondaryControls::VIRTUALIZE_APIC)
        }

        if vcpu.vmx().vpid == 0 {
            ctrls.remove(SecondaryControls::ENABLE_VPID);
        }

        if !self.enable_ept {
            ctrls.remove(SecondaryControls::ENABLE_EPT);
            self.enable_unrestricted_guest = false;
        }

        if !self.enable_unrestricted_guest {
            ctrls.remove(SecondaryControls::UNRESTRICTED_GUEST);
        }

        if vm.arch.pause_in_guest {
            ctrls.remove(SecondaryControls::PAUSE_LOOP_EXITING);
        }
        if !vcpu.arch.vcpu_apicv_active() {
            ctrls.remove(
                SecondaryControls::VIRTUALIZE_APIC_REGISTER
                    | SecondaryControls::VIRTUAL_INTERRUPT_DELIVERY,
            );
        }

        ctrls.remove(SecondaryControls::VIRTUALIZE_X2APIC);

        ctrls.remove(SecondaryControls::ENABLE_VM_FUNCTIONS);

        ctrls.remove(SecondaryControls::DTABLE_EXITING);

        ctrls.remove(SecondaryControls::VMCS_SHADOWING);

        if !self.enable_pml || vm.nr_memslots_dirty_logging == 0 {
            ctrls.remove(SecondaryControls::ENABLE_PML);
        }

        // TODO: vmx_adjust_sec_exec_feature

        if self.has_rdtscp() {
            warn!("adjust RDTSCP todo!");
            // todo!()
        }

        return ctrls;
    }

    fn get_vmexit_controls(&self) -> ExitControls {
        let mut ctrls = self.vmcs_config.vmexit_ctrl;

        ctrls.remove(
            ExitControls::SAVE_IA32_PAT
                | ExitControls::SAVE_IA32_EFER
                | ExitControls::SAVE_VMX_PREEMPTION_TIMER,
        );

        if self.pt_mode == ProcessorTraceMode::System {
            ctrls.remove(ExitControls::CONCEAL_VMX_FROM_PT | ExitControls::CLEAR_IA32_RTIT_CTL);
        }

        // todo: cpu_has_perf_global_ctrl_bug

        ctrls.remove(ExitControls::LOAD_IA32_PERF_GLOBAL_CTRL | ExitControls::LOAD_IA32_EFER);

        ctrls
    }

    fn get_vmentry_controls(&self) -> EntryControls {
        let mut ctrls = self.vmcs_config.vmentry_ctrl;

        if self.pt_mode == ProcessorTraceMode::System {
            ctrls.remove(EntryControls::CONCEAL_VMX_FROM_PT | EntryControls::LOAD_IA32_RTIT_CTL);
        }

        ctrls.remove(
            EntryControls::LOAD_IA32_PERF_GLOBAL_CTRL
                | EntryControls::LOAD_IA32_EFER
                | EntryControls::IA32E_MODE_GUEST,
        );

        // todo: cpu_has_perf_global_ctrl_bug

        ctrls
    }

    pub fn emulation_required(&self, vcpu: &mut VirtCpu) -> bool {
        return self.emulate_invalid_guest_state && !self.guest_state_valid(vcpu);
    }

    pub fn guest_state_valid(&self, vcpu: &mut VirtCpu) -> bool {
        return vcpu.is_unrestricted_guest() || self.__guest_state_valid(vcpu);
    }

    pub fn __guest_state_valid(&self, vcpu: &mut VirtCpu) -> bool {
        if vcpu.arch.is_portected_mode()
            || x86_kvm_ops().get_rflags(vcpu).contains(RFlags::FLAGS_VM)
        {
            if !self.rmode_segment_valid(vcpu, VcpuSegment::CS) {
                return false;
            }
            if !self.rmode_segment_valid(vcpu, VcpuSegment::SS) {
                return false;
            }
            if !self.rmode_segment_valid(vcpu, VcpuSegment::DS) {
                return false;
            }
            if !self.rmode_segment_valid(vcpu, VcpuSegment::ES) {
                return false;
            }
            if !self.rmode_segment_valid(vcpu, VcpuSegment::FS) {
                return false;
            }
            if !self.rmode_segment_valid(vcpu, VcpuSegment::GS) {
                return false;
            }
        } else {
            todo!("protected mode guest state checks todo");
        }

        return true;
    }

    pub fn vmx_get_segment(
        &self,
        vcpu: &mut VirtCpu,
        mut var: UapiKvmSegment,
        seg: VcpuSegment,
    ) -> UapiKvmSegment {
        if vcpu.vmx().rmode.vm86_active && seg != VcpuSegment::LDTR {
            var = vcpu.vmx().rmode.segs[seg as usize];
            if seg == VcpuSegment::TR || var.selector == Vmx::vmx_read_guest_seg_selector(vcpu, seg)
            {
                return var;
            }

            var.base = Vmx::vmx_read_guest_seg_base(vcpu, seg);
            var.selector = Vmx::vmx_read_guest_seg_selector(vcpu, seg);
            return var;
        }

        var.base = Vmx::vmx_read_guest_seg_base(vcpu, seg);
        var.limit = Vmx::vmx_read_guest_seg_limit(vcpu, seg);
        var.selector = Vmx::vmx_read_guest_seg_selector(vcpu, seg);

        let ar = Vmx::vmx_read_guest_seg_ar(vcpu, seg);

        var.unusable = ((ar >> 16) & 1) as u8;
        var.type_ = (ar & 15) as u8;
        var.s = ((ar >> 4) & 1) as u8;
        var.dpl = ((ar >> 5) & 3) as u8;

        var.present = !var.unusable;
        var.avl = ((ar >> 12) & 1) as u8;
        var.l = ((ar >> 13) & 1) as u8;
        var.db = ((ar >> 14) & 1) as u8;
        var.g = ((ar >> 15) & 1) as u8;

        return var;
    }

    pub fn _vmx_set_segment(
        &self,
        vcpu: &mut VirtCpu,
        mut var: UapiKvmSegment,
        seg: VcpuSegment,
    ) -> UapiKvmSegment {
        let sf = &KVM_VMX_SEGMENT_FIELDS[seg as usize];

        vcpu.vmx_mut().segment_cache_clear();

        if vcpu.vmx().rmode.vm86_active && seg != VcpuSegment::LDTR {
            vcpu.vmx_mut().rmode.segs[seg as usize] = var;
            if seg == VcpuSegment::TR {
                VmxAsm::vmx_vmwrite(sf.selector, var.selector as u64);
            } else if var.s != 0 {
                Vmx::fix_rmode_seg(seg, &vcpu.vmx().rmode.segs[seg as usize]);
            }
            return var;
        }

        VmxAsm::vmx_vmwrite(sf.base, var.base);
        VmxAsm::vmx_vmwrite(sf.limit, var.limit as u64);
        VmxAsm::vmx_vmwrite(sf.selector, var.selector as u64);

        if vcpu.is_unrestricted_guest() && seg != VcpuSegment::LDTR {
            var.type_ |= 0x1;
        }

        VmxAsm::vmx_vmwrite(sf.ar_bytes, var.vmx_segment_access_rights() as u64);
        return var;
    }

    pub fn rmode_segment_valid(&self, vcpu: &mut VirtCpu, seg: VcpuSegment) -> bool {
        let mut var = UapiKvmSegment::default();
        var = self.vmx_get_segment(vcpu, var, seg);

        var.dpl = 0x3;

        if seg == VcpuSegment::CS {
            var.type_ = 0x3;
        }

        let ar = var.vmx_segment_access_rights();

        if var.base != ((var.selector as u64) << 4) {
            return false;
        }

        if var.limit != 0xffff {
            return false;
        }

        if ar != 0xf3 {
            return false;
        }

        true
    }

    pub fn fix_rmode_seg(seg: VcpuSegment, save: &UapiKvmSegment) {
        let sf = &KVM_VMX_SEGMENT_FIELDS[seg as usize];

        let mut var = *save;
        var.dpl = 0x3;
        if seg == VcpuSegment::CS {
            var.type_ = 0x3;
        }

        if !vmx_info().emulate_invalid_guest_state {
            var.selector = (var.base >> 4) as u16;
            var.base &= 0xffff0;
            var.limit = 0xffff;
            var.g = 0;
            var.db = 0;
            var.present = 1;
            var.s = 1;
            var.l = 0;
            var.unusable = 0;
            var.type_ = 0x3;
            var.avl = 0;
            if save.base & 0xf != 0 {
                warn!("segment base is not paragraph aligned when entering protected mode (seg={seg:?})");
            }
        }

        VmxAsm::vmx_vmwrite(sf.selector, var.selector as u64);
        VmxAsm::vmx_vmwrite(sf.base, var.base);
        VmxAsm::vmx_vmwrite(sf.limit, var.limit as u64);
        VmxAsm::vmx_vmwrite(sf.ar_bytes, var.vmx_segment_access_rights() as u64);
    }

    pub fn fix_pmode_seg(
        &self,
        vcpu: &mut VirtCpu,
        seg: VcpuSegment,
        mut save: UapiKvmSegment,
    ) -> UapiKvmSegment {
        if self.emulate_invalid_guest_state {
            if seg == VcpuSegment::CS || seg == VcpuSegment::SS {
                save.selector &= !0x3;
            }

            save.dpl = (save.selector & 0x3) as u8;
            save.s = 1;
        }

        self._vmx_set_segment(vcpu, save, seg);

        return save;
    }

    pub fn enter_pmode(&self, vcpu: &mut VirtCpu) {
        self.get_segment_with_rmode(vcpu, VcpuSegment::ES);
        self.get_segment_with_rmode(vcpu, VcpuSegment::DS);
        self.get_segment_with_rmode(vcpu, VcpuSegment::FS);
        self.get_segment_with_rmode(vcpu, VcpuSegment::GS);
        self.get_segment_with_rmode(vcpu, VcpuSegment::SS);
        self.get_segment_with_rmode(vcpu, VcpuSegment::CS);

        vcpu.vmx_mut().rmode.vm86_active = false;

        self.set_segment_with_rmode(vcpu, VcpuSegment::TR);

        let mut flags = RFlags::from_bits_truncate(VmxAsm::vmx_vmread(guest::RFLAGS));

        flags.remove(RFlags::FLAGS_IOPL3 | RFlags::FLAGS_VM);

        flags.insert(vcpu.vmx().rmode.save_rflags & (RFlags::FLAGS_IOPL3 | RFlags::FLAGS_VM));

        VmxAsm::vmx_vmwrite(guest::RFLAGS, flags.bits());

        let cr4 = (Cr4::from_bits_truncate(VmxAsm::vmx_vmread(guest::CR4) as usize)
            & (!Cr4::CR4_ENABLE_VME))
            | (Cr4::from_bits_truncate(VmxAsm::vmx_vmread(control::CR4_READ_SHADOW) as usize)
                & Cr4::CR4_ENABLE_VME);
        VmxAsm::vmx_vmwrite(guest::CR4, cr4.bits() as u64);

        VmxKvmFunc.update_exception_bitmap(vcpu);

        self.fix_pmode_seg_with_rmode(vcpu, VcpuSegment::CS);
        self.fix_pmode_seg_with_rmode(vcpu, VcpuSegment::SS);
        self.fix_pmode_seg_with_rmode(vcpu, VcpuSegment::ES);
        self.fix_pmode_seg_with_rmode(vcpu, VcpuSegment::DS);
        self.fix_pmode_seg_with_rmode(vcpu, VcpuSegment::FS);
        self.fix_pmode_seg_with_rmode(vcpu, VcpuSegment::GS);
    }

    fn fix_pmode_seg_with_rmode(&self, vcpu: &mut VirtCpu, seg: VcpuSegment) {
        let segment = vcpu.vmx().rmode.segs[seg as usize];
        vcpu.vmx_mut().rmode.segs[seg as usize] = self.fix_pmode_seg(vcpu, seg, segment);
    }

    fn get_segment_with_rmode(&self, vcpu: &mut VirtCpu, seg: VcpuSegment) {
        let segment = vcpu.vmx().rmode.segs[seg as usize];
        vcpu.vmx_mut().rmode.segs[seg as usize] = self.vmx_get_segment(vcpu, segment, seg);
    }

    fn set_segment_with_rmode(&self, vcpu: &mut VirtCpu, seg: VcpuSegment) {
        let segment = vcpu.vmx().rmode.segs[seg as usize];
        vcpu.vmx_mut().rmode.segs[seg as usize] = self._vmx_set_segment(vcpu, segment, seg);
    }

    pub fn enter_rmode(&self, vcpu: &mut VirtCpu, vm: &Vm) {
        let kvm_vmx = vm.kvm_vmx();

        self.get_segment_with_rmode(vcpu, VcpuSegment::TR);
        self.get_segment_with_rmode(vcpu, VcpuSegment::ES);
        self.get_segment_with_rmode(vcpu, VcpuSegment::DS);
        self.get_segment_with_rmode(vcpu, VcpuSegment::FS);
        self.get_segment_with_rmode(vcpu, VcpuSegment::GS);
        self.get_segment_with_rmode(vcpu, VcpuSegment::SS);
        self.get_segment_with_rmode(vcpu, VcpuSegment::CS);

        vcpu.vmx_mut().rmode.vm86_active = true;

        vcpu.vmx_mut().segment_cache_clear();

        VmxAsm::vmx_vmwrite(guest::TR_BASE, kvm_vmx.tss_addr as u64);
        VmxAsm::vmx_vmwrite(guest::TR_LIMIT, RMODE_TSS_SIZE as u64 - 1);
        VmxAsm::vmx_vmwrite(guest::TR_ACCESS_RIGHTS, 0x008b);

        let mut flags = RFlags::from_bits_truncate(VmxAsm::vmx_vmread(guest::RFLAGS));
        vcpu.vmx_mut().rmode.save_rflags = flags;

        flags.insert(RFlags::FLAGS_IOPL3 | RFlags::FLAGS_VM);

        VmxAsm::vmx_vmwrite(guest::RFLAGS, flags.bits());
        VmxAsm::vmx_vmwrite(
            guest::CR4,
            VmxAsm::vmx_vmread(guest::CR4) | Cr4::CR4_ENABLE_VME.bits() as u64,
        );

        VmxKvmFunc.update_exception_bitmap(vcpu);

        self.fix_rmode_seg_with_rmode(vcpu, VcpuSegment::SS);
        self.fix_rmode_seg_with_rmode(vcpu, VcpuSegment::CS);
        self.fix_rmode_seg_with_rmode(vcpu, VcpuSegment::ES);
        self.fix_rmode_seg_with_rmode(vcpu, VcpuSegment::DS);
        self.fix_rmode_seg_with_rmode(vcpu, VcpuSegment::GS);
        self.fix_rmode_seg_with_rmode(vcpu, VcpuSegment::FS);
    }

    fn fix_rmode_seg_with_rmode(&self, vcpu: &VirtCpu, seg: VcpuSegment) {
        Vmx::fix_rmode_seg(seg, &vcpu.vmx().rmode.segs[seg as usize]);
    }

    pub fn vmx_read_guest_seg_ar(vcpu: &mut VirtCpu, seg: VcpuSegment) -> u32 {
        if !Vmx::vmx_segment_cache_test_set(vcpu, seg, SegmentCacheField::AR) {
            vcpu.vmx_mut().segment_cache.seg[seg as usize].ar =
                VmxAsm::vmx_vmread(KVM_VMX_SEGMENT_FIELDS[seg as usize].ar_bytes) as u32;
        }

        return vcpu.vmx().segment_cache.seg[seg as usize].ar;
    }

    pub fn vmx_read_guest_seg_selector(vcpu: &mut VirtCpu, seg: VcpuSegment) -> u16 {
        if !Vmx::vmx_segment_cache_test_set(vcpu, seg, SegmentCacheField::SEL) {
            vcpu.vmx_mut().segment_cache.seg[seg as usize].selector =
                VmxAsm::vmx_vmread(KVM_VMX_SEGMENT_FIELDS[seg as usize].selector) as u16;
        }

        return vcpu.vmx().segment_cache.seg[seg as usize].selector;
    }

    pub fn vmx_read_guest_seg_base(vcpu: &mut VirtCpu, seg: VcpuSegment) -> u64 {
        if !Vmx::vmx_segment_cache_test_set(vcpu, seg, SegmentCacheField::BASE) {
            vcpu.vmx_mut().segment_cache.seg[seg as usize].base =
                VmxAsm::vmx_vmread(KVM_VMX_SEGMENT_FIELDS[seg as usize].base);
        }

        return vcpu.vmx().segment_cache.seg[seg as usize].base;
    }

    pub fn vmx_read_guest_seg_limit(vcpu: &mut VirtCpu, seg: VcpuSegment) -> u32 {
        if !Vmx::vmx_segment_cache_test_set(vcpu, seg, SegmentCacheField::LIMIT) {
            vcpu.vmx_mut().segment_cache.seg[seg as usize].limit =
                VmxAsm::vmx_vmread(KVM_VMX_SEGMENT_FIELDS[seg as usize].limit) as u32;
        }

        return vcpu.vmx().segment_cache.seg[seg as usize].limit;
    }

    fn vmx_segment_cache_test_set(
        vcpu: &mut VirtCpu,
        seg: VcpuSegment,
        field: SegmentCacheField,
    ) -> bool {
        let mask = 1u32 << (seg as usize * SegmentCacheField::NR as usize + field as usize);

        if !vcpu.arch.is_register_available(KvmReg::VcpuExregSegments) {
            vcpu.arch.mark_register_available(KvmReg::VcpuExregSegments);
            vcpu.vmx_mut().segment_cache_clear();
        }

        let ret = vcpu.vmx().segment_cache.bitmask & mask;

        vcpu.vmx_mut().segment_cache.bitmask |= mask;

        return ret != 0;
    }

    pub fn vmx_vcpu_enter_exit(vcpu: &mut VirtCpu, flags: VmxRunFlag) {
        // TODO: vmx_l1d_should_flush and mmio_stale_data_clear

        // TODO: vmx_disable_fb_clear

        if vcpu.arch.cr2 != unsafe { cr2() } as u64 {
            unsafe { cr2_write(vcpu.arch.cr2) };
        }

        let fail =
            unsafe { __vmx_vcpu_run(vcpu.vmx(), vcpu.arch.regs.as_ptr(), flags.bits as u32) };

        vcpu.vmx_mut().fail = fail as u8;

        vcpu.arch.cr2 = unsafe { cr2() } as u64;
        vcpu.arch.regs_avail.set_all(true);

        // 这些寄存器需要更新缓存
        for reg_idx in Vmx::VMX_REGS_LAZY_LOAD_SET {
            vcpu.arch.regs_avail.set(*reg_idx, false);
        }

        vcpu.vmx_mut().idt_vectoring_info = IntrInfo::empty();

        // TODO: enable_fb_clear

        if unlikely(vcpu.vmx().fail != 0) {
            vcpu.vmx_mut().exit_reason = VmxExitReason::from(0xdead);
            return;
        }

        vcpu.vmx_mut().exit_reason =
            VmxExitReason::from(VmxAsm::vmx_vmread(ro::EXIT_REASON) as u32);

        if likely(!vcpu.vmx().exit_reason.failed_vmentry()) {
            vcpu.vmx_mut().idt_vectoring_info =
                IntrInfo::from_bits_truncate(VmxAsm::vmx_vmread(ro::IDT_VECTORING_INFO) as u32);
        }

        if VmxExitReasonBasic::from(vcpu.vmx().exit_reason.basic())
            == VmxExitReasonBasic::EXCEPTION_OR_NMI
            && VmcsIntrHelper::is_nmi(&Vmx::vmx_get_intr_info(vcpu))
        {
            todo!()
        }
    }

    fn vmx_get_intr_info(vcpu: &mut VirtCpu) -> IntrInfo {
        if !vcpu
            .arch
            .test_and_mark_available(KvmReg::VcpuExregExitInfo2)
        {
            vcpu.vmx_mut().exit_intr_info = IntrInfo::from_bits_truncate(VmxAsm::vmx_vmread(
                ro::VMEXIT_INTERRUPTION_INFO,
            ) as u32);
        }

        return vcpu.vmx_mut().exit_intr_info;
    }

    pub fn vmx_exit_handlers_fastpath(vcpu: &mut VirtCpu) -> ExitFastpathCompletion {
        match VmxExitReasonBasic::from(vcpu.vmx().exit_reason.basic()) {
            VmxExitReasonBasic::WRMSR => {
                todo!()
            }
            VmxExitReasonBasic::VMX_PREEMPTION_TIMER_EXPIRED => {
                todo!()
            }
            _ => ExitFastpathCompletion::None,
        }
    }

    pub fn vmx_handle_exit(
        &self,
        vcpu: &mut VirtCpu,
        vm: &Vm,
        exit_fastpath: ExitFastpathCompletion,
    ) -> Result<i32, SystemError> {
        let exit_reason = vcpu.vmx().exit_reason;
        // self.dump_vmcs(vcpu);
        {
            let reason = self.vmread(ro::EXIT_REASON);
            debug!("vm_exit reason 0x{:x}\n", reason);
        }
        let unexpected_vmexit = |vcpu: &mut VirtCpu| -> Result<i32, SystemError> {
            error!("vmx: unexpected exit reason {:?}\n", exit_reason);

            self.dump_vmcs(vcpu);

            let cpu = vcpu.arch.last_vmentry_cpu.into() as u64;
            let run = vcpu.kvm_run_mut();
            run.exit_reason = kvm_exit::KVM_EXIT_INTERNAL_ERROR;

            unsafe {
                run.__bindgen_anon_1.internal.ndata = 2;
                run.__bindgen_anon_1.internal.data[0] = Into::<u32>::into(exit_reason) as u64;
                run.__bindgen_anon_1.internal.data[1] = cpu;
            }

            return Ok(0);
        };

        let vectoring_info = vcpu.vmx().idt_vectoring_info;

        if self.enable_pml && !vcpu.arch.is_guest_mode() {
            todo!()
        }

        if vcpu.arch.is_guest_mode() {
            if exit_reason.basic() == VmxExitReasonBasic::PML_FULL as u16 {
                return unexpected_vmexit(vcpu);
            }

            todo!()
        }

        if vcpu.vmx().emulation_required {
            todo!()
        }

        if exit_reason.failed_vmentry() {
            self.dump_vmcs(vcpu);
            todo!()
        }

        if unlikely(vcpu.vmx().fail != 0) {
            self.dump_vmcs(vcpu);
            todo!()
        }

        let basic = VmxExitReasonBasic::from(exit_reason.basic());
        if vectoring_info.contains(IntrInfo::INTR_INFO_VALID_MASK)
            && basic != VmxExitReasonBasic::EXCEPTION_OR_NMI
            && basic != VmxExitReasonBasic::EPT_VIOLATION
            && basic != VmxExitReasonBasic::PML_FULL
            && basic != VmxExitReasonBasic::APIC_ACCESS
            && basic != VmxExitReasonBasic::TASK_SWITCH
            && basic != VmxExitReasonBasic::NOTIFY
        {
            todo!()
        }

        if unlikely(!self.enable_pml && vcpu.vmx().loaded_vmcs().soft_vnmi_blocked) {
            todo!()
        }

        if exit_fastpath != ExitFastpathCompletion::None {
            return Err(SystemError::EINVAL);
        }

        match VmxExitHandlers::try_handle_exit(
            vcpu,
            vm,
            VmxExitReasonBasic::from(exit_reason.basic()),
        ) {
            Some(Ok(r)) => {
                debug!("vmx: handled exit return {:?}\n", r);
                return Ok(r);
            }
            Some(Err(_)) | None => unexpected_vmexit(vcpu),
        }
    }

    #[allow(unreachable_code)]
    pub fn handle_external_interrupt_irqoff(vcpu: &mut VirtCpu) {
        let intr_info = Vmx::vmx_get_intr_info(vcpu);
        let _vector = intr_info & IntrInfo::INTR_INFO_VECTOR_MASK;
        // let desc = vmx_info().host_idt_base + vector.bits() as u64;
        if !VmcsIntrHelper::is_external_intr(&intr_info) {
            error!("unexpected VM-Exit interrupt info: {:?}", intr_info);
            return;
        }

        vcpu.arch.kvm_before_interrupt(KvmIntrType::Irq);
        // TODO
        warn!("handle_external_interrupt_irqoff TODO");
        vcpu.arch.kvm_after_interrupt();

        vcpu.arch.at_instruction_boundary = true;
    }

    /// 需要在缓存中更新的寄存器集。此处未列出的其他寄存器在 VM 退出后立即同步到缓存。
    pub const VMX_REGS_LAZY_LOAD_SET: &'static [usize] = &[
        KvmReg::VcpuRegsRip as usize,
        KvmReg::VcpuRegsRsp as usize,
        KvmReg::VcpuExregRflags as usize,
        KvmReg::NrVcpuRegs as usize,
        KvmReg::VcpuExregSegments as usize,
        KvmReg::VcpuExregCr0 as usize,
        KvmReg::VcpuExregCr3 as usize,
        KvmReg::VcpuExregCr4 as usize,
        KvmReg::VcpuExregExitInfo1 as usize,
        KvmReg::VcpuExregExitInfo2 as usize,
    ];
}

extern "C" {
    /// #[allow(improper_ctypes)]因为只需要在内部调用而无需与C交互
    #[allow(improper_ctypes)]
    fn __vmx_vcpu_run(vmx: &VmxVCpuPriv, regs: *const u64, flags: u32) -> i32;
}

struct VmcsEntryExitPair {
    entry: EntryControls,
    exit: ExitControls,
}

impl VmcsEntryExitPair {
    pub const fn new(entry: EntryControls, exit: ExitControls) -> Self {
        Self { entry, exit }
    }
}

#[derive(Debug, Default)]
#[repr(C, align(64))]
pub struct PostedIntrDesc {
    pir: [u32; 8],
    control: PostedIntrDescControl,
    // 保留位
    rsvd: [u32; 6],
}

#[bitfield(u64)]
pub struct PostedIntrDescControl {
    #[bits(1)]
    on: bool,
    #[bits(1)]
    sn: bool,
    #[bits(14)]
    rsvd_1: u16,
    nv: u8,
    rsvd_2: u8,
    ndst: u32,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct VmxUretMsr {
    load_into_hardware: bool,
    data: u64,
    mask: u64,
}

#[derive(Debug, Default)]
pub struct VmxMsrs {
    nr: usize,
    val: [VmxMsrEntry; Self::MAX_NR_LOADSTORE_MSRS],
}

impl VmxMsrs {
    pub const MAX_NR_LOADSTORE_MSRS: usize = 8;

    pub fn find_loadstore_msr_slot(&self, msr: u32) -> Option<usize> {
        return (0..self.nr).find(|&i| self.val[i].index == msr);
    }
}

#[derive(Debug, Default)]
pub struct VmxMsrAutoLoad {
    guest: VmxMsrs,
    host: VmxMsrs,
}

#[derive(Debug)]
pub struct VmxRMode {
    pub vm86_active: bool,
    pub save_rflags: RFlags,
    pub segs: [UapiKvmSegment; 8],
}

impl Default for VmxRMode {
    fn default() -> Self {
        Self {
            vm86_active: false,
            save_rflags: RFlags::empty(),
            segs: [UapiKvmSegment::default(); 8],
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct VmxSaveSegment {
    selector: u16,
    base: u64,
    limit: u32,
    ar: u32,
}

#[derive(Debug, Default)]
pub struct VmxSegmentCache {
    pub bitmask: u32,
    pub seg: [VmxSaveSegment; 8],
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct VmxVCpuPriv {
    vpid: u16,

    fail: u8,

    exit_reason: VmxExitReason,

    exit_intr_info: IntrInfo,

    idt_vectoring_info: IntrInfo,

    vmcs01: Arc<LockedLoadedVmcs>,
    loaded_vmcs: Arc<LockedLoadedVmcs>,
    guest_uret_msrs: [VmxUretMsr; KvmArchManager::KVM_MAX_NR_USER_RETURN_MSRS],
    guest_uret_msrs_loaded: bool,

    post_intr_desc: PostedIntrDesc,

    shadow_msr_intercept_read: AllocBitmap,
    shadow_msr_intercept_write: AllocBitmap,

    msr_ia32_feature_control: u64,
    msr_ia32_feature_control_valid_bits: u64,

    msr_host_kernel_gs_base: u64,
    msr_guest_kernel_gs_base: u64,

    emulation_required: bool,

    rflags: RFlags,

    ple_window: u32,
    ple_window_dirty: bool,

    msr_autoload: VmxMsrAutoLoad,
    msr_autostore: VmxMsrs,

    pml_pg: Box<[u8; MMArch::PAGE_SIZE]>,

    rmode: VmxRMode,

    spec_ctrl: u64,
    msr_ia32_umwait_control: u32,
    hv_deadline_tsc: u64,

    segment_cache: VmxSegmentCache,

    req_immediate_exit: bool,
    guest_state_loaded: bool,

    exit_qualification: u64, //暂时不知道用处fztodo
}

#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct KvmVmx {
    tss_addr: usize,
    ept_identity_pagetable_done: bool,
    ept_identity_map_addr: u64,
    pid_table: Option<Box<[u64; MMArch::PAGE_SIZE]>>,
}

impl KvmVmx {
    pub fn pid_table(&self) -> &[u64; MMArch::PAGE_SIZE] {
        self.pid_table.as_ref().unwrap().as_ref()
    }
}

impl VmxVCpuPriv {
    pub const PML_ENTITY_NUM: usize = 512;

    pub fn loaded_vmcs(&self) -> SpinLockGuard<LoadedVmcs> {
        self.loaded_vmcs.lock()
    }

    /// 参考：https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/vmx/vmx.c#7452
    pub fn init(vcpu: &mut VirtCpu, vm: &Vm) {
        let vmcs = LockedLoadedVmcs::new();

        // TODO: 改堆分配
        let mut vmx = Self {
            vpid: 0,
            fail: 0,
            vmcs01: vmcs.clone(),
            loaded_vmcs: vmcs,
            guest_uret_msrs: [VmxUretMsr::default(); KvmArchManager::KVM_MAX_NR_USER_RETURN_MSRS],
            shadow_msr_intercept_read: AllocBitmap::new(16),
            shadow_msr_intercept_write: AllocBitmap::new(16),
            post_intr_desc: PostedIntrDesc::default(),
            ple_window: 0,
            ple_window_dirty: false,
            msr_autoload: VmxMsrAutoLoad::default(),
            pml_pg: unsafe { Box::new_zeroed().assume_init() },
            guest_uret_msrs_loaded: false,
            msr_ia32_feature_control: 0,
            msr_ia32_feature_control_valid_bits: 0,
            rmode: VmxRMode::default(),
            spec_ctrl: 0,
            msr_ia32_umwait_control: 0,
            hv_deadline_tsc: u64::MAX,
            segment_cache: VmxSegmentCache::default(),
            emulation_required: false,
            rflags: RFlags::empty(),
            req_immediate_exit: false,
            guest_state_loaded: false,
            msr_host_kernel_gs_base: 0,
            msr_guest_kernel_gs_base: 0,
            idt_vectoring_info: IntrInfo::empty(),
            exit_reason: VmxExitReason::new(),
            exit_intr_info: IntrInfo::empty(),
            msr_autostore: VmxMsrs::default(),
            exit_qualification: 0, //fztodo
        };

        vmx.vpid = vmx_info().alloc_vpid().unwrap_or_default() as u16;

        for i in 0..x86_kvm_manager().kvm_uret_msrs_list.len() {
            vmx.guest_uret_msrs[i].mask = u64::MAX;
        }

        if CpuId::new().get_extended_feature_info().unwrap().has_rtm() {
            let tsx_ctrl = vmx.find_uret_msr_mut(msr::MSR_IA32_TSX_CTRL);
            if let Some((_idx, tsx_ctrl)) = tsx_ctrl {
                // Disable TSX enumeration
                tsx_ctrl.mask = !(1 << 1);
            }
        }

        vmx.shadow_msr_intercept_read.set_all(true);
        vmx.shadow_msr_intercept_write.set_all(true);

        let arch = &vm.arch;

        vmx.disable_intercept_for_msr(arch, msr::IA32_TIME_STAMP_COUNTER, MsrType::READ);
        vmx.disable_intercept_for_msr(arch, msr::IA32_FS_BASE, MsrType::RW);
        vmx.disable_intercept_for_msr(arch, msr::IA32_GS_BASE, MsrType::RW);
        vmx.disable_intercept_for_msr(arch, msr::IA32_KERNEL_GSBASE, MsrType::RW);

        vmx.disable_intercept_for_msr(arch, msr::IA32_SYSENTER_CS, MsrType::RW);
        vmx.disable_intercept_for_msr(arch, msr::IA32_SYSENTER_ESP, MsrType::RW);
        vmx.disable_intercept_for_msr(arch, msr::IA32_SYSENTER_EIP, MsrType::RW);

        if arch.pause_in_guest {
            vmx.disable_intercept_for_msr(arch, msr::MSR_CORE_C1_RESIDENCY, MsrType::READ);
            vmx.disable_intercept_for_msr(arch, msr::MSR_CORE_C3_RESIDENCY, MsrType::READ);
            vmx.disable_intercept_for_msr(arch, msr::MSR_CORE_C6_RESIDENCY, MsrType::READ);
            vmx.disable_intercept_for_msr(arch, msr::MSR_CORE_C7_RESIDENCY, MsrType::READ);
        }

        if vmx_info().enable_flexpriority && vcpu.arch.lapic_in_kernel() {
            todo!()
        }

        if vmx_info().enable_ept && !vmx_info().enable_unrestricted_guest {
            todo!()
        }

        if vcpu.arch.lapic_in_kernel() && vmx_info().enable_ipiv {
            todo!()
        }

        // 初始化vmx私有信息
        vcpu.private = Some(vmx);
    }

    pub fn find_uret_msr(&self, msr: u32) -> Option<(usize, &VmxUretMsr)> {
        let idx = x86_kvm_manager().find_user_return_msr_idx(msr);
        if let Some(index) = idx {
            return Some((index, &self.guest_uret_msrs[index]));
        } else {
            return None;
        }
    }

    fn set_uret_msr(&mut self, msr: u32, data: u64) {
        if let Some((_idx, msr)) = self.find_uret_msr_mut(msr) {
            msr.data = data;
        }
    }

    pub fn find_uret_msr_mut(&mut self, msr: u32) -> Option<(usize, &mut VmxUretMsr)> {
        let idx = x86_kvm_manager().find_user_return_msr_idx(msr);
        if let Some(index) = idx {
            return Some((index, &mut self.guest_uret_msrs[index]));
        } else {
            return None;
        }
    }

    fn set_guest_uret_msr(&mut self, slot: usize, data: u64) -> Result<(), SystemError> {
        let msr = &mut self.guest_uret_msrs[slot];
        if msr.load_into_hardware {
            x86_kvm_manager().kvm_set_user_return_msr(slot, data, msr.mask);
        }

        msr.data = data;

        Ok(())
    }

    /// ## 禁用对特定的 MSR 的拦截
    fn disable_intercept_for_msr(&mut self, arch: &KvmArch, msr: u32, mut msr_type: MsrType) {
        if !vmx_info().has_msr_bitmap() {
            return;
        }

        let msr_bitmap = &mut self.vmcs01.lock().msr_bitmap;

        // TODO: https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/vmx/vmx.c#3974
        // 嵌套vmx处理

        if Vmx::is_valid_passthrough_msr(msr) {
            if let Some(idx) = Vmx::possible_passthrough_msr_slot(msr) {
                if msr_type.contains(MsrType::READ) {
                    self.shadow_msr_intercept_read.set(idx, false);
                }
                if msr_type.contains(MsrType::WRITE) {
                    self.shadow_msr_intercept_write.set(idx, false);
                }
            }
        }

        if msr_type.contains(MsrType::READ)
            && !arch.msr_allowed(msr, MsrFilterType::KVM_MSR_FILTER_READ)
        {
            msr_bitmap.ctl(msr, VmxMsrBitmapAction::Set, VmxMsrBitmapAccess::Read);
            msr_type.remove(MsrType::READ);
        }

        if msr_type.contains(MsrType::WRITE)
            && !arch.msr_allowed(msr, MsrFilterType::KVM_MSR_FILTER_WRITE)
        {
            msr_bitmap.ctl(msr, VmxMsrBitmapAction::Set, VmxMsrBitmapAccess::Write);
            msr_type.remove(MsrType::WRITE);
        }

        if msr_type.contains(MsrType::READ) {
            msr_bitmap.ctl(msr, VmxMsrBitmapAction::Clear, VmxMsrBitmapAccess::Read);
        }

        if msr_type.contains(MsrType::WRITE) {
            msr_bitmap.ctl(msr, VmxMsrBitmapAction::Clear, VmxMsrBitmapAccess::Write);
        }
    }

    #[inline]
    pub fn segment_cache_clear(&mut self) {
        self.segment_cache.bitmask = 0;
    }

    pub fn clear_atomic_switch_msr(&mut self, msr: u32) {
        match msr {
            msr::IA32_EFER => {
                if vmx_info().has_load_ia32_efer() {
                    self.clear_stomic_switch_msr_special(
                        EntryControls::LOAD_IA32_EFER.bits().into(),
                        ExitControls::LOAD_IA32_EFER.bits().into(),
                    );
                    return;
                }
            }

            msr::MSR_PERF_GLOBAL_CTRL => {
                if vmx_info().has_load_perf_global_ctrl() {
                    self.clear_stomic_switch_msr_special(
                        EntryControls::LOAD_IA32_PERF_GLOBAL_CTRL.bits().into(),
                        ExitControls::LOAD_IA32_PERF_GLOBAL_CTRL.bits().into(),
                    );
                    return;
                }
            }
            _ => {}
        }

        let m = &mut self.msr_autoload;
        let i = m.guest.find_loadstore_msr_slot(msr);

        if let Some(i) = i {
            m.guest.nr -= 1;
            m.guest.val[i] = m.guest.val[m.guest.nr];
            VmxAsm::vmx_vmwrite(control::VMENTRY_MSR_LOAD_COUNT, m.guest.nr as u64);
        }

        let i = m.host.find_loadstore_msr_slot(msr);
        if let Some(i) = i {
            m.host.nr -= 1;
            m.host.val[i] = m.host.val[m.host.nr];
            VmxAsm::vmx_vmwrite(control::VMEXIT_MSR_LOAD_COUNT, m.host.nr as u64);
        }
    }

    fn clear_stomic_switch_msr_special(&self, entry: u64, exit: u64) {
        let mut guard = self.loaded_vmcs.lock();
        guard.controls_clearbit(ControlsType::VmEntry, entry);
        guard.controls_clearbit(ControlsType::VmExit, exit);
    }

    pub fn add_atomic_switch_msr(
        &mut self,
        msr: u32,
        guest_val: u64,
        host_val: u64,
        entry_only: bool,
    ) {
        match msr {
            msr::IA32_EFER => {
                if vmx_info().has_load_ia32_efer() {
                    self.add_atomic_switch_msr_special(
                        EntryControls::LOAD_IA32_EFER.bits() as u64,
                        ExitControls::LOAD_IA32_EFER.bits() as u64,
                        guest::IA32_EFER_FULL,
                        host::IA32_EFER_FULL,
                        guest_val,
                        host_val,
                    );
                    return;
                }
            }
            msr::MSR_PERF_GLOBAL_CTRL => {
                if vmx_info().has_load_perf_global_ctrl() {
                    self.add_atomic_switch_msr_special(
                        EntryControls::LOAD_IA32_PERF_GLOBAL_CTRL.bits().into(),
                        ExitControls::LOAD_IA32_PERF_GLOBAL_CTRL.bits().into(),
                        guest::IA32_PERF_GLOBAL_CTRL_FULL,
                        host::IA32_PERF_GLOBAL_CTRL_FULL,
                        guest_val,
                        host_val,
                    );
                    return;
                }
            }
            msr::MSR_PEBS_ENABLE => {
                unsafe { wrmsr(msr::MSR_PEBS_ENABLE, 0) };
            }

            _ => {}
        }

        let m = &mut self.msr_autoload;
        let i = m.guest.find_loadstore_msr_slot(msr);
        let j = if !entry_only {
            m.host.find_loadstore_msr_slot(msr)
        } else {
            Some(0)
        };

        if (i.is_none() && m.guest.nr == VmxMsrs::MAX_NR_LOADSTORE_MSRS)
            || (j.is_none() && m.host.nr == VmxMsrs::MAX_NR_LOADSTORE_MSRS)
        {
            warn!("Not enough msr switch entries. Can't add msr 0x{:x}", msr);
            return;
        }

        let i = if let Some(i) = i {
            i
        } else {
            m.guest.nr += 1;
            VmxAsm::vmx_vmwrite(control::VMENTRY_MSR_LOAD_COUNT, m.guest.nr as u64);
            m.guest.nr
        };

        m.guest.val[i].index = msr;
        m.guest.val[i].data = guest_val;

        if entry_only {
            return;
        }

        let j = if let Some(j) = j {
            j
        } else {
            m.host.nr += 1;
            VmxAsm::vmx_vmwrite(control::VMEXIT_MSR_LOAD_COUNT, m.host.nr as u64);
            m.host.nr
        };

        m.host.val[j].index = msr;
        m.host.val[j].data = host_val;
    }

    fn add_atomic_switch_msr_special(
        &self,
        entry: u64,
        exit: u64,
        guest_val_vmcs: u32,
        host_val_vmcs: u32,
        guest_val: u64,
        host_val: u64,
    ) {
        VmxAsm::vmx_vmwrite(guest_val_vmcs, guest_val);
        if host_val_vmcs != host::IA32_EFER_FULL {
            VmxAsm::vmx_vmwrite(host_val_vmcs, host_val);
        }

        let mut guard = self.loaded_vmcs.lock();
        guard.controls_setbit(ControlsType::VmEntry, entry);
        guard.controls_setbit(ControlsType::VmExit, exit);
    }

    pub fn vmx_vcpu_run_flags(&self) -> VmxRunFlag {
        let mut flags = VmxRunFlag::empty();

        if self.loaded_vmcs().launched {
            flags.insert(VmxRunFlag::VMRESUME);
        }

        // MSR_IA32_SPEC_CTRL
        if !self.loaded_vmcs().msr_write_intercepted(0x48) {
            flags.insert(VmxRunFlag::SAVE_SPEC_CTRL);
        }

        flags
    }
    pub fn get_exit_qual(&self) -> u64 {
        self.exit_qualification
    }
    pub fn vmread_exit_qual(&mut self) {
        self.exit_qualification = VmxAsm::vmx_vmread(ro::EXIT_QUALIFICATION);
    }
}

bitflags! {
    pub struct MsrType: u8 {
        const READ = 1;
        const WRITE = 2;
        const RW = 3;
    }

    //https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/include/asm/kvm_host.h#249
    pub struct PageFaultErr: u64 {
        const PFERR_PRESENT = 1 << 0;
        const PFERR_WRITE = 1 << 1;
        const PFERR_USER = 1 << 2;
        const PFERR_RSVD = 1 << 3;
        const PFERR_FETCH = 1 << 4;
        const PFERR_PK = 1 << 5;
        const PFERR_SGX = 1 << 15;
        const PFERR_GUEST_FINAL = 1 << 32;
        const PFERR_GUEST_PAGE = 1 << 33;
        const PFERR_IMPLICIT_ACCESS = 1 << 48;
    }

    pub struct VmxRunFlag: u8 {
        const VMRESUME = 1 << 0;
        const SAVE_SPEC_CTRL = 1 << 1;
    }
}

#[derive(Debug, PartialEq)]
#[allow(dead_code)]
pub enum VmxL1dFlushState {
    Auto,
    Never,
    Cond,
    Always,
    EptDisabled,
    NotRequired,
}

#[derive(Debug, PartialEq)]
pub struct VmxSegmentField {
    selector: u32,
    base: u32,
    limit: u32,
    ar_bytes: u32,
}
//fix
pub const KVM_VMX_SEGMENT_FIELDS: &[VmxSegmentField] = &[
    // ES
    VmxSegmentField {
        selector: guest::ES_SELECTOR,
        base: guest::ES_BASE,
        limit: guest::ES_LIMIT,
        ar_bytes: guest::ES_ACCESS_RIGHTS,
    },
    // CS
    VmxSegmentField {
        selector: guest::CS_SELECTOR,
        base: guest::CS_BASE,
        limit: guest::CS_LIMIT,
        ar_bytes: guest::CS_ACCESS_RIGHTS,
    },
    // SS
    VmxSegmentField {
        selector: guest::SS_SELECTOR,
        base: guest::SS_BASE,
        limit: guest::SS_LIMIT,
        ar_bytes: guest::SS_ACCESS_RIGHTS,
    },
    // DS
    VmxSegmentField {
        selector: guest::DS_SELECTOR,
        base: guest::DS_BASE,
        limit: guest::DS_LIMIT,
        ar_bytes: guest::DS_ACCESS_RIGHTS,
    },
    // FS
    VmxSegmentField {
        selector: guest::FS_SELECTOR,
        base: guest::FS_BASE,
        limit: guest::FS_LIMIT,
        ar_bytes: guest::FS_ACCESS_RIGHTS,
    },
    // GS
    VmxSegmentField {
        selector: guest::GS_SELECTOR,
        base: guest::GS_BASE,
        limit: guest::GS_LIMIT,
        ar_bytes: guest::GS_ACCESS_RIGHTS,
    },
    // TR
    VmxSegmentField {
        selector: guest::TR_SELECTOR,
        base: guest::TR_BASE,
        limit: guest::TR_LIMIT,
        ar_bytes: guest::TR_ACCESS_RIGHTS,
    },
    // LDTR
    VmxSegmentField {
        selector: guest::LDTR_SELECTOR,
        base: guest::LDTR_BASE,
        limit: guest::LDTR_LIMIT,
        ar_bytes: guest::LDTR_ACCESS_RIGHTS,
    },
];

pub static L1TF_VMX_MITIGATION: RwLock<VmxL1dFlushState> = RwLock::new(VmxL1dFlushState::Auto);

pub fn vmx_init() -> Result<(), SystemError> {
    let cpuid = CpuId::new();
    let cpu_feat = cpuid.get_feature_info().ok_or(SystemError::ENOSYS)?;
    if !cpu_feat.has_vmx() {
        return Err(SystemError::ENOSYS);
    }

    init_kvm_arch();

    x86_kvm_manager_mut().vendor_init(&VmxKvmInitFunc)?;

    vmx_info().setup_l1d_flush();

    kvm_init()?;
    Ok(())
}

#[no_mangle]
unsafe extern "C" fn vmx_update_host_rsp(vcpu_vmx: &VmxVCpuPriv, host_rsp: usize) {
    warn!("vmx_update_host_rsp");
    let mut guard = vcpu_vmx.loaded_vmcs.lock();
    if unlikely(host_rsp != guard.host_state.rsp) {
        guard.host_state.rsp = host_rsp;
        VmxAsm::vmx_vmwrite(host::RSP, host_rsp as u64);
    }
}

#[no_mangle]
unsafe extern "C" fn vmx_spec_ctrl_restore_host(_vcpu_vmx: &VmxVCpuPriv, _flags: u32) {
    // TODO
    warn!("vmx_spec_ctrl_restore_host todo!");
}
