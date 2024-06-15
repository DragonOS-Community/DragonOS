use bitfield_struct::bitfield;
use system_error::SystemError;

use crate::virt::vm::kvm_host::vcpu::VirtCpu;

#[bitfield(u32)]
pub struct VmxExitReason {
    pub basic: u16,
    pub reserved16: bool,
    pub reserved17: bool,
    pub reserved18: bool,
    pub reserved19: bool,
    pub reserved20: bool,
    pub reserved21: bool,
    pub reserved22: bool,
    pub reserved23: bool,
    pub reserved24: bool,
    pub reserved25: bool,
    pub bus_lock_detected: bool,
    pub enclave_mode: bool,
    pub smi_pending_mtf: bool,
    pub smi_from_vmx_root: bool,
    pub reserved30: bool,
    pub failed_vmentry: bool,
}

#[derive(FromPrimitive, PartialEq)]
#[allow(non_camel_case_types)]
pub enum VmxExitReasonBasic {
    EXCEPTION_OR_NMI = 0,
    EXTERNAL_INTERRUPT = 1,
    TRIPLE_FAULT = 2,
    INIT_SIGNAL = 3,
    SIPI = 4,
    IO_SMI = 5,
    OTHER_SMI = 6,
    INTERRUPT_WINDOW = 7,
    NMI_WINDOW = 8,
    TASK_SWITCH = 9,
    CPUID = 10,
    GETSEC = 11,
    HLT = 12,
    INVD = 13,
    INVLPG = 14,
    RDPMC = 15,
    RDTSC = 16,
    RSM = 17,
    VMCALL = 18,
    VMCLEAR = 19,
    VMLAUNCH = 20,
    VMPTRLD = 21,
    VMPTRST = 22,
    VMREAD = 23,
    VMRESUME = 24,
    VMWRITE = 25,
    VMXOFF = 26,
    VMXON = 27,
    CR_ACCESS = 28,
    DR_ACCESS = 29,
    IO_INSTRUCTION = 30,
    RDMSR = 31,
    WRMSR = 32,
    VM_ENTRY_FAILURE_INVALID_GUEST_STATE = 33,
    VM_ENTRY_FAILURE_MSR_LOADING = 34,
    MWAIT = 36,
    MONITOR_TRAP_FLAG = 37,
    MONITOR = 39,
    PAUSE = 40,
    VM_ENTRY_FAILURE_MACHINE_CHECK_EVENT = 41,
    TPR_BELOW_THRESHOLD = 43,
    APIC_ACCESS = 44,
    VIRTUALIZED_EOI = 45,
    ACCESS_GDTR_OR_IDTR = 46,
    ACCESS_LDTR_OR_TR = 47,
    EPT_VIOLATION = 48,
    EPT_MISCONFIG = 49,
    INVEPT = 50,
    RDTSCP = 51,
    VMX_PREEMPTION_TIMER_EXPIRED = 52,
    INVVPID = 53,
    WBINVD = 54,
    XSETBV = 55,
    APIC_WRITE = 56,
    RDRAND = 57,
    INVPCID = 58,
    VMFUNC = 59,
    ENCLS = 60,
    RDSEED = 61,
    PML_FULL = 62,
    XSAVES = 63,
    XRSTORS = 64,

    UMWAIT = 67,
    TPAUSE = 68,
    BUS_LOCK = 74,
    NOTIFY = 75,

    UNKNOWN,
}

