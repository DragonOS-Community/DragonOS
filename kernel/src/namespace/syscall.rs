use system_error::SystemError;

use crate::{
    process::{fork::CloneFlags, ProcessControlBlock, ProcessManager},
    syscall::Syscall,
};

use super::{create_new_namespaces, namespace::USER_NS, NsProxy};

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

        let check = Self::check_unshare_flags(unshare_flags);
        if check.is_err() {
            return check;
        }

        let current = ProcessManager::current_pcb();
        if let Some(nsproxy) = Self::unshare_nsproxy_namespaces(unshare_flags)? {
            *current.get_nsproxy().write() = nsproxy;
        }

        Ok(0)
    }

    fn check_unshare_flags(unshare_flags: u64) -> Result<usize, SystemError> {
        let valid_flags = CloneFlags::CLONE_THREAD
            | CloneFlags::CLONE_FS
            | CloneFlags::CLONE_NEWNS
            | CloneFlags::CLONE_SIGHAND
            | CloneFlags::CLONE_VM
            | CloneFlags::CLONE_FILES
            | CloneFlags::CLONE_SYSVSEM
            | CloneFlags::CLONE_NEWUTS
            | CloneFlags::CLONE_NEWIPC
            | CloneFlags::CLONE_NEWNET
            | CloneFlags::CLONE_NEWUSER
            | CloneFlags::CLONE_NEWPID
            | CloneFlags::CLONE_NEWCGROUP;

        if unshare_flags & !valid_flags.bits() != 0 {
            return Err(SystemError::EINVAL);
        }
        Ok(0)
    }

    fn unshare_nsproxy_namespaces(unshare_flags: u64) -> Result<Option<NsProxy>, SystemError> {
        if (unshare_flags
            & (CloneFlags::CLONE_NEWNS.bits()
                | CloneFlags::CLONE_NEWUTS.bits()
                | CloneFlags::CLONE_NEWIPC.bits()
                | CloneFlags::CLONE_NEWNET.bits()
                | CloneFlags::CLONE_NEWPID.bits()
                | CloneFlags::CLONE_NEWCGROUP.bits()))
            == 0
        {
            return Ok(None);
        }
        let current = ProcessManager::current_pid();
        let pcb = ProcessManager::find(current).unwrap();
        let new_nsproxy = create_new_namespaces(unshare_flags, &pcb, USER_NS.clone())?;
        Ok(Some(new_nsproxy))
    }
}
