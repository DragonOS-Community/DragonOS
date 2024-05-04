use core::{
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{
    arch::{
        vm::{
            asm::KvmX86Asm,
            kvm_host::{vcpu::VirCpuRequest, X86KvmArch},
            vmx::vmcs::vmx_area,
        },
        CurrentIrqArch, VirtCpuArch,
    },
    exception::InterruptArch,
    kdebug,
    libs::{once::Once, spinlock::SpinLock},
    mm::{
        percpu::{PerCpu, PerCpuVar},
        virt_2_phys, PhysAddr,
    },
    smp::{core::smp_get_processor_id, cpu::ProcessorId},
    virt::vm::{kvm_dev::kvm_init, kvm_host::vcpu::VirtCpu},
};
use alloc::{alloc::Global, boxed::Box, collections::LinkedList, sync::Arc, vec::Vec};
use bitmap::{traits::BitMapOps, AllocBitmap, StaticBitmap};
use raw_cpuid::CpuId;
use system_error::SystemError;
use x86::{
    controlregs::Xcr0,
    msr::{
        rdmsr, IA32_CSTAR, IA32_EFER, IA32_FMASK, IA32_FS_BASE, IA32_GS_BASE, IA32_KERNEL_GSBASE,
        IA32_LSTAR, IA32_SMBASE, IA32_STAR, IA32_SYSENTER_CS, IA32_SYSENTER_EIP, IA32_SYSENTER_ESP,
        IA32_TIME_STAMP_COUNTER, IA32_TSC_AUX, IA32_VMX_BASIC, IA32_VMX_CR0_FIXED0,
        IA32_VMX_CR0_FIXED1, IA32_VMX_CR4_FIXED0, IA32_VMX_CR4_FIXED1, IA32_VMX_ENTRY_CTLS,
        IA32_VMX_EPT_VPID_CAP, IA32_VMX_EXIT_CTLS, IA32_VMX_MISC, IA32_VMX_PINBASED_CTLS,
        IA32_VMX_PROCBASED_CTLS, IA32_VMX_PROCBASED_CTLS2, IA32_VMX_TRUE_ENTRY_CTLS,
        IA32_VMX_TRUE_EXIT_CTLS, IA32_VMX_TRUE_PINBASED_CTLS, IA32_VMX_TRUE_PROCBASED_CTLS,
        IA32_VMX_VMCS_ENUM, IA32_VMX_VMFUNC, MSR_CORE_C1_RESIDENCY, MSR_CORE_C3_RESIDENCY,
        MSR_CORE_C6_RESIDENCY, MSR_CORE_C7_RESIDENCY, MSR_IA32_ADDR0_START, MSR_IA32_ADDR3_END,
        MSR_IA32_CR3_MATCH, MSR_IA32_RTIT_OUTPUT_BASE, MSR_IA32_RTIT_OUTPUT_MASK_PTRS,
        MSR_IA32_RTIT_STATUS, MSR_IA32_TSX_CTRL, MSR_LASTBRANCH_TOS, MSR_LBR_SELECT,
    },
    vmx::vmcs::{
        control::{
            EntryControls, ExitControls, PrimaryControls, SecondaryControls, PINBASED_EXEC_CONTROLS,
        },
        host,
    },
};
use x86_64::instructions::tables::sidt;

use crate::{
    arch::{
        vm::{vmx::vmcs::feat::VmxFeat, x86_kvm_manager_mut, McgCap},
        KvmArch,
    },
    kerror, kwarn,
    libs::{lazy_init::Lazy, rwlock::RwLock},
    virt::vm::kvm_host::Vm,
};

use self::{
    capabilities::{NestedVmxMsrs, ProcessorTraceMode, VmcsConfig, VmxCapability},
    vmcs::{
        current_loaded_vmcs_list_mut, current_vmcs, current_vmcs_mut, LockedLoadedVmcs,
        VMControlStructure, VmxMsrBitmapAccess, VmxMsrBitmapAction, PERCPU_LOADED_VMCS_LIST,
        PERCPU_VMCS, VMXAREA,
    },
};

use super::{
    asm::VmxAsm,
    init_kvm_arch,
    kvm_host::{KvmFunc, KvmInitFunc, MsrFilterType},
    x86_kvm_manager, KvmArchManager, CPU_BASED_ALWAYSON_WITHOUT_TRUE_MSR,
    PIN_BASED_ALWAYSON_WITHOUT_TRUE_MSR, VM_ENTRY_ALWAYSON_WITHOUT_TRUE_MSR,
    VM_EXIT_ALWAYSON_WITHOUT_TRUE_MSR,
};

pub mod capabilities;
pub mod vmcs;
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
            kerror!("[KVM] NX (Execute Disable) not supported");
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

        vmx_init.vpid_bitmap.lock().set_all(false);

        if vmx_init.enable_ept {
            // TODO: mmu_set_ept_masks
            kwarn!("mmu_set_ept_masks TODO!");
        }

        kwarn!("vmx_setup_me_spte_mask TODO!");

        kwarn!("kvm_configure_mmu TODO!");

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
            kdebug!("revision_id {id}");
            vcpu.request(VirCpuRequest::KVM_REQ_TLB_FLUSH);

            VmxAsm::vmx_vmwrite(
                host::TR_BASE,
                KvmX86Asm::get_segment_base(
                    pseudo_descriptpr.base,
                    pseudo_descriptpr.limit,
                    unsafe { x86::task::tr().bits() },
                ),
            );

            VmxAsm::vmx_vmwrite(host::GDTR_BASE, pseudo_descriptpr.base as usize as u64);

            VmxAsm::vmx_vmwrite(host::IA32_SYSENTER_ESP, unsafe { rdmsr(IA32_SYSENTER_ESP) });
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

            let _ = current_loaded_vmcs_list_mut().extract_if(|x| Arc::ptr_eq(&x, loaded_vmcs));

            guard.cpu = ProcessorId::INVALID;
            guard.launched = false;
        } else {
            // 交由对应cpu处理
            todo!()
        }
    }
}

