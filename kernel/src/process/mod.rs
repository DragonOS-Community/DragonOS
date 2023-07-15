use core::{sync::atomic::{compiler_fence, Ordering}, ptr::null_mut};

use crate::{mm::{set_INITIAL_PROCESS_ADDRESS_SPACE, ucontext::AddressSpace, INITIAL_PROCESS_ADDRESS_SPACE}, arch::asm::current::current_pcb};

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
        set_INITIAL_PROCESS_ADDRESS_SPACE(
            AddressSpace::new().expect("Failed to create address space for INIT process."),
        );
        compiler_fence(Ordering::SeqCst);
        current_pcb().set_address_space(INITIAL_PROCESS_ADDRESS_SPACE());
        compiler_fence(Ordering::SeqCst);
    };
}
