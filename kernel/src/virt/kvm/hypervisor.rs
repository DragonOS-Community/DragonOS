use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::syscall::SystemError;
use crate::virt::kvm::Vcpu;
use core::arch::asm;
use crate::{kdebug};
pub const VMM_STACK_SIZE:usize = 0x1000 * 6;

pub struct Hypervisor {
    sys_fd: u32,	/* For system ioctls(), i.e. /dev/kvm */
    nr_vcpus: u32,  /* Number of cpus to run */
    vcpu: Vec<Vcpu>,
    pub stack: Vec<u8>,

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
    pub fn new(sys_fd:u32, nr_vcpus: u32) -> Result<Box<Self>, SystemError> {
        let mut vcpu = Vec::new();
        // for i in 0..nr_vcpus {
        //     vcpu.push(Vcpu::new(i, Arc::new(hypervisor))?);
        // }
        // Allocate stack for vm-exit handlers and fill it with garbage data
        let stack = vec![0xCC; VMM_STACK_SIZE];
        let instance = Self {
            sys_fd,
            nr_vcpus,
            vcpu,
            stack,
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

pub unsafe fn vmx_return(){
    save_rpg();
    // XMM registers are vector registers. They're renamed onto the FP/SIMD register file
    asm!(
        "sub     rsp, 68h",
        "movaps  xmmword ptr [rsp +  0h], xmm0",
        "movaps  xmmword ptr [rsp + 10h], xmm1",
        "movaps  xmmword ptr [rsp + 20h], xmm2",
        "movaps  xmmword ptr [rsp + 30h], xmm3",
        "movaps  xmmword ptr [rsp + 40h], xmm4",
        "movaps  xmmword ptr [rsp + 50h], xmm5",

        "mov     rcx, rsp",
        "sub     rsp, 20h",
        "call vmexit_handler",

        "add     rsp, 20h",
        "movaps  xmm0, xmmword ptr [rsp +  0h]",
        "movaps  xmm1, xmmword ptr [rsp + 10h]",
        "movaps  xmm2, xmmword ptr [rsp + 20h]",
        "movaps  xmm3, xmmword ptr [rsp + 30h]",
        "movaps  xmm4, xmmword ptr [rsp + 40h]",
        "movaps  xmm5, xmmword ptr [rsp + 50h]",
        "add     rsp, 68h",

        
    );
    restore_rpg();
    asm!(
        "vmresume"
    );
}

#[no_mangle]
fn vmexit_handler(){
    kdebug!("vmexit handler!");
}