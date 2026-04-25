use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETRESGID;
use crate::process::cred::Cred;
use crate::process::syscall::id_utils;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysSetResGid;

impl SysSetResGid {
    fn rgid(args: &[usize]) -> usize {
        args[0]
    }

    fn egid(args: &[usize]) -> usize {
        args[1]
    }

    fn sgid(args: &[usize]) -> usize {
        args[2]
    }
}

impl Syscall for SysSetResGid {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let rgid = Self::rgid(args);
        let egid = Self::egid(args);
        let sgid = Self::sgid(args);

        id_utils::validate_id(rgid)?;
        id_utils::validate_id(egid)?;
        id_utils::validate_id(sgid)?;

        let pcb = ProcessManager::current_pcb();
        let mut guard = pcb.cred.lock();

        let old_rgid = guard.gid.data();
        let old_egid = guard.egid.data();
        let old_sgid = guard.sgid.data();

        let new_rgid = id_utils::resolve_id(rgid, old_rgid);
        let new_egid = id_utils::resolve_id(egid, old_egid);
        let new_sgid = id_utils::resolve_id(sgid, old_sgid);

        let is_privileged = guard.euid.data() == 0;
        id_utils::check_setres_permissions(
            old_rgid,
            old_egid,
            old_sgid,
            new_rgid,
            new_egid,
            new_sgid,
            is_privileged,
        )?;

        let mut new_cred = (**guard).clone();

        if !id_utils::is_no_change(rgid) {
            new_cred.setgid(new_rgid);
        }
        if !id_utils::is_no_change(egid) {
            new_cred.setegid(new_egid);
        }
        if !id_utils::is_no_change(sgid) {
            new_cred.setsgid(new_sgid);
        }

        // fsgid 跟随 egid
        if !id_utils::is_no_change(egid) {
            new_cred.setfsgid(new_egid);
        }

        *guard = Cred::new_arc(new_cred);
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("rgid", format!("{:#x}", Self::rgid(args))),
            FormattedSyscallParam::new("egid", format!("{:#x}", Self::egid(args))),
            FormattedSyscallParam::new("sgid", format!("{:#x}", Self::sgid(args))),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETRESGID, SysSetResGid);
