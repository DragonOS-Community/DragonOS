use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ffi::c_int;

use crate::arch::interrupt::TrapFrame;
use crate::process::pid::Pid;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::{
    arch::{ipc::signal::Signal, syscall::nr::SYS_KILL},
    process::{ProcessManager, RawPid},
};
use log::warn;
use system_error::SystemError;

use crate::ipc::kill::{kill_all, kill_process, kill_process_group};

/// ### pid转换器，将输入的id转换成对应的pid或pgid
/// - 如果id < -1，则为pgid
/// - 如果id == -1，则为所有进程
/// - 如果id == 0，则为当前进程组
/// - 如果id > 0，则为pid
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PidConverter {
    All,
    Pid(Arc<Pid>),
    Pgid(Option<Arc<Pid>>),
}

impl PidConverter {
    /// ### 为 `wait` 和 `kill` 调用使用
    pub fn from_id(id: i32) -> Option<Self> {
        if id < -1 {
            let pgid = ProcessManager::find_vpid(RawPid::from(-id as usize));
            Some(PidConverter::Pgid(pgid))
        } else if id == -1 {
            Some(PidConverter::All)
        } else if id == 0 {
            let pgid = ProcessManager::current_pcb().task_pgrp().unwrap();
            Some(PidConverter::Pgid(Some(pgid)))
        } else {
            let pid = ProcessManager::find_vpid(RawPid::from(id as usize))?;
            Some(PidConverter::Pid(pid))
        }
    }
}

pub struct SysKillHandle;

impl SysKillHandle {
    #[inline(always)]
    fn pid(args: &[usize]) -> i32 {
        // 第一个参数是id
        args[0] as i32
    }
    #[inline(always)]
    fn sig(args: &[usize]) -> c_int {
        // 第二个参数是信号值
        args[1] as c_int
    }
}

impl Syscall for SysKillHandle {
    fn num_args(&self) -> usize {
        2
    }
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let id = Self::pid(args);
        let sig_c_int = Self::sig(args);

        let converter = PidConverter::from_id(id).ok_or(SystemError::ESRCH)?;

        let sig = Signal::from(sig_c_int);
        if sig == Signal::INVALID {
            warn!(
                "Failed to convert signal number {} to Signal enum",
                sig_c_int
            );
            return Err(SystemError::EINVAL);
        }

        match converter {
            PidConverter::Pid(pid) => kill_process(pid.pid_vnr(), sig),
            PidConverter::Pgid(pgid) => kill_process_group(&pgid.ok_or(SystemError::ESRCH)?, sig),
            PidConverter::All => kill_all(sig),
        }
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", Self::pid(args).to_string()),
            FormattedSyscallParam::new("sig", Self::sig(args).to_string()),
        ]
    }
}

// 注册系统调用
syscall_table_macros::declare_syscall!(SYS_KILL, SysKillHandle);
