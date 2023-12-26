use super::vmcs::{VmcsFields, VmxExitReason};
use super::vmx_asm_wrapper::{vmx_vmread, vmx_vmwrite};
use crate::kdebug;
use crate::virt::kvm::vm;
use core::arch::asm;
use system_error::SystemError;
use x86::vmx::vmcs::ro::GUEST_PHYSICAL_ADDR_FULL;

#[derive(FromPrimitive)]
#[allow(non_camel_case_types)]
pub enum APICExceptionVectors {
    EXCEPTION_DIVIDE_ERROR,
    EXCEPTION_DEBUG_BREAKPOINT,
    EXCEPTION_NMI,
    EXCEPTION_BREAKPOINT,
    EXCEPTION_OVERFLOW,
    EXCEPTION_BOUND_RANGE_EXCEEDED,
    EXCEPTION_UNDEFINED_OPCODE,
    EXCEPTION_NO_MATH_COPROCESSOR,
    EXCEPTION_DOUBLE_FAULT,
    EXCEPTION_RESERVED0,
    EXCEPTION_INVALID_TASK_SEGMENT_SELECTOR,
    EXCEPTION_SEGMENT_NOT_PRESENT,
    EXCEPTION_STACK_SEGMENT_FAULT,
    EXCEPTION_GENERAL_PROTECTION_FAULT,
    EXCEPTION_PAGE_FAULT,
    EXCEPTION_RESERVED1,
    EXCEPTION_MATH_FAULT,
    EXCEPTION_ALIGNMENT_CHECK,
    EXCEPTION_MACHINE_CHECK,
    EXCEPTION_SIMD_FLOATING_POINT_NUMERIC_ERROR,
    EXCEPTION_VIRTUAL_EXCEPTION,
    EXCEPTION_RESERVED2,
    EXCEPTION_RESERVED3,
    EXCEPTION_RESERVED4,
    EXCEPTION_RESERVED5,
    EXCEPTION_RESERVED6,
    EXCEPTION_RESERVED7,
    EXCEPTION_RESERVED8,
    EXCEPTION_RESERVED9,
    EXCEPTION_RESERVED10,
    EXCEPTION_RESERVED11,
    EXCEPTION_RESERVED12,
}

#[derive(FromPrimitive)]
#[allow(non_camel_case_types)]
pub enum InterruptType {
    INTERRUPT_TYPE_EXTERNAL_INTERRUPT = 0,
    INTERRUPT_TYPE_RESERVED = 1,
    INTERRUPT_TYPE_NMI = 2,
    INTERRUPT_TYPE_HARDWARE_EXCEPTION = 3,
    INTERRUPT_TYPE_SOFTWARE_INTERRUPT = 4,
    INTERRUPT_TYPE_PRIVILEGED_SOFTWARE_INTERRUPT = 5,
    INTERRUPT_TYPE_SOFTWARE_EXCEPTION = 6,
    INTERRUPT_TYPE_OTHER_EVENT = 7,
}

pub fn vmexit_vmx_instruction_executed() -> Result<(), SystemError> {
    let valid: u32 = 1;
    let vector: u32 = APICExceptionVectors::EXCEPTION_UNDEFINED_OPCODE as u32;
    let interrupt_type = InterruptType::INTERRUPT_TYPE_HARDWARE_EXCEPTION as u32;
    let deliver_code: u32 = 0;
    let interrupt_info = valid << 31 | interrupt_type << 8 | deliver_code << 11 | vector;
    vmx_vmwrite(
        VmcsFields::CTRL_VM_ENTRY_INTR_INFO_FIELD as u32,
        interrupt_info as u64,
    )?;
    vmx_vmwrite(VmcsFields::CTRL_VM_ENTRY_INSTR_LEN as u32, 0)?;
    let rflags: u64 = vmx_vmread(VmcsFields::GUEST_RFLAGS as u32).unwrap() | 0x0001_0000; // set RF flags
    vmx_vmwrite(VmcsFields::GUEST_RFLAGS as u32, rflags)?;
    Ok(())
}

