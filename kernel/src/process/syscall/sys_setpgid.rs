use alloc::sync::Arc;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETPGID;
use crate::process::Pgid;
use crate::process::ProcessFlags;
use crate::process::ProcessManager;
use crate::process::RawPid;
use crate::process::pid::PidType;
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

            if !p.flags().contains(ProcessFlags::FORKNOEXEC) {
                return Err(SystemError::EACCES);
            }
        } else if !Arc::ptr_eq(&p, &group_leader) {
            return Err(SystemError::ESRCH);
        }

        if p.sig_info_irqsave().is_session_leader {
            return Err(SystemError::EPERM);
        }
        let mut pgrp = p.pid();
        if pgid != pid {
            pgrp = ProcessManager::find_vpid(pgid).ok_or(SystemError::EPERM)?;
            let g = pgrp.pid_task(PidType::PGID).ok_or(SystemError::EPERM)?;
            let s1 = g.task_session();
            let s2 = group_leader.task_session();
            // 模拟C的 task_session(g) != task_session(group_leader) 判断
            // 1. 如果两个会话都是None，视为相等
            // 2. 如果只有一个为None，视为不等
            // 3. 如果两个都有值，比较内部Pid
            match (s1, s2) {
                (None, None) => (), // 都为空，允许
                (Some(_), None) | (None, Some(_)) => {
                    return Err(SystemError::EPERM);
                }
                (Some(session1), Some(session2)) if !Arc::ptr_eq(&session1, &session2) => {
                    return Err(SystemError::EPERM);
                }
                _ => (), // 会话相同，继续
            }
        }

        let pp = p.task_pgrp().unwrap();

        if !Arc::ptr_eq(&pp, &pgrp) {
            p.change_pid(PidType::PGID, pgrp);
        }

        return Ok(0);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", format!("{:#x}", Self::pid(args).0)),
            FormattedSyscallParam::new("pgid", format!("{:#x}", Self::pgid(args).0)),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETPGID, SysSetPgid);
