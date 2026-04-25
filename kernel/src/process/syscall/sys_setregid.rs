use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETREGID;
use crate::process::cred::Cred;
use crate::process::syscall::id_utils;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysSetReGid;

impl SysSetReGid {
    fn rgid(args: &[usize]) -> usize {
        args[0]
    }

    fn egid(args: &[usize]) -> usize {
        args[1]
    }
}

impl Syscall for SysSetReGid {
    fn num_args(&self) -> usize {
        2
    }

    /// setregid - set real and/or effective group ID
    ///
    /// Linux 语义要点（参考 Linux 6.6 / man2 setregid）:
    /// - 参数为 -1 表示不修改对应字段
    /// - 非特权进程只能将 rgid/egid 设置为当前 rgid/egid/sgid 之一
    /// - 如果设置了 rgid，或者 egid 被设置为与旧 rgid 不同的值，则 sgid = new_egid
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let rgid = Self::rgid(args);
        let egid = Self::egid(args);

        id_utils::validate_id(rgid)?;
        id_utils::validate_id(egid)?;

        let pcb = ProcessManager::current_pcb();
        let mut guard = pcb.cred.lock();

        let old_rgid = guard.gid.data();
        let old_egid = guard.egid.data();
        let old_sgid = guard.sgid.data();

        let new_rgid = id_utils::resolve_id(rgid, old_rgid);
        let new_egid = id_utils::resolve_id(egid, old_egid);

        let is_privileged = guard.euid.data() == 0;
        id_utils::check_setre_permissions(
            old_rgid,
            old_egid,
            old_sgid,
            new_rgid,
            new_egid,
            is_privileged,
        )?;

        let mut new_cred = (**guard).clone();

        if !id_utils::is_no_change(rgid) {
            new_cred.setgid(new_rgid);
        }
        if !id_utils::is_no_change(egid) {
            new_cred.setegid(new_egid);
            new_cred.setfsgid(new_egid);
        }

        // 更新 sgid 的规则
        // 如果设置了 rgid，或者 egid 被设置为与旧 rgid 不同的值，则 sgid = new_egid
        if !id_utils::is_no_change(rgid) || (!id_utils::is_no_change(egid) && new_egid != old_rgid)
        {
            new_cred.setsgid(new_egid);
        }

        // 注意：GID 变化不直接影响 capability，但为了代码一致性保留此调用
        // 目前 handle_gid_capabilities 是空实现
        let new_rgid = new_cred.gid.data();
        let new_egid = new_cred.egid.data();
        let new_sgid = new_cred.sgid.data();
        id_utils::handle_gid_capabilities(
            &mut new_cred,
            old_rgid,
            old_egid,
            old_sgid,
            new_rgid,
            new_egid,
            new_sgid,
        );

        *guard = Cred::new_arc(new_cred);
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("rgid", format!("{:#x}", Self::rgid(args))),
            FormattedSyscallParam::new("egid", format!("{:#x}", Self::egid(args))),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETREGID, SysSetReGid);
