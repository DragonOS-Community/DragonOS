use core::{
    ptr::null_mut,
    sync::atomic::{compiler_fence, Ordering},
};

use crate::{
    arch::{asm::current::current_pcb, mm::test_buddy},
    mm::{
        set_INITIAL_PROCESS_ADDRESS_SPACE, ucontext::AddressSpace, INITIAL_PROCESS_ADDRESS_SPACE,
    }, kdebug,
};

pub mod abi;
pub mod c_adapter;
pub mod exec;
pub mod fork;
pub mod initial_proc;
pub mod pid;
pub mod preempt;
pub mod process;
pub mod syscall;

pub fn process_init() {
    unsafe {
        compiler_fence(Ordering::SeqCst);
        current_pcb().address_space = null_mut();
        kdebug!("To create address space for INIT process.");
        // test_buddy();
        set_INITIAL_PROCESS_ADDRESS_SPACE(
            AddressSpace::new(true).expect("Failed to create address space for INIT process."),
        );
        kdebug!("INIT process address space created.");
        compiler_fence(Ordering::SeqCst);
        current_pcb().set_address_space(INITIAL_PROCESS_ADDRESS_SPACE());
        compiler_fence(Ordering::SeqCst);
    };
}
