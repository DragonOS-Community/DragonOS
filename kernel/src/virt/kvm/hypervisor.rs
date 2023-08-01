use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::syscall::SystemError;
use crate::virt::kvm::Vcpu;
use core::arch::asm;
use crate::{kdebug};
use crate::virt::kvm::vmx_asm_wrapper::{
    vmxon, vmxoff, vmx_vmwrite, vmx_vmread, vmx_vmlaunch, vmx_vmptrld, vmx_vmclear
};
use crate::virt::kvm::vmexit::{vmexit_vmx_instruction_executed, vmexit_cpuid_handler};
use crate::virt::kvm::vmcs::{VmcsFields, VmxExitReason};
pub struct Hypervisor {
    sys_fd: u32,	/* For system ioctls(), i.e. /dev/kvm */
    nr_vcpus: u32,  /* Number of cpus to run */
    vcpu: Vec<Vcpu>,
    pub host_stack: u64,

// 	vm_fd: u32,  	/* For VM ioctls() */
//     timerid: u32,   /* Posix timer for interrupts */
//     mem_slots: u32, /* for KVM_SET_USER_MEMORY_REGION */
//     ram_size: u64,  /* Guest memory size, in bytes */
//     ram_start: *u64,
//     ram_pagesize: u64,
//     mem_banks_lock: SpinLock<()>,
//     // mem_banks: Box<[kvm_mem_bank]>,

//     vm_state: u32,
}

impl Hypervisor {
    pub fn new(sys_fd:u32, nr_vcpus: u32, host_stack: u64) -> Result<Box<Self>, SystemError> {
        let mut vcpu = Vec::new();
        // for i in 0..nr_vcpus {
        //     vcpu.push(Vcpu::new(i, Arc::new(hypervisor))?);
        // }
        // Allocate stack for vm-exit handlers and fill it with garbage data
        let instance = Self {
            sys_fd,
            nr_vcpus,
            vcpu,
            host_stack,
        };
        let mut instance = Box::new(instance);
        Ok(instance)
    }

    // pub fn virtualize_cpu(&self) -> Result<(), SystemError> {
    //     let mut vcpu = self.vcpu[0].lock();
    //     vcpu.virtualize_cpu();
    // }
}

unsafe fn save_rpg(){
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

unsafe fn restore_rpg(){
    asm!(
        "pop    rax",
        "pop    rcx",
        "pop    rdx",
        "pop    rbx",
        "pop    rbp",
        "pop    rsi",
        "pop    rdi",
        "pop    r8",
        "pop    r9",
        "pop    r10",
        "pop    r11",
        "pop    r12",
        "pop    r13",
        "pop    r14",
        "pop    r15",
    );
}

#[repr(C)]
pub struct GuestCpuContext{
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
pub unsafe fn vmx_return(){
    kdebug!("vmx_return!");
    save_rpg();
    // XMM registers are vector registers. They're renamed onto the FP/SIMD register file
    asm!(
        // "sub     rsp, 68h",
        // "movaps  xmmword ptr [rsp +  0h], xmm0",
    //     "movaps  xmmword ptr [rsp + 10h], xmm1",
    //     "movaps  xmmword ptr [rsp + 20h], xmm2",
    //     "movaps  xmmword ptr [rsp + 30h], xmm3",
    //     "movaps  xmmword ptr [rsp + 40h], xmm4",
    //     "movaps  xmmword ptr [rsp + 50h], xmm5",

        "mov     rcx, rsp",
        "sub     rsp, 20h",
        "call vmexit_handler",

        "add     rsp, 20h",
    //     "movaps  xmm0, xmmword ptr [rsp +  0h]",
    //     "movaps  xmm1, xmmword ptr [rsp + 10h]",
    //     "movaps  xmm2, xmmword ptr [rsp + 20h]",
    //     "movaps  xmm3, xmmword ptr [rsp + 30h]",
    //     "movaps  xmm4, xmmword ptr [rsp + 40h]",
    //     "movaps  xmm5, xmmword ptr [rsp + 50h]",
    //     "add     rsp, 68h",
    );

    restore_rpg();
    asm!(
        "vmresume"
    );
}

#[no_mangle]
fn vmexit_handler(){
    kdebug!("vmexit handler!");

    let mut guest_cpu_context_ptr: *const GuestCpuContext;
    unsafe{asm!("mov {}, rcx", out(reg) guest_cpu_context_ptr)};
    let mut guest_cpu_context = unsafe { &*guest_cpu_context_ptr };
    // kdebug!("rax={:x}, rcx={:x}", guest_cpu_context.rax, guest_cpu_context.rcx);


    let exit_reason = vmx_vmread(VmcsFields::VMEXIT_EXIT_REASON as u32).unwrap() as u32;
    let exit_basic_reason = exit_reason & 0x0000_ffff;
    let guest_rip = vmx_vmread(VmcsFields::GUEST_RIP as u32).unwrap();
    let guest_rsp = vmx_vmread(VmcsFields::GUEST_RSP as u32).unwrap();
    let guest_rflags = vmx_vmread(VmcsFields::GUEST_RFLAGS as u32).unwrap();

    match VmxExitReason::from(exit_basic_reason as i32) {
        VmxExitReason::VMCALL | VmxExitReason::VMCLEAR | VmxExitReason::VMLAUNCH | 
        VmxExitReason::VMPTRLD | VmxExitReason::VMPTRST | VmxExitReason::VMREAD | 
        VmxExitReason::VMRESUME | VmxExitReason::VMWRITE | VmxExitReason::VMXOFF | 
        VmxExitReason::VMXON | VmxExitReason::VMFUNC | VmxExitReason::INVEPT | 
        VmxExitReason::INVVPID => {
            kdebug!("vmexit handler: vmx instruction!");
            vmexit_vmx_instruction_executed();
        },
        VmxExitReason::CPUID => {
            kdebug!("vmexit handler: cpuid instruction!");
            // vmexit_cpuid_handler(guest_cpu_context);
            adjust_rip(guest_rip).unwrap();
        },
        VmxExitReason::RDMSR => {
            kdebug!("vmexit handler: rdmsr instruction!");
            adjust_rip(guest_rip).unwrap();
        },
        VmxExitReason::WRMSR => {
            kdebug!("vmexit handler: wrmsr instruction!");
            adjust_rip(guest_rip).unwrap();
        },
        VmxExitReason::TRIPLE_FAULT => {
            kdebug!("vmexit handler: triple fault!");
            adjust_rip(guest_rip).unwrap();
        },
        _ => {
            kdebug!("vmexit handler: unhandled vmexit reason!");
            panic!();
        }
    }
}

fn adjust_rip(rip: u64) -> Result<(), SystemError> {
    let instruction_length = vmx_vmread(VmcsFields::VMEXIT_INSTR_LEN as u32)?;
    vmx_vmwrite(VmcsFields::GUEST_RIP as u32, rip + instruction_length)?;
    Ok(())
}

