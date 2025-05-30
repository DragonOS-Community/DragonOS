use system_error::SystemError;

use crate::process::ProcessManager;

pub fn do_geteuid()->Result<usize,SystemError>{
    let pcb = ProcessManager::current_pcb();
    return Ok(pcb.cred.lock().euid.data());
}