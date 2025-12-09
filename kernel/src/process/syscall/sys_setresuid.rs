use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETRESUID;
use crate::process::cred::{CAPFlags, Cred};
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

        let pcb = ProcessManager::current_pcb();
        let mut guard = pcb.cred.lock();

        let old_ruid = guard.uid.data();
        let old_euid = guard.euid.data();
        let old_suid = guard.suid.data();

        // -1 (usize::MAX) 表示不修改该字段
        let new_ruid = if ruid == usize::MAX { old_ruid } else { ruid };
        let new_euid = if euid == usize::MAX { old_euid } else { euid };
        let new_suid = if suid == usize::MAX { old_suid } else { suid };

        // 如果没有任何改变，直接返回
        if new_ruid == old_ruid && new_euid == old_euid && new_suid == old_suid {
            return Ok(0);
        }

        // 权限检查：非特权进程只能设置为当前 ruid, euid, suid 之一
        let is_privileged = guard.euid.data() == 0;
        if !is_privileged {
            let allowed =
                |uid: usize| -> bool { uid == old_ruid || uid == old_euid || uid == old_suid };
            if (ruid != usize::MAX && !allowed(new_ruid))
                || (euid != usize::MAX && !allowed(new_euid))
                || (suid != usize::MAX && !allowed(new_suid))
            {
                return Err(SystemError::EPERM);
            }
        }

        let mut new_cred = (**guard).clone();

        // 设置新的 UID 值
        if ruid != usize::MAX {
            new_cred.setuid(new_ruid);
        }
        if euid != usize::MAX {
            new_cred.seteuid(new_euid);
        }
        if suid != usize::MAX {
            new_cred.setsuid(new_suid);
        }

        // 根据 capabilities(7) 手册，处理 capability 的丢弃：
        // 1. 如果 euid 从 0 变为非 0，清除 effective capabilities
        // 2. 如果 {ruid, euid, suid} 都从至少有一个 0 变为全部非 0，
        //    清除 permitted, effective 和 ambient capabilities
        let old_has_root = old_ruid == 0 || old_euid == 0 || old_suid == 0;
        let new_has_root = new_ruid == 0 || new_euid == 0 || new_suid == 0;

        // 规则 1: euid 从 root 变为非 root，清除 effective
        if old_euid == 0 && new_euid != 0 {
            new_cred.cap_effective = CAPFlags::CAP_EMPTY_SET;
        }

        // 规则 2: 所有 UID 都从 root 变为非 root，清除 permitted, effective, ambient
        if old_has_root && !new_has_root {
            new_cred.cap_permitted = CAPFlags::CAP_EMPTY_SET;
            new_cred.cap_effective = CAPFlags::CAP_EMPTY_SET;
            new_cred.cap_ambient = CAPFlags::CAP_EMPTY_SET;
        }

        // fsuid 跟随 euid
        if euid != usize::MAX {
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