// pub fn vmexit_cpuid_handler(guest_cpu_context: &mut GuestCpuContext) -> Result<(), SystemError>{
//     let rax = guest_cpu_context.rax;
//     let rcx = guest_cpu_context.rcx;
//     // let rdx = guest_cpu_context.rdx;
//     // let rbx = guest_cpu_context.rbx;
//     cpuid!(rax, rcx);
//     unsafe{asm!("mov {}, rax", out(reg) guest_cpu_context.rax)};
//     unsafe{asm!("mov {}, rcx", out(reg) guest_cpu_context.rcx)};
//     unsafe{asm!("mov {}, rdx", out(reg) guest_cpu_context.rdx)};
//     unsafe{asm!("mov {}, rbx", out(reg) guest_cpu_context.rbx)};
//     Ok(())
// }

unsafe fn save_rpg() {
    asm!(
        "push    rax",
        "push    rcx",
        "push    rdx",
        "push    rbx",
        "push    rbp",
        "push    rsi",
        "push    rdi",
        "push    r8",
        "push    r9",
        "push    r10",
        "push    r11",
        "push    r12",
        "push    r13",
        "push    r14",
        "push    r15",
    );
}

unsafe fn restore_rpg() {
    asm!(
        "pop    r15",
        "pop    r14",
        "pop    r13",
        "pop    r12",
        "pop    r11",
        "pop    r10",
        "pop    r9",
        "pop    r8",
        "pop    rdi",
        "pop    rsi",
        "pop    rbp",
        "pop    rbx",
        "pop    rdx",
        "pop    rcx",
        "pop    rax",
    );
}

#[repr(C)]
#[allow(dead_code)]
pub struct GuestCpuContext {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rax: u64,
}

#[no_mangle]
pub extern "C" fn vmx_return() {
    kdebug!("vmx_return!");
    unsafe { save_rpg() };
    vmexit_handler();
    // XMM registers are vector registers. They're renamed onto the FP/SIMD register file
    // unsafe {asm!(
    //     "sub     rsp, 60h",
    //     "movaps  xmmword ptr [rsp +  0h], xmm0",
    //     "movaps  xmmword ptr [rsp + 10h], xmm1",
    //     "movaps  xmmword ptr [rsp + 20h], xmm2",
    //     "movaps  xmmword ptr [rsp + 30h], xmm3",
    //     "movaps  xmmword ptr [rsp + 40h], xmm4",
    //     "movaps  xmmword ptr [rsp + 50h], xmm5",

    //     "mov     rdi, rsp",
    //     "sub     rsp, 20h",
    //     "call vmexit_handler",
    //     "add     rsp, 20h",

    //     "movaps  xmm0, xmmword ptr [rsp +  0h]",
    //     "movaps  xmm1, xmmword ptr [rsp + 10h]",
    //     "movaps  xmm2, xmmword ptr [rsp + 20h]",
    //     "movaps  xmm3, xmmword ptr [rsp + 30h]",
    //     "movaps  xmm4, xmmword ptr [rsp + 40h]",
    //     "movaps  xmm5, xmmword ptr [rsp + 50h]",
    //     "add     rsp, 60h",
    // clobber_abi("C"),
    // )};
    unsafe { restore_rpg() };
    unsafe { asm!("vmresume",) };
}

