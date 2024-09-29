use bitfield_struct::bitfield;
use system_error::SystemError;
use x86::vmx::vmcs::{guest, ro};

use crate::{
    arch::vm::asm::{IntrInfo, VmxAsm},
    kerror,
    libs::spinlock::SpinLockGuard,
    virt::vm::kvm_host::{vcpu::VirtCpu, Vm},
};

use super::{ept::EptViolationExitQual, vmx_info, PageFaultErr};
extern crate num_traits;

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

//#define VMX_EXIT_REASONS
#[derive(FromPrimitive, PartialEq, Clone, Copy)]
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
    VIRTUALIZED_EOI = 45, // "EOI_INDUCED"
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
pub struct VmxExitHandlers {}
//     //name 代表暂时不懂含义的(name linux=name DragonOS)
//     ExceptionNmi = VmxExitReasonBasic::EXCEPTION_OR_NMI as isize,
//     ExternalInterrupt = VmxExitReasonBasic::EXTERNAL_INTERRUPT as isize,
//     TripleFault = VmxExitReasonBasic::TRIPLE_FAULT as isize,
//     NmiWindow = VmxExitReasonBasic::NMI_WINDOW as isize,
//     IoInstruction = VmxExitReasonBasic::IO_INSTRUCTION as isize,
//     CrAccess = VmxExitReasonBasic::CR_ACCESS as isize,
//     DrAccess = VmxExitReasonBasic::DR_ACCESS as isize,
//     Cpuid = VmxExitReasonBasic::CPUID as isize,
//     MsrRead = VmxExitReasonBasic::RDMSR as isize,
//     MsrWrite = VmxExitReasonBasic::WRMSR as isize,
//     InterruptWindow = VmxExitReasonBasic::INTERRUPT_WINDOW as isize,
//     Hlt = VmxExitReasonBasic::HLT as isize,
//     Invd = VmxExitReasonBasic::INVD as isize,
//     Invlpg = VmxExitReasonBasic::INVLPG as isize,
//     Rdpmc = VmxExitReasonBasic::RDPMC as isize,
//     Vmcall = VmxExitReasonBasic::VMCALL as isize,
//     Vmclear = VmxExitReasonBasic::VMCLEAR as isize,
//     Vmlaunch = VmxExitReasonBasic::VMLAUNCH as isize,
//     Vmptrld = VmxExitReasonBasic::VMPTRLD as isize,
//     Vmptrst = VmxExitReasonBasic::VMPTRST as isize,
//     Vmread = VmxExitReasonBasic::VMREAD as isize,
//     Vmresume = VmxExitReasonBasic::VMRESUME as isize,
//     Vmwrite = VmxExitReasonBasic::VMWRITE as isize,
//     Vmoff = VmxExitReasonBasic::VMXOFF as isize,
//     Vmon = VmxExitReasonBasic::VMXON as isize,
//     TprBelowThreshold = VmxExitReasonBasic::TPR_BELOW_THRESHOLD as isize,
//     ApicAccess = VmxExitReasonBasic::APIC_ACCESS as isize,
//     ApicWrite = VmxExitReasonBasic::APIC_WRITE as isize,
//     EoiInduced = VmxExitReasonBasic::VIRTUALIZED_EOI as isize, //name
//     Wbinvd = VmxExitReasonBasic::WBINVD as isize,
//     Xsetbv = VmxExitReasonBasic::XSETBV as isize,
//     TaskSwitch = VmxExitReasonBasic::TASK_SWITCH as isize,
//     MceDuringVmentry = VmxExitReasonBasic::VM_ENTRY_FAILURE_MACHINE_CHECK_EVENT as isize, //name
//     GdtrIdtr = VmxExitReasonBasic::ACCESS_GDTR_OR_IDTR as isize,
//     LdtrTr = VmxExitReasonBasic::ACCESS_LDTR_OR_TR as isize,
//     EptViolation = VmxExitReasonBasic::EPT_VIOLATION as isize,
//     EptMisconfig = VmxExitReasonBasic::EPT_MISCONFIG as isize,
//     PauseInstruction = VmxExitReasonBasic::PAUSE as isize,
//     MwaitInstruction = VmxExitReasonBasic::MWAIT as isize,
//     MonitorTrapFlag = VmxExitReasonBasic::MONITOR_TRAP_FLAG as isize,
//     MonitorInstruction = VmxExitReasonBasic::MONITOR as isize,
//     Invept = VmxExitReasonBasic::INVEPT as isize,
//     Invvpid = VmxExitReasonBasic::INVVPID as isize,
//     Rdrand = VmxExitReasonBasic::RDRAND as isize,
//     Rdseed = VmxExitReasonBasic::RDSEED as isize,
//     PmlFull = VmxExitReasonBasic::PML_FULL as isize,
//     Invpcid = VmxExitReasonBasic::INVPCID as isize,
//     Vmfunc = VmxExitReasonBasic::VMFUNC as isize,
//     PreemptionTimer = VmxExitReasonBasic::VMX_PREEMPTION_TIMER_EXPIRED as isize,
//     Encls = VmxExitReasonBasic::ENCLS as isize,
//     BusLock = VmxExitReasonBasic::BUS_LOCK as isize,
//     Notify = VmxExitReasonBasic::NOTIFY as isize,
//     Unknown,

