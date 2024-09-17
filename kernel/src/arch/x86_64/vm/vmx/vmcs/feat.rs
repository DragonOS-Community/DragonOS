use system_error::SystemError;
use x86::{
    msr::{
        IA32_VMX_ENTRY_CTLS, IA32_VMX_EXIT_CTLS, IA32_VMX_PINBASED_CTLS, IA32_VMX_PROCBASED_CTLS,
        IA32_VMX_PROCBASED_CTLS2,
    },
    vmx::vmcs::control::{
        EntryControls, ExitControls, PinbasedControls, PrimaryControls, SecondaryControls,
    },
};

use crate::arch::vm::vmx::Vmx;

pub struct VmxFeat;
#[allow(dead_code)]
impl VmxFeat {
    pub const KVM_REQUIRED_VMX_CPU_BASED_VM_EXEC_CONTROL: u32 = PrimaryControls::HLT_EXITING.bits()
        | PrimaryControls::CR3_LOAD_EXITING.bits()
        | PrimaryControls::CR3_STORE_EXITING.bits()
        | PrimaryControls::UNCOND_IO_EXITING.bits()
        | PrimaryControls::MOV_DR_EXITING.bits()
        | PrimaryControls::USE_TSC_OFFSETTING.bits()
        | PrimaryControls::MWAIT_EXITING.bits()
        | PrimaryControls::MONITOR_EXITING.bits()
        | PrimaryControls::INVLPG_EXITING.bits()
        | PrimaryControls::RDPMC_EXITING.bits()
        | PrimaryControls::INTERRUPT_WINDOW_EXITING.bits()
        | PrimaryControls::CR8_LOAD_EXITING.bits()
        | PrimaryControls::CR8_STORE_EXITING.bits();

    pub const KVM_OPTIONAL_VMX_CPU_BASED_VM_EXEC_CONTROL: u32 = PrimaryControls::RDTSC_EXITING
        .bits()
        | PrimaryControls::USE_TPR_SHADOW.bits()
        | PrimaryControls::USE_IO_BITMAPS.bits()
        | PrimaryControls::MONITOR_TRAP_FLAG.bits()
        | PrimaryControls::USE_MSR_BITMAPS.bits()
        | PrimaryControls::NMI_WINDOW_EXITING.bits()
        | PrimaryControls::PAUSE_EXITING.bits()
        | PrimaryControls::SECONDARY_CONTROLS.bits();

    pub const KVM_REQUIRED_VMX_SECONDARY_VM_EXEC_CONTROL: u32 = 0;

    pub const KVM_OPTIONAL_VMX_SECONDARY_VM_EXEC_CONTROL: u32 = SecondaryControls::VIRTUALIZE_APIC
        .bits()
        | SecondaryControls::VIRTUALIZE_X2APIC.bits()
        | SecondaryControls::WBINVD_EXITING.bits()
        | SecondaryControls::ENABLE_VPID.bits()
        | SecondaryControls::ENABLE_EPT.bits()
        | SecondaryControls::UNRESTRICTED_GUEST.bits()
        | SecondaryControls::PAUSE_LOOP_EXITING.bits()
        | SecondaryControls::DTABLE_EXITING.bits()
        | SecondaryControls::ENABLE_RDTSCP.bits()
        | SecondaryControls::ENABLE_INVPCID.bits()
        | SecondaryControls::VIRTUALIZE_APIC_REGISTER.bits()
        | SecondaryControls::VIRTUAL_INTERRUPT_DELIVERY.bits()
        | SecondaryControls::VMCS_SHADOWING.bits()
        | SecondaryControls::ENABLE_XSAVES_XRSTORS.bits()
        | SecondaryControls::RDSEED_EXITING.bits()
        | SecondaryControls::RDRAND_EXITING.bits()
        | SecondaryControls::ENABLE_PML.bits()
        | SecondaryControls::USE_TSC_SCALING.bits()
        | SecondaryControls::ENABLE_USER_WAIT_PAUSE.bits()
        | SecondaryControls::INTEL_PT_GUEST_PHYSICAL.bits()
        | SecondaryControls::CONCEAL_VMX_FROM_PT.bits()
        | SecondaryControls::ENABLE_VM_FUNCTIONS.bits()
        | SecondaryControls::ENCLS_EXITING.bits();
    // | SecondaryControls::BUS_LOCK_DETECTION.bits()
    // | SecondaryControls::NOTIFY_VM_EXITING.bits()

    pub const KVM_REQUIRED_VMX_VM_EXIT_CONTROLS: u32 = ExitControls::SAVE_DEBUG_CONTROLS.bits()
        | ExitControls::ACK_INTERRUPT_ON_EXIT.bits()
        | ExitControls::HOST_ADDRESS_SPACE_SIZE.bits();

