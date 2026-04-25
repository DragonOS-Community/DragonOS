use core::sync::atomic::{AtomicBool, Ordering};

use alloc::vec::Vec;
use log::warn;
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_FLOCK},
    filesystem::vfs::{
        file::FileMode,
        flock::{apply_flock, FlockOperation},
    },
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
};

const LOCK_SH: u32 = 1;
const LOCK_EX: u32 = 2;
const LOCK_NB: u32 = 4;
const LOCK_UN: u32 = 8;
const LOCK_MAND: u32 = 32;

static WARNED_LOCK_MAND: AtomicBool = AtomicBool::new(false);

pub struct SysFlockHandle;

impl Syscall for SysFlockHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = args[0] as i32;
        let cmd = args[1] as u32;

        if (cmd & LOCK_MAND) != 0 {
            if !WARNED_LOCK_MAND.swap(true, Ordering::Relaxed) {
                warn!(
                    "flock: LOCK_MAND support has been removed; request ignored (Linux compatible)"
                );
            }
            return Ok(0);
        }

        let (operation, nonblocking) = parse_flock_cmd(cmd)?;

        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        let file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;
        drop(fd_table_guard);

        if operation != FlockOperation::Unlock
            && !file
                .mode()
                .intersects(FileMode::FMODE_READ | FileMode::FMODE_WRITE)
        {
            return Err(SystemError::EBADF);
        }

        apply_flock(&file, operation, nonblocking)?;
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", format!("{:#x}", args[0] as i32)),
            FormattedSyscallParam::new("cmd", format!("{:#x}", args[1] as u32)),
        ]
    }
}

fn parse_flock_cmd(mut cmd: u32) -> Result<(FlockOperation, bool), SystemError> {
    let nonblocking = (cmd & LOCK_NB) != 0;
    cmd &= !LOCK_NB;

    let operation = match cmd {
        LOCK_SH => FlockOperation::Shared,
        LOCK_EX => FlockOperation::Exclusive,
        LOCK_UN => FlockOperation::Unlock,
        _ => return Err(SystemError::EINVAL),
    };

    Ok((operation, nonblocking))
}

syscall_table_macros::declare_syscall!(SYS_FLOCK, SysFlockHandle);