impl VmxExitHandlers {
    #[inline(never)]
    pub fn try_handle_exit(
        vcpu: &mut VirtCpu,
        vm: &Vm,
        basic: VmxExitReasonBasic,
    ) -> Option<Result<u64, SystemError>> {
        match basic {
            VmxExitReasonBasic::IO_INSTRUCTION => {
                return Some(Self::handle_io(vcpu));
            }
            VmxExitReasonBasic::EPT_VIOLATION => {
                return Some(Self::handle_ept_violation(vcpu, vm));
            }
            VmxExitReasonBasic::EXTERNAL_INTERRUPT => {
                return Some(Self::handle_external_interrupt(vcpu));
            }
            _ => None,
        }
    }

    fn handle_io(vcpu: &mut VirtCpu) -> Result<u64, SystemError> {
        let exit_qualification = vcpu.get_exit_qual();
        let string = (exit_qualification & 16) != 0;

        vcpu.stat.io_exits += 1;

        if string {
            todo!("kvm_emulate_instruction todo");
        }

        let port = exit_qualification >> 16;
        let size = (exit_qualification & 7) + 1;
        let is_in = (exit_qualification & 8) != 0;

        return vcpu
            .arch
            .kvm_fast_pio(
                vcpu.run.as_mut().unwrap().as_mut(),
                size as u32,
                port as u16,
                is_in,
            )
            .and(Ok(0));
    }

    fn handle_external_interrupt(vcpu: &mut VirtCpu) -> Result<u64, SystemError> {
        vcpu.stat.irq_exits += 1;
        Ok(1)
    }

    fn handle_ept_violation(vcpu: &mut VirtCpu, vm: &Vm) -> Result<u64, SystemError> {
        let exit_qualification = vcpu.get_exit_qual();

        // EPT 违规发生在从 NMI 执行 iret 时，
        // 在下一次 VM 进入之前必须设置 "blocked by NMI" 位。
        // 有一些错误可能会导致该位未被设置：
        // AAK134, BY25。
        let vmx = vcpu.vmx();
        if vmx.idt_vectoring_info.bits() & IntrInfo::INTR_INFO_VALID_MASK.bits() != 0
            && vmx_info().enable_vnmi
            && exit_qualification & IntrInfo::INTR_INFO_UNBLOCK_NMI.bits() as u64 != 0
        {
            VmxAsm::vmx_vmwrite(guest::INTERRUPTIBILITY_STATE, 0x8); //GUEST_INTR_STATE_NMI
        }
        let gpa = VmxAsm::vmx_vmread(ro::GUEST_PHYSICAL_ADDR_FULL);
        // trace_kvm_page_fault(vcpu, gpa, exit_qualification);//fztodo!()

        // 根据故障类型确定错误代码
        let mut error_code = if exit_qualification & (EptViolationExitQual::ACC_READ.bits()) != 0 {
            //active
            PageFaultErr::PFERR_USER.bits()
        } else {
            0
        };
        error_code |= if exit_qualification & (EptViolationExitQual::ACC_WRITE.bits()) != 0 {
            //active
            PageFaultErr::PFERR_WRITE.bits()
        } else {
            0
        };
        error_code |= if exit_qualification & (EptViolationExitQual::ACC_INSTR.bits()) != 0 {
            PageFaultErr::PFERR_FETCH.bits()
        } else {
            0
        };
        error_code |= if exit_qualification & (EptViolationExitQual::RWX_MASK.bits()) != 0 {
            PageFaultErr::PFERR_PRESENT.bits()
        } else {
            0
        };
        error_code |= if exit_qualification & (EptViolationExitQual::GVA_TRANSLATED.bits()) != 0 {
            PageFaultErr::PFERR_GUEST_FINAL.bits() //active
        } else {
            PageFaultErr::PFERR_GUEST_PAGE.bits()
        };
        //fixme:: 此时error_code为0x100000011,感觉有问题

        vcpu.arch.exit_qual = exit_qualification;

        // 检查 GPA 是否超出物理内存限制，因为这是一个客户机页面错误。
        // 我们必须在这里模拟指令，因为如果非法地址是分页结构的地址，
        // 则会设置 EPT_VIOLATION_ACC_WRITE 位。
        // 或者，如果支持，我们还可以使用 EPT 违规的高级 VM 退出信息来重建页面错误代码。
        // if allow_smaller_maxphyaddr && kvm_vcpu_is_illegal_gpa(vcpu, gpa) {
        //     return kvm_emulate_instruction(vcpu, 0);
        // }

        vcpu.page_fault(vm, gpa, error_code, None, 0)
    }
}