    pub const KVM_OPTIONAL_VMX_VM_EXIT_CONTROLS: u32 = ExitControls::LOAD_IA32_PERF_GLOBAL_CTRL
        .bits()
        | ExitControls::SAVE_IA32_PAT.bits()
        | ExitControls::LOAD_IA32_PAT.bits()
        | ExitControls::SAVE_IA32_EFER.bits()
        | ExitControls::SAVE_VMX_PREEMPTION_TIMER.bits()
        | ExitControls::LOAD_IA32_EFER.bits()
        | ExitControls::CLEAR_IA32_BNDCFGS.bits()
        | ExitControls::CONCEAL_VMX_FROM_PT.bits()
        | ExitControls::CLEAR_IA32_RTIT_CTL.bits();

    pub const KVM_REQUIRED_VMX_PIN_BASED_VM_EXEC_CONTROL: u32 =
        PinbasedControls::EXTERNAL_INTERRUPT_EXITING.bits() | PinbasedControls::NMI_EXITING.bits();

    pub const KVM_OPTIONAL_VMX_PIN_BASED_VM_EXEC_CONTROL: u32 =
        PinbasedControls::VIRTUAL_NMIS.bits() | PinbasedControls::POSTED_INTERRUPTS.bits();

    pub const KVM_REQUIRED_VMX_VM_ENTRY_CONTROLS: u32 =
        EntryControls::LOAD_DEBUG_CONTROLS.bits() | EntryControls::IA32E_MODE_GUEST.bits();

    pub const KVM_OPTIONAL_VMX_VM_ENTRY_CONTROLS: u32 = EntryControls::LOAD_IA32_PERF_GLOBAL_CTRL
        .bits()
        | EntryControls::LOAD_IA32_PAT.bits()
        | EntryControls::LOAD_IA32_EFER.bits()
        | EntryControls::LOAD_IA32_BNDCFGS.bits()
        | EntryControls::CONCEAL_VMX_FROM_PT.bits()
        | EntryControls::LOAD_IA32_RTIT_CTL.bits();

    /* VMX_BASIC bits and bitmasks */
    pub const VMX_BASIC_VMCS_SIZE_SHIFT: u64 = 32;
    pub const VMX_BASIC_TRUE_CTLS: u64 = 1 << 55;
    pub const VMX_BASIC_64: u64 = 0x0001000000000000;
    pub const VMX_BASIC_MEM_TYPE_SHIFT: u64 = 50;
    pub const VMX_BASIC_MEM_TYPE_MASK: u64 = 0x003c000000000000;
    pub const VMX_BASIC_MEM_TYPE_WB: u64 = 6;
    pub const VMX_BASIC_INOUT: u64 = 0x0040000000000000;

    pub fn adjust_primary_controls() -> Result<PrimaryControls, SystemError> {
        Ok(unsafe {
            PrimaryControls::from_bits_unchecked(Vmx::adjust_vmx_controls(
                Self::KVM_REQUIRED_VMX_CPU_BASED_VM_EXEC_CONTROL,
                Self::KVM_OPTIONAL_VMX_CPU_BASED_VM_EXEC_CONTROL,
                IA32_VMX_PROCBASED_CTLS,
            )?)
        })
    }

    pub fn adjust_secondary_controls() -> Result<SecondaryControls, SystemError> {
        Ok(unsafe {
            SecondaryControls::from_bits_unchecked(Vmx::adjust_vmx_controls(
                Self::KVM_REQUIRED_VMX_SECONDARY_VM_EXEC_CONTROL,
                Self::KVM_OPTIONAL_VMX_SECONDARY_VM_EXEC_CONTROL,
                IA32_VMX_PROCBASED_CTLS2,
            )?)
        })
    }

    pub fn adjust_exit_controls() -> Result<ExitControls, SystemError> {
        Ok(unsafe {
            ExitControls::from_bits_unchecked(Vmx::adjust_vmx_controls(
                Self::KVM_REQUIRED_VMX_VM_EXIT_CONTROLS,
                Self::KVM_OPTIONAL_VMX_VM_EXIT_CONTROLS,
                IA32_VMX_EXIT_CTLS,
            )?)
        })
    }

    pub fn adjust_entry_controls() -> Result<EntryControls, SystemError> {
        Ok(unsafe {
            EntryControls::from_bits_unchecked(Vmx::adjust_vmx_controls(
                Self::KVM_REQUIRED_VMX_VM_ENTRY_CONTROLS,
                Self::KVM_OPTIONAL_VMX_VM_ENTRY_CONTROLS,
                IA32_VMX_ENTRY_CTLS,
            )?)
        })
    }

    pub fn adjust_pin_based_controls() -> Result<PinbasedControls, SystemError> {
        Ok(unsafe {
            PinbasedControls::from_bits_unchecked(Vmx::adjust_vmx_controls(
                Self::KVM_REQUIRED_VMX_PIN_BASED_VM_EXEC_CONTROL,
                Self::KVM_OPTIONAL_VMX_PIN_BASED_VM_EXEC_CONTROL,
                IA32_VMX_PINBASED_CTLS,
            )?)
        })
    }
}
