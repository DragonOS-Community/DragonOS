use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETRESGID;
use crate::process::cred::{CAPFlags, Cred};
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
        let pcb = ProcessManager::current_pcb();
        let old_cred = pcb.cred();
        let rgid = id_utils::map_gid_arg(&old_cred.user_ns, Self::rgid(args), true)?;
        let egid = id_utils::map_gid_arg(&old_cred.user_ns, Self::egid(args), true)?;
        let sgid = id_utils::map_gid_arg(&old_cred.user_ns, Self::sgid(args), true)?;

        let old_rgid = old_cred.gid.data();
        let old_egid = old_cred.egid.data();
        let old_sgid = old_cred.sgid.data();

        let new_rgid = id_utils::resolve_id(rgid, old_rgid);
        let new_egid = id_utils::resolve_id(egid, old_egid);
        let new_sgid = id_utils::resolve_id(sgid, old_sgid);

        if new_rgid == old_rgid
            && new_sgid == old_sgid
            && new_egid == old_egid
            && (id_utils::is_no_change(egid) || new_egid == old_cred.fsgid.data())
        {
            return Ok(0);
        }

        let is_privileged = old_cred.has_capability(CAPFlags::CAP_SETGID);
        id_utils::check_setres_permissions(
            old_rgid,
            old_egid,
            old_sgid,
            new_rgid,
            new_egid,
            new_sgid,
            is_privileged,
        )?;

        let mut new_cred = (*old_cred).clone();

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
        new_cred.setfsgid(new_egid);

        pcb.commit_cred(Cred::new_arc(new_cred))?;
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
