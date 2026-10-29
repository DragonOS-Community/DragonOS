use system_error::SystemError;

use crate::process::namespace::user_namespace::from_kuid_munged;
use crate::process::ProcessManager;

pub fn do_geteuid() -> Result<usize, SystemError> {
    let pcb = ProcessManager::current_pcb();
    let cred = pcb.cred();
    Ok(from_kuid_munged(&cred.user_ns, cred.euid) as usize)
}
