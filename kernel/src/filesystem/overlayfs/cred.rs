use crate::process::{Cred, ProcessControlBlock, ProcessManager};
use alloc::sync::Arc;
use system_error::SystemError;

/// RAII guard for Linux-style overlayfs backing credential overrides.
///
/// Callers must finish all user-memory access before constructing this guard.
pub(super) struct CredOverrideGuard {
    pcb: Arc<ProcessControlBlock>,
    saved_cred: Arc<Cred>,
}

impl CredOverrideGuard {
    pub(super) fn new(override_cred: Arc<Cred>) -> Result<Self, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let saved_cred = pcb.cred();
        pcb.set_cred(override_cred)?;
        Ok(Self { pcb, saved_cred })
    }
}

impl Drop for CredOverrideGuard {
    fn drop(&mut self) {
        if let Err(err) = self.pcb.set_cred(self.saved_cred.clone()) {
            log::error!("overlayfs: failed to restore backing credentials: {err:?}");
        }
    }
}
