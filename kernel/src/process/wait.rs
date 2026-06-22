use alloc::sync::Arc;
use system_error::SystemError;

use super::{pid::Pid, ProcessManager, RawPid};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitSelector {
    Any,
    Pid(Arc<Pid>),
    Pgid(Option<Arc<Pid>>),
}

impl WaitSelector {
    pub fn from_wait4_pid(pid: i32) -> Result<Self, SystemError> {
        if pid == i32::MIN {
            return Err(SystemError::ESRCH);
        }

        if pid < -1 {
            Ok(Self::Pgid(ProcessManager::find_vpid(RawPid::from(
                -pid as usize,
            ))))
        } else if pid == -1 {
            Ok(Self::Any)
        } else if pid == 0 {
            Ok(Self::Pgid(Some(
                ProcessManager::current_pcb()
                    .task_pgrp()
                    .ok_or(SystemError::ECHILD)?,
            )))
        } else {
            let pid =
                ProcessManager::find_vpid(RawPid::from(pid as usize)).ok_or(SystemError::ECHILD)?;
            Ok(Self::Pid(pid))
        }
    }

    pub fn from_waitid(which: u32, upid: i32) -> Result<Self, SystemError> {
        match which {
            // P_ALL
            0 => Ok(Self::Any),
            // P_PID
            1 => {
                if upid <= 0 {
                    return Err(SystemError::EINVAL);
                }
                let pid = ProcessManager::find_vpid(RawPid::from(upid as usize))
                    .ok_or(SystemError::ECHILD)?;
                Ok(Self::Pid(pid))
            }
            // P_PGID
            2 => {
                if upid < 0 {
                    return Err(SystemError::EINVAL);
                }
                if upid == 0 {
                    Ok(Self::Pgid(Some(
                        ProcessManager::current_pcb()
                            .task_pgrp()
                            .ok_or(SystemError::ECHILD)?,
                    )))
                } else {
                    Ok(Self::Pgid(ProcessManager::find_vpid(RawPid::new(
                        upid as usize,
                    ))))
                }
            }
            // P_PIDFD is a waitid-specific selector. DragonOS has pidfd basics,
            // but pidfd wait still needs fd validation and O_NONBLOCK/EAGAIN
            // semantics, so keep the unsupported boundary explicit here after
            // preserving Linux's invalid negative-fd boundary.
            3 => {
                if upid < 0 {
                    return Err(SystemError::EINVAL);
                }
                Err(SystemError::ENOSYS)
            }
            _ => Err(SystemError::EINVAL),
        }
    }
}
