use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETREUID;
use crate::process::cred::Cred;
use crate::process::syscall::id_utils;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysSetReUid;

impl SysSetReUid {
    fn ruid(args: &[usize]) -> usize {
        args[0]
    }

    fn euid(args: &[usize]) -> usize {
        args[1]
    }
}

impl Syscall for SysSetReUid {
    fn num_args(&self) -> usize {
        2
    }

    /// setreuid - set real and/or effective user ID
    ///
    /// Linux 语义要点（参考 Linux 6.6 / man2 setreuid）:
    /// - 参数为 -1 表示不修改对应字段
    /// - 非特权进程只能将 ruid/euid 设置为当前 ruid/euid/suid 之一
    /// - 如果设置了 ruid，或者 euid 被设置为与旧 ruid 不同的值，则 suid = new_euid
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let ruid = Self::ruid(args);
        let euid = Self::euid(args);

        id_utils::validate_id(ruid)?;
        id_utils::validate_id(euid)?;

        let pcb = ProcessManager::current_pcb();
        let mut guard = pcb.cred.lock();

        let old_ruid = guard.uid.data();
        let old_euid = guard.euid.data();
        let old_suid = guard.suid.data();

        let new_ruid = id_utils::resolve_id(ruid, old_ruid);
        let new_euid = id_utils::resolve_id(euid, old_euid);

        let is_privileged = guard.euid.data() == 0;
        id_utils::check_setre_permissions(
            old_ruid,
            old_euid,
            old_suid,
            new_ruid,
            new_euid,
            is_privileged,
        )?;

        let mut new_cred = (**guard).clone();

        if !id_utils::is_no_change(ruid) {
            new_cred.setuid(new_ruid);
        }
        if !id_utils::is_no_change(euid) {
            new_cred.seteuid(new_euid);
            new_cred.setfsuid(new_euid);
        }

        // 更新 suid 的规则
        // 如果设置了 ruid，或者 euid 被设置为与旧 ruid 不同的值，则 suid = new_euid
        // 这个条件确保：当 ruid 改变时，或者当 euid 改变且新 euid 不等于旧 ruid 时，更新 suid
        if !id_utils::is_no_change(ruid) || (!id_utils::is_no_change(euid) && new_euid != old_ruid)
        {
            new_cred.setsuid(new_euid);
        }

        // 处理 capability 更新
        let new_ruid = new_cred.uid.data();
        let new_euid = new_cred.euid.data();
        let new_suid = new_cred.suid.data();
        id_utils::handle_uid_capabilities(
            &mut new_cred,
            old_ruid,
            old_euid,
            old_suid,
            new_ruid,
            new_euid,
            new_suid,
        );

        *guard = Cred::new_arc(new_cred);
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("ruid", format!("{:#x}", Self::ruid(args))),
            FormattedSyscallParam::new("euid", format!("{:#x}", Self::euid(args))),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETREUID, SysSetReUid);