impl From<u16> for VmxExitReasonBasic {
    fn from(num: u16) -> Self {
        match num {
            0 => VmxExitReasonBasic::EXCEPTION_OR_NMI,
            1 => VmxExitReasonBasic::EXTERNAL_INTERRUPT,
            2 => VmxExitReasonBasic::TRIPLE_FAULT,
            3 => VmxExitReasonBasic::INIT_SIGNAL,
            4 => VmxExitReasonBasic::SIPI,
            5 => VmxExitReasonBasic::IO_SMI,
            6 => VmxExitReasonBasic::OTHER_SMI,
            7 => VmxExitReasonBasic::INTERRUPT_WINDOW,
            8 => VmxExitReasonBasic::NMI_WINDOW,
            9 => VmxExitReasonBasic::TASK_SWITCH,
            10 => VmxExitReasonBasic::CPUID,
            11 => VmxExitReasonBasic::GETSEC,
            12 => VmxExitReasonBasic::HLT,
            13 => VmxExitReasonBasic::INVD,
            14 => VmxExitReasonBasic::INVLPG,
            15 => VmxExitReasonBasic::RDPMC,
            16 => VmxExitReasonBasic::RDTSC,
            17 => VmxExitReasonBasic::RSM,
            18 => VmxExitReasonBasic::VMCALL,
            19 => VmxExitReasonBasic::VMCLEAR,
            20 => VmxExitReasonBasic::VMLAUNCH,
            21 => VmxExitReasonBasic::VMPTRLD,
            22 => VmxExitReasonBasic::VMPTRST,
            23 => VmxExitReasonBasic::VMREAD,
            24 => VmxExitReasonBasic::VMRESUME,
            25 => VmxExitReasonBasic::VMWRITE,
            26 => VmxExitReasonBasic::VMXOFF,
            27 => VmxExitReasonBasic::VMXON,
            28 => VmxExitReasonBasic::CR_ACCESS,
            29 => VmxExitReasonBasic::DR_ACCESS,
            30 => VmxExitReasonBasic::IO_INSTRUCTION,
            31 => VmxExitReasonBasic::RDMSR,
            32 => VmxExitReasonBasic::WRMSR,
            33 => VmxExitReasonBasic::VM_ENTRY_FAILURE_INVALID_GUEST_STATE,
            34 => VmxExitReasonBasic::VM_ENTRY_FAILURE_MSR_LOADING,
            36 => VmxExitReasonBasic::MWAIT,
            37 => VmxExitReasonBasic::MONITOR_TRAP_FLAG,
            39 => VmxExitReasonBasic::MONITOR,
            40 => VmxExitReasonBasic::PAUSE,
            41 => VmxExitReasonBasic::VM_ENTRY_FAILURE_MACHINE_CHECK_EVENT,
            43 => VmxExitReasonBasic::TPR_BELOW_THRESHOLD,
            44 => VmxExitReasonBasic::APIC_ACCESS,
            45 => VmxExitReasonBasic::VIRTUALIZED_EOI,
            46 => VmxExitReasonBasic::ACCESS_GDTR_OR_IDTR,
            47 => VmxExitReasonBasic::ACCESS_LDTR_OR_TR,
            48 => VmxExitReasonBasic::EPT_VIOLATION,
            49 => VmxExitReasonBasic::EPT_MISCONFIG,
            50 => VmxExitReasonBasic::INVEPT,
            51 => VmxExitReasonBasic::RDTSCP,
            52 => VmxExitReasonBasic::VMX_PREEMPTION_TIMER_EXPIRED,
            53 => VmxExitReasonBasic::INVVPID,
            54 => VmxExitReasonBasic::WBINVD,
            55 => VmxExitReasonBasic::XSETBV,
            56 => VmxExitReasonBasic::APIC_WRITE,
            57 => VmxExitReasonBasic::RDRAND,
            58 => VmxExitReasonBasic::INVPCID,
            59 => VmxExitReasonBasic::VMFUNC,
            60 => VmxExitReasonBasic::ENCLS,
            61 => VmxExitReasonBasic::RDSEED,
            62 => VmxExitReasonBasic::PML_FULL,
            63 => VmxExitReasonBasic::XSAVES,
            64 => VmxExitReasonBasic::XRSTORS,

            67 => VmxExitReasonBasic::UMWAIT,
            68 => VmxExitReasonBasic::TPAUSE,
            74 => VmxExitReasonBasic::BUS_LOCK,
            75 => VmxExitReasonBasic::NOTIFY,
            _ => VmxExitReasonBasic::UNKNOWN,
        }
    }
}

#[derive(Debug, PartialEq)]
#[allow(dead_code)]
pub enum ExitFastpathCompletion {
    None,
    ReenterGuest,
    ExitHandled,
}

pub struct VmxExitHandler;

impl VmxExitHandler {
    pub fn handle(
        vcpu: &mut VirtCpu,
        basic: VmxExitReasonBasic,
    ) -> Option<Result<(), SystemError>> {
        match basic {
            VmxExitReasonBasic::IO_INSTRUCTION => {
                return Some(Self::handle_io(vcpu));
            }
            _ => {
                return None;
            }
        }
    }

    fn handle_io(vcpu: &mut VirtCpu) -> Result<(), SystemError> {
        todo!();
    }
}