#[no_mangle]
extern "C" fn vmexit_handler() {
    // let guest_cpu_context = unsafe { guest_cpu_context_ptr.as_mut().unwrap() };
    // kdebug!("guest_cpu_context_ptr={:p}",guest_cpu_context_ptr);
    kdebug!("vmexit handler!");

    let exit_reason = vmx_vmread(VmcsFields::VMEXIT_EXIT_REASON as u32).unwrap() as u32;
    let exit_basic_reason = exit_reason & 0x0000_ffff;
    let guest_rip = vmx_vmread(VmcsFields::GUEST_RIP as u32).unwrap();
    // let guest_rsp = vmx_vmread(VmcsFields::GUEST_RSP as u32).unwrap();
    kdebug!("guest_rip={:x}", guest_rip);
    let _guest_rflags = vmx_vmread(VmcsFields::GUEST_RFLAGS as u32).unwrap();

    match VmxExitReason::from(exit_basic_reason as i32) {
        VmxExitReason::VMCALL
        | VmxExitReason::VMCLEAR
        | VmxExitReason::VMLAUNCH
        | VmxExitReason::VMPTRLD
        | VmxExitReason::VMPTRST
        | VmxExitReason::VMREAD
        | VmxExitReason::VMRESUME
        | VmxExitReason::VMWRITE
        | VmxExitReason::VMXOFF
        | VmxExitReason::VMXON
        | VmxExitReason::VMFUNC
        | VmxExitReason::INVEPT
        | VmxExitReason::INVVPID => {
            kdebug!("vmexit handler: vmx instruction!");
            vmexit_vmx_instruction_executed().expect("previledge instruction handle error");
        }
        VmxExitReason::CPUID => {
            kdebug!("vmexit handler: cpuid instruction!");
            // vmexit_cpuid_handler(guest_cpu_context);
            adjust_rip(guest_rip).unwrap();
        }
        VmxExitReason::RDMSR => {
            kdebug!("vmexit handler: rdmsr instruction!");
            adjust_rip(guest_rip).unwrap();
        }
        VmxExitReason::WRMSR => {
            kdebug!("vmexit handler: wrmsr instruction!");
            adjust_rip(guest_rip).unwrap();
        }
        VmxExitReason::TRIPLE_FAULT => {
            kdebug!("vmexit handler: triple fault!");
            adjust_rip(guest_rip).unwrap();
        }
        VmxExitReason::EPT_VIOLATION => {
            kdebug!("vmexit handler: ept violation!");
            let gpa = vmx_vmread(GUEST_PHYSICAL_ADDR_FULL as u32).unwrap();
            let exit_qualification = vmx_vmread(VmcsFields::VMEXIT_QUALIFICATION as u32).unwrap();
            /* It is a write fault? */
            let mut error_code = exit_qualification & (1 << 1);
            /* It is a fetch fault? */
            error_code |= (exit_qualification << 2) & (1 << 4);
            /* ept page table is present? */
            error_code |= (exit_qualification >> 3) & (1 << 0);

            let kvm = vm(0).unwrap();
            let vcpu = kvm.vcpu[0].clone();
            // Use the data
            let kvm_ept_page_fault = vcpu.lock().mmu.page_fault.unwrap();
            kvm_ept_page_fault(&mut (*vcpu.lock()), gpa, error_code as u32, false)
                .expect("ept page fault error");
        }
        _ => {
            kdebug!(
                "vmexit handler: unhandled vmexit reason: {}!",
                exit_basic_reason
            );

            let info = vmx_vmread(VmcsFields::VMEXIT_INSTR_LEN as u32).unwrap() as u32;
            kdebug!("vmexit handler: VMEXIT_INSTR_LEN: {}!", info);
            let info = vmx_vmread(VmcsFields::VMEXIT_INSTR_INFO as u32).unwrap() as u32;
            kdebug!("vmexit handler: VMEXIT_INSTR_INFO: {}!", info);
            let info = vmx_vmread(VmcsFields::CTRL_EXPECTION_BITMAP as u32).unwrap() as u32;
            kdebug!("vmexit handler: CTRL_EXPECTION_BITMAP: {}!", info);

            adjust_rip(guest_rip).unwrap();
            // panic!();
        }
    }
}

#[no_mangle]
fn adjust_rip(rip: u64) -> Result<(), SystemError> {
    let instruction_length = vmx_vmread(VmcsFields::VMEXIT_INSTR_LEN as u32)?;
    vmx_vmwrite(VmcsFields::GUEST_RIP as u32, rip + instruction_length)?;
    Ok(())
}
