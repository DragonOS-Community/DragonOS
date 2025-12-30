use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETRESUID;
use crate::process::cred::Cred;
use crate::process::syscall::id_utils;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysSetResUid;

impl SysSetResUid {
    fn ruid(args: &[usize]) -> usize {
        args[0]
    }
    fn euid(args: &[usize]) -> usize {
        args[1]
    }
    fn suid(args: &[usize]) -> usize {
        args[2]
    }
}

impl Syscall for SysSetResUid {
    fn num_args(&self) -> usize {
        3
    }

    /// setresuid - set real, effective and saved user IDs
    ///
    /// 参考: https://man7.org/linux/man-pages/man2/setresuid.2.html
    /// 参考: https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/sys.c#setresuid
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let ruid = Self::ruid(args);
        let euid = Self::euid(args);
        let suid = Self::suid(args);

        id_utils::validate_id(ruid)?;
        id_utils::validate_id(euid)?;
        id_utils::validate_id(suid)?;

        let pcb = ProcessManager::current_pcb();
        let mut guard = pcb.cred.lock();

        let old_ruid = guard.uid.data();
        let old_euid = guard.euid.data();
        let old_suid = guard.suid.data();

        let new_ruid = id_utils::resolve_id(ruid, old_ruid);
        let new_euid = id_utils::resolve_id(euid, old_euid);
        let new_suid = id_utils::resolve_id(suid, old_suid);

        let is_privileged = guard.euid.data() == 0;
        id_utils::check_setres_permissions(
            old_ruid,
            old_euid,
            old_suid,
            new_ruid,
            new_euid,
            new_suid,
            is_privileged,
        )?;

        let mut new_cred = (**guard).clone();

        // 设置新的 UID 值
        if !id_utils::is_no_change(ruid) {
            new_cred.setuid(new_ruid);
        }
        if !id_utils::is_no_change(euid) {
            new_cred.seteuid(new_euid);
        }
        if !id_utils::is_no_change(suid) {
            new_cred.setsuid(new_suid);
        }

        // 处理 capability 更新（使用统一的处理函数）
        id_utils::handle_uid_capabilities(
            &mut new_cred,
            old_ruid,
            old_euid,
            old_suid,
            new_ruid,
            new_euid,
            new_suid,
        );

        // fsuid 跟随 euid
        if !id_utils::is_no_change(euid) {
            new_cred.setfsuid(new_euid);
        }

        *guard = Cred::new_arc(new_cred);

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("ruid", format!("{:#x}", Self::ruid(args))),
            FormattedSyscallParam::new("euid", format!("{:#x}", Self::euid(args))),
            FormattedSyscallParam::new("suid", format!("{:#x}", Self::suid(args))),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETRESUID, SysSetResUid);
