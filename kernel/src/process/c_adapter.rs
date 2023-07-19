use crate::{
    include::bindings::bindings::process_control_block,
    mm::{set_INITIAL_PROCESS_ADDRESS_SPACE, ucontext::AddressSpace},
};

use super::{fork::copy_mm, process_init};

#[no_mangle]
pub extern "C" fn rs_process_init() {
    process_init();
}

#[no_mangle]
pub extern "C" fn rs_process_copy_mm(clone_vm: bool, new_pcb: &mut process_control_block) -> usize {
    return copy_mm(clone_vm, new_pcb)
        .map(|_| 0)
        .unwrap_or_else(|err| err.to_posix_errno() as usize);
}