impl KvmFunc for VmxKvmFunc {
    fn name(&self) -> &'static str {
        "VMX"
    }

    fn hardware_enable(&self) -> Result<(), SystemError> {
        let vmcs = vmx_area().get().as_ref();

        kdebug!("vmcs idx {}", vmcs.abort);

        let phys_addr = virt_2_phys(vmcs as *const _ as usize);

        // TODO: intel_pt_handle_vmx(1);

        VmxAsm::kvm_cpu_vmxon(PhysAddr::new(phys_addr))?;

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

    fn cache_reg(&self, vcpu: &VirtCpuArch, reg: super::kvm_host::KvmReg) {
        todo!()
    }

    fn apicv_pre_state_restore(&self, vcpu: &mut VirtCpu) {
        // https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/vmx/vmx.c#6924
        // TODO: pi
        // todo!()
    }

    fn set_msr(&self, vcpu: &mut VirtCpuArch, msr: super::asm::MsrData) {
        todo!()
    }

    fn vcpu_reset(&self, vcpu: &mut VirtCpu, init_event: bool) {
        todo!()
    }

    fn set_rflags(&self, vcpu: &mut VirtCpu, rflags: x86::bits64::rflags::RFlags) {
        todo!()
    }

    fn set_cr0(&self, vcpu: &mut VirtCpu, cr0: x86::controlregs::Cr0) {
        todo!()
    }

    fn set_cr4(&self, vcpu: &mut VirtCpu, cr4: x86::controlregs::Cr4) {
        todo!()
    }

    fn set_efer(&self, vcpu: &mut VirtCpu, efer: x86_64::registers::control::EferFlags) {
        todo!()
    }

    fn update_exception_bitmap(&self, vcpu: &mut VirtCpu) {
        todo!()
    }

    fn has_emulated_msr(&self, msr: u32) -> bool {
        match msr {
            IA32_SMBASE => {
                return vmx_info().enable_unrestricted_guest
                    || vmx_info().emulate_invalid_guest_state;
            }

            IA32_VMX_BASIC..=IA32_VMX_VMFUNC => {
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

    fn get_msr_feature(&self, msr: &mut super::asm::KvmMsrEntry) -> bool {
        match msr.index {
            IA32_VMX_BASIC..=IA32_VMX_VMFUNC => {
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

    fn get_rflags(&self, vcpu: &VirtCpu) -> x86::bits64::rflags::RFlags {
        todo!()
    }
}

static mut VMX: Option<Vmx> = None;

#[inline]
pub fn vmx_info() -> &'static Vmx {
    unsafe { VMX.as_ref().unwrap() }
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
    pub vpid_bitmap: SpinLock<StaticBitmap<{ 1 << 16 }>>,
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

    pub nested: bool,

    pub ple_gap: u32,
    pub ple_window: u32,
    pub ple_window_grow: u32,
    pub ple_window_max: u32,
    pub ple_window_shrink: u32,

    pub pt_mode: ProcessorTraceMode,
}

impl Default for Vmx {
    fn default() -> Self {
        Self {
            host_idt_base: Default::default(),
            vmcs_config: Default::default(),
            vmx_cap: Default::default(),
            vpid_bitmap: SpinLock::new(StaticBitmap::new()),
            enable_vpid: true,
            enable_ept: true,
            enable_ept_ad: true,
            enable_unrestricted_guest: true,
            enable_flexpriority: true,
            enable_vnmi: true,
            enable_sgx: true,
            ple_gap: 128,
            ple_window: 4096,
            ple_window_grow: 2,
            ple_window_max: u32::MAX,
            ple_window_shrink: 0,
            enable_apicv: true,
            enable_ipiv: true,
            enable_pml: true,
            enable_preemption_timer: true,
            pt_mode: ProcessorTraceMode::System,
            emulate_invalid_guest_state: true,

            // 目前先不管嵌套虚拟化，后续再实现
            nested: true,
        }
    }
}

impl Vmx {
    /*
     * Internal error codes that are used to indicate that MSR emulation encountered
     * an error that should result in #GP in the guest, unless userspace
     * handles it.
     */
    pub const KVM_MSR_RET_INVALID: u32 = 2; /* in-kernel MSR emulation #GP condition */
    pub const KVM_MSR_RET_FILTERED: u32 = 3; /* #GP due to userspace MSR filter */

    pub const MAX_POSSIBLE_PASSTHROUGH_MSRS: usize = 16;

    pub const VMX_POSSIBLE_PASSTHROUGH_MSRS: [u32; Self::MAX_POSSIBLE_PASSTHROUGH_MSRS] = [
        0x48,  // MSR_IA32_SPEC_CTRL
        0x49,  // MSR_IA32_PRED_CMD
        0x10b, // MSR_IA32_FLUSH_CMD
        IA32_TIME_STAMP_COUNTER,
        IA32_FS_BASE,
        IA32_GS_BASE,
        IA32_KERNEL_GSBASE,
        0x1c4, // MSR_IA32_XFD
        0x1c5, // MSR_IA32_XFD_ERR
        IA32_SYSENTER_CS,
        IA32_SYSENTER_ESP,
        IA32_SYSENTER_EIP,
        MSR_CORE_C1_RESIDENCY,
        MSR_CORE_C3_RESIDENCY,
        MSR_CORE_C6_RESIDENCY,
        MSR_CORE_C7_RESIDENCY,
    ];

    /// ### 查看CPU是否支持虚拟化
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
        const VMX_URET_MSRS_LIST: &'static [u32] = &[
            IA32_FMASK,
            IA32_LSTAR,
            IA32_CSTAR,
            IA32_EFER,
            IA32_TSC_AUX,
            IA32_STAR,
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
        const VMCS_ENTRY_EXIT_PAIRS: &'static [VmcsEntryExitPair] = &[
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

        let cap = unsafe { rdmsr(IA32_VMX_EPT_VPID_CAP) };
        vmx_cap.set_val_from_msr_val(cap);

        // 不支持ept但是读取到了值
        if !cpu_based_2nd_exec_control.contains(SecondaryControls::ENABLE_EPT)
            && !vmx_cap.ept.is_empty()
        {
            kwarn!("EPT CAP should not exist if not support. 1-setting enable EPT VM-execution control");
            return Err(SystemError::EIO);
        }

        if !cpu_based_2nd_exec_control.contains(SecondaryControls::ENABLE_VPID)
            && !vmx_cap.vpid.is_empty()
        {
            kwarn!("VPID CAP should not exist if not support. 1-setting enable VPID VM-execution control");
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
            if !(vmentry_control.contains(n_ctrl)) == !(vmxexit_control.contains(x_ctrl)) {
                continue;
            }

            kwarn!(
                "Inconsistent VM-Entry/VM-Exit pair, entry = {:?}, exit = {:?}",
                vmentry_control & n_ctrl,
                vmxexit_control & x_ctrl,
            );

            return Err(SystemError::EIO);
        }

        let basic = unsafe { rdmsr(IA32_VMX_BASIC) };
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

        let misc_msr = unsafe { rdmsr(IA32_VMX_MISC) };

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
            MSR_IA32_RTIT_STATUS
            | MSR_IA32_RTIT_OUTPUT_BASE
            | MSR_IA32_RTIT_OUTPUT_MASK_PTRS
            | MSR_IA32_CR3_MATCH
            | MSR_LBR_SELECT
            | MSR_LASTBRANCH_TOS => {
                return true;
            }
            MSR_IA32_ADDR0_START..MSR_IA32_ADDR3_END => {
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
        *L1TF_VMX_MITIGATION.write() = VmxL1dFlushState::FlushNotRequired;
    }
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

#[derive(Debug, Default, Clone, Copy)]
pub struct VmxUretMsr {
    load_into_hardware: bool,
    data: u64,
    mask: u64,
}

#[derive(Debug)]
pub struct VmxVCpuPriv {
    vpid: Option<usize>,
    vmcs01: Arc<LockedLoadedVmcs>,
    loaded_vmcs: Arc<LockedLoadedVmcs>,
    guest_uret_msrs: [VmxUretMsr; KvmArchManager::KVM_MAX_NR_USER_RETURN_MSRS],

    shadow_msr_intercept_read: AllocBitmap,
    shadow_msr_intercept_write: AllocBitmap,
}

impl VmxVCpuPriv {
    /// 参考：https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/vmx/vmx.c#7452
    pub fn init(vcpu: &mut VirtCpu, vm: &Vm) {
        let vmcs = LockedLoadedVmcs::new();
        let mut vmx = Self {
            vpid: None,
            vmcs01: vmcs.clone(),
            loaded_vmcs: vmcs,
            guest_uret_msrs: [VmxUretMsr::default(); KvmArchManager::KVM_MAX_NR_USER_RETURN_MSRS],
            shadow_msr_intercept_read: AllocBitmap::new(16),
            shadow_msr_intercept_write: AllocBitmap::new(16),
        };

        vmx.vpid = vmx_info().alloc_vpid();

        for i in 0..x86_kvm_manager().kvm_uret_msrs_list.len() {
            vmx.guest_uret_msrs[i].mask = u64::MAX;
        }

        if CpuId::new().get_extended_feature_info().unwrap().has_rtm() {
            let tsx_ctrl = vmx.find_uret_msr_mut(MSR_IA32_TSX_CTRL);
            if let Some(tsx_ctrl) = tsx_ctrl {
                // Disable TSX enumeration
                tsx_ctrl.mask = !(1 << 1);
            }
        }

        vmx.shadow_msr_intercept_read.set_all(true);
        vmx.shadow_msr_intercept_write.set_all(true);

        let arch = &vm.arch;

        vmx.disable_intercept_for_msr(arch, IA32_TIME_STAMP_COUNTER, MsrType::READ);
        vmx.disable_intercept_for_msr(arch, IA32_FS_BASE, MsrType::RW);
        vmx.disable_intercept_for_msr(arch, IA32_GS_BASE, MsrType::RW);
        vmx.disable_intercept_for_msr(arch, IA32_KERNEL_GSBASE, MsrType::RW);

        vmx.disable_intercept_for_msr(arch, IA32_SYSENTER_CS, MsrType::RW);
        vmx.disable_intercept_for_msr(arch, IA32_SYSENTER_ESP, MsrType::RW);
        vmx.disable_intercept_for_msr(arch, IA32_SYSENTER_EIP, MsrType::RW);

        if arch.pause_in_guest {
            vmx.disable_intercept_for_msr(arch, MSR_CORE_C1_RESIDENCY, MsrType::READ);
            vmx.disable_intercept_for_msr(arch, MSR_CORE_C3_RESIDENCY, MsrType::READ);
            vmx.disable_intercept_for_msr(arch, MSR_CORE_C6_RESIDENCY, MsrType::READ);
            vmx.disable_intercept_for_msr(arch, MSR_CORE_C7_RESIDENCY, MsrType::READ);
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

    pub fn find_uret_msr(&self, msr: u32) -> Option<&VmxUretMsr> {
        let idx = x86_kvm_manager().find_user_return_msr_idx(msr);
        if let Some(index) = idx {
            return Some(&self.guest_uret_msrs[index]);
        } else {
            return None;
        }
    }

    pub fn find_uret_msr_mut(&mut self, msr: u32) -> Option<&mut VmxUretMsr> {
        let idx = x86_kvm_manager().find_user_return_msr_idx(msr);
        if let Some(index) = idx {
            return Some(&mut self.guest_uret_msrs[index]);
        } else {
            return None;
        }
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
}

bitflags! {
    pub struct MsrType: u8 {
        const READ = 1;
        const WRITE = 2;
        const RW = 3;
    }
}

#[derive(Debug, PartialEq)]
pub enum VmxL1dFlushState {
    FlushAuto,
    FlushNever,
    FlushCond,
    FlushAlways,
    FlushEptDisabled,
    FlushNotRequired,
}

pub static L1TF_VMX_MITIGATION: RwLock<VmxL1dFlushState> = RwLock::new(VmxL1dFlushState::FlushAuto);

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
