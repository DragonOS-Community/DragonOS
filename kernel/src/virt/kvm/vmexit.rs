use crate::virt::kvm::vmx_asm_wrapper::{
    vmxon, vmxoff, vmx_vmwrite, vmx_vmread, vmx_vmlaunch, vmx_vmptrld, vmx_vmclear
};
use crate::syscall::SystemError;
use crate::virt::kvm::vmcs::{VmcsFields};
use crate::virt::kvm::hypervisor::GuestCpuContext;
use x86::cpuid::cpuid;
use core::arch::asm;
use crate::{kdebug};

#[derive(FromPrimitive)]
#[allow(non_camel_case_types)]
pub enum APICExceptionVectors 
{
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
    EXCEPTION_RESERVED12
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
    INTERRUPT_TYPE_OTHER_EVENT = 7
}

pub fn vmexit_vmx_instruction_executed() -> Result<(), SystemError>{
    let valid: u32 = 1;
    let vector: u32 = APICExceptionVectors::EXCEPTION_UNDEFINED_OPCODE as  u32;
    let interrupt_type = InterruptType::INTERRUPT_TYPE_HARDWARE_EXCEPTION as u32;
    let deliver_code: u32 = 0;
    let interrupt_info = valid << 31 | interrupt_type << 8 | deliver_code << 11 | vector;
    vmx_vmwrite(VmcsFields::CTRL_VM_ENTRY_INTR_INFO_FIELD as u32, interrupt_info as u64)?;
    vmx_vmwrite(VmcsFields::CTRL_VM_ENTRY_INSTR_LEN as u32, 0)?;
    let rflags:u64 = vmx_vmread(VmcsFields::GUEST_RFLAGS as u32).unwrap() | 0x0001_0000; // set RF flags
    vmx_vmwrite(VmcsFields::GUEST_RFLAGS as u32, rflags);
    Ok(())
}

pub fn vmexit_cpuid_handler(guest_cpu_context: &mut GuestCpuContext) -> Result<(), SystemError>{
    let rax = guest_cpu_context.rax;
    let rcx = guest_cpu_context.rcx;
    let rdx = guest_cpu_context.rdx;
    let rbx = guest_cpu_context.rbx;
    // kdebug!("rax={:x}, rcx={:x}", guest_cpu_context.rax, guest_cpu_context.rcx);
    // cpuid!(rax, rcx);
    // unsafe{asm!("mov {}, rax", out(reg) guest_cpu_context.rax)};
    // unsafe{asm!("mov {}, rcx", out(reg) guest_cpu_context.rcx)};
    // unsafe{asm!("mov {}, rdx", out(reg) guest_cpu_context.rdx)};
    // unsafe{asm!("mov {}, rbx", out(reg) guest_cpu_context.rbx)};
    Ok(())
}