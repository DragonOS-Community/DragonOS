use system_error::SystemError;

use crate::{
    process::{fork::CloneFlags, ProcessManager},
    syscall::Syscall,
};

use super::namespace::{
    check_unshare_flags, commit_nsset, prepare_nsset, unshare_nsproxy_namespaces,
};

impl Syscall {
    pub fn sys_unshare(mut unshare_flags: u64) -> Result<usize, SystemError> {
        if unshare_flags & CloneFlags::CLONE_NEWUSER.bits() != 0 {
            unshare_flags |= CloneFlags::CLONE_THREAD.bits() | CloneFlags::CLONE_FS.bits();
        }

        if unshare_flags & CloneFlags::CLONE_VM.bits() != 0 {
            unshare_flags |= CloneFlags::CLONE_SIGHAND.bits();
        }

        if unshare_flags & CloneFlags::CLONE_SIGHAND.bits() != 0 {
            unshare_flags |= CloneFlags::CLONE_THREAD.bits();
        }

        if unshare_flags & CloneFlags::CLONE_NEWNS.bits() != 0 {
            unshare_flags |= CloneFlags::CLONE_FS.bits();
        }

        let check = check_unshare_flags(unshare_flags)?;

        let current = ProcessManager::current_pcb();
        if let Some(nsproxy) = unshare_nsproxy_namespaces(unshare_flags)? {
            *current.get_nsproxy().write() = nsproxy;
        }

        Ok(check)
    }
    #[allow(dead_code)]
    pub fn sys_setns(_fd: i32, flags: u64) -> Result<usize, SystemError> {
        let check = check_unshare_flags(flags)?;

        let nsset = prepare_nsset(flags)?;

        if check == 0 {
            commit_nsset(nsset)
        };
        Ok(0)
    }
}
