use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETUID;
use crate::process::cred::{CAPFlags, Cred};
use crate::process::syscall::id_utils;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysSetUid;

impl SysSetUid {
    fn uid(args: &[usize]) -> usize {
        args[0]
    }
}

impl Syscall for SysSetUid {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let old_cred = pcb.cred();
        let uid = id_utils::map_uid_arg(&old_cred.user_ns, Self::uid(args), false)?;

        let old_ruid = old_cred.uid.data();
        let old_euid = old_cred.euid.data();
        let old_suid = old_cred.suid.data();

        let mut new_cred = (*old_cred).clone();

        if old_cred.has_capability(CAPFlags::CAP_SETUID) {
            // 特权进程：设置所有 UID
            new_cred.setuid(uid);
            new_cred.seteuid(uid);
            new_cred.setsuid(uid);
            new_cred.setfsuid(uid);
        } else if uid == old_cred.uid.data() || uid == old_cred.suid.data() {
            // 非特权进程：只能设置 euid 为当前 ruid/euid/suid 之一
            new_cred.seteuid(uid);
            new_cred.setfsuid(uid);
        } else {
            return Err(SystemError::EPERM);
        }

        // 处理 capability 更新
        let new_ruid = new_cred.uid.data();
        let new_euid = new_cred.euid.data();
        let new_suid = new_cred.suid.data();
        let keepcaps = pcb.keepcaps();
        id_utils::handle_uid_capabilities(
            &mut new_cred,
            id_utils::UidTransition {
                old_ruid,
                old_euid,
                old_suid,
                new_ruid,
                new_euid,
                new_suid,
            },
            keepcaps,
        );

        pcb.commit_cred(Cred::new_arc(new_cred))?;

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "uid",
            format!("{:#x}", Self::uid(args)),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETUID, SysSetUid);
