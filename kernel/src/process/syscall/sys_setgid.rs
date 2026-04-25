use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETGID;
use crate::process::cred::Cred;
use crate::process::syscall::id_utils;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysSetGid;

impl SysSetGid {
    fn gid(args: &[usize]) -> usize {
        args[0]
    }
}

impl Syscall for SysSetGid {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let gid = Self::gid(args);
        // setgid 不接受 -1，使用专门的验证函数
        id_utils::validate_setuid_id(gid)?;

        let pcb = ProcessManager::current_pcb();
        let mut guard = pcb.cred.lock();
        let mut new_cred: Cred = (**guard).clone();

        if guard.euid.data() == 0 {
            // 特权进程：设置所有 GID
            new_cred.setgid(gid);
            new_cred.setegid(gid);
            new_cred.setsgid(gid);
            new_cred.setfsgid(gid);
        } else if guard.gid.data() == gid || guard.egid.data() == gid || guard.sgid.data() == gid {
            // 非特权进程：只能设置 egid 为当前 rgid/egid/sgid 之一
            new_cred.setegid(gid);
            new_cred.setfsgid(gid);
        } else {
            return Err(SystemError::EPERM);
        }

        // 注意：GID 变化不直接影响 capability，但为了代码一致性保留此调用
        // 目前 handle_gid_capabilities 是空实现
        let old_rgid = guard.gid.data();
        let old_egid = guard.egid.data();
        let old_sgid = guard.sgid.data();
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
        vec![FormattedSyscallParam::new(
            "gid",
            format!("{:#x}", Self::gid(args)),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETGID, SysSetGid);
