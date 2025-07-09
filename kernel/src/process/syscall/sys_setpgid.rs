use alloc::sync::Arc;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETPGID;
use crate::process::Pgid;
use crate::process::ProcessFlags;
use crate::process::ProcessManager;
use crate::process::RawPid;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;
pub struct SysSetPgid;

impl SysSetPgid {
    fn pid(args: &[usize]) -> RawPid {
        RawPid::new(args[0])
    }

    fn pgid(args: &[usize]) -> Pgid {
        Pgid::new(args[1])
    }
}

impl Syscall for SysSetPgid {
    fn num_args(&self) -> usize {
        2
    }

    /// # sys_setpgid
    /// 设置指定进程的pgid
    ///
    /// ## 参数
    /// - pid: 指定进程号
    /// - pgid: 新的进程组号
    ///
    /// https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/sys.c#1073
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let mut pid = Self::pid(args);
        let mut pgid = Self::pgid(args);
        let group_leader = ProcessManager::current_pcb()
            .threads_read_irqsave()
            .group_leader
            .clone();
        let group_leader = group_leader.upgrade().ok_or(SystemError::ESRCH)?;

        if pid.data() == 0 {
            pid = group_leader.task_pid_vnr();
        }

        if pgid.data() == 0 {
            pgid = pid;
        }

        let p = ProcessManager::find_task_by_vpid(pid).ok_or(SystemError::ESRCH)?;
        if !p.is_thread_group_leader() {
            return Err(SystemError::EINVAL);
        }
        let real_parent = p.real_parent_pcb.read().clone();
        if ProcessManager::same_thread_group(&group_leader, &real_parent) {
            if !Arc::ptr_eq(
                &p.task_session().unwrap(),
                &group_leader.task_session().unwrap(),
            ) {
                return Err(SystemError::EPERM);
            }

            if !p.flags().contains(ProcessFlags::FORKNOEXEC){
                return Err(SystemError::EACCES);
            }
        }else{
            if !Arc::ptr_eq(&p, &group_leader) {
                return Err(SystemError::ESRCH);
            }
        }

        todo!("Implement sys_setpgid logic");
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", format!("{:#x}", Self::pid(args).0)),
            FormattedSyscallParam::new("pgid", format!("{:#x}", Self::pgid(args).0)),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETPGID, SysSetPgid);
