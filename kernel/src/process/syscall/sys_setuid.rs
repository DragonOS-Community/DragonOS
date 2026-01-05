use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETUID;
use crate::process::cred::Cred;
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
        let uid = Self::uid(args);
        // setuid 不接受 -1，使用专门的验证函数
        id_utils::validate_setuid_id(uid)?;

        let pcb = ProcessManager::current_pcb();
        let mut guard = pcb.cred.lock();

        let old_ruid = guard.uid.data();
        let old_euid = guard.euid.data();
        let old_suid = guard.suid.data();

        let mut new_cred = (**guard).clone();

        if guard.euid.data() == 0 {
            // 特权进程：设置所有 UID
            new_cred.setuid(uid);
            new_cred.seteuid(uid);
            new_cred.setsuid(uid);
            new_cred.setfsuid(uid);
        } else if uid == guard.uid.data() || uid == guard.euid.data() || uid == guard.suid.data() {
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
        vec![FormattedSyscallParam::new(
            "uid",
            format!("{:#x}", Self::uid(args)),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETUID, SysSetUid);
