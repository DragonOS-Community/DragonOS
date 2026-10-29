use crate::alloc::vec::Vec;
use crate::arch::interrupt::TrapFrame;
use crate::syscall::table::FormattedSyscallParam;
use crate::{
    arch::syscall::nr::SYS_SEMCTL,
    ipc::sem::{PosixSemIdDs, PosixSemInfo, SemCtlCmd, SemId, SemWakeBatch},
    process::ProcessManager,
    syscall::table::Syscall,
    syscall::user_access::{UserBufferReader, UserBufferWriter},
};
use syscall_table_macros::declare_syscall;
use system_error::SystemError;

pub struct SysSemctlHandle;

/// # SYS_SEMCTL syscall: control a semaphore set
///
/// ## Parameters
///
/// - `semid`: semaphore set ID
/// - `semnum`: semaphore index (used by some commands)
/// - `cmd`: command
/// - `arg`: address of `union semun` (the value itself for SETVAL)
///
/// ## Return value
///
/// On success: command-specific result (query commands such as GETVAL return a value; all
/// others return 0).
/// On failure: error code
pub(super) fn do_kernel_semctl(
    semid: SemId,
    semnum: usize,
    cmd: SemCtlCmd,
    arg: usize,
    from_user: bool,
) -> Result<usize, SystemError> {
    let ipcns = ProcessManager::current_ipcns();
    // Declared outside all manager-guard scopes: completion notifications are
    // delivered after unlocking, including on early returns.
    let mut wakes = SemWakeBatch::default();

    match cmd {
        // Retrieve semaphore system information
        SemCtlCmd::IpcInfo | SemCtlCmd::SemInfo => {
            let (ret, sem_info) = {
                let guard = ipcns.sem.lock();
                guard.sem_info_data(cmd)
            };
            let mut user_buffer_writer = UserBufferWriter::new(
                arg as *mut u8,
                core::mem::size_of::<PosixSemInfo>(),
                from_user,
            )?;
            user_buffer_writer.copy_one_to_user(&sem_info, 0)?;
            Ok(ret)
        }
        // Retrieve information for the semaphore set identified by ID
        SemCtlCmd::IpcStat | SemCtlCmd::SemStat | SemCtlCmd::SemStatAny => {
            let (ret, sem_id_ds) = {
                let guard = ipcns.sem.lock();
                guard.sem_stat_data(semid, cmd)?
            };
            let mut user_buffer_writer = UserBufferWriter::new(
                arg as *mut u8,
                core::mem::size_of::<PosixSemIdDs>(),
                from_user,
            )?;
            user_buffer_writer.copy_one_to_user(&sem_id_ds, 0)?;
            Ok(ret)
        }
        // Set permissions
        SemCtlCmd::IpcSet => {
            let mut sem_id_ds = PosixSemIdDs::default();
            let user_buffer_reader = UserBufferReader::new(
                arg as *const u8,
                core::mem::size_of::<PosixSemIdDs>(),
                from_user,
            )?;
            user_buffer_reader.copy_one_from_user(&mut sem_id_ds, 0)?;
            let mut guard = ipcns.sem.lock();
            guard.ipc_set(semid, sem_id_ds)?;
            Ok(0)
        }
        // Remove the semaphore set
        SemCtlCmd::IpcRmid => {
            let removed = {
                let mut guard = ipcns.sem.lock();
                guard.ipc_rmid(semid, &mut wakes)?
            };
            wakes.wake_all();
            removed.reclaim_removed_undo_storage();
            drop(removed);
            Ok(0)
        }
        // Query a single semaphore
        SemCtlCmd::GetVal | SemCtlCmd::GetPid | SemCtlCmd::GetNcnt | SemCtlCmd::GetZcnt => {
            let guard = ipcns.sem.lock();
            guard.sem_get_value(semid, semnum, cmd)
        }
        // Set a single semaphore value (`arg` is a value, not a pointer)
        SemCtlCmd::SetVal => {
            let val = arg as u32 as i32;
            let mut guard = ipcns.sem.lock();
            guard.setval(semid, semnum, val, &mut wakes)?;
            Ok(0)
        }
        // Get values of all semaphores in the set
        SemCtlCmd::GetAll => {
            let vals = {
                let guard = ipcns.sem.lock();
                guard.getall(semid)?
            };
            let mut user_buffer_writer = UserBufferWriter::new(
                arg as *mut u8,
                vals.len() * core::mem::size_of::<u16>(),
                from_user,
            )?;
            for (i, v) in vals.iter().enumerate() {
                user_buffer_writer.copy_one_to_user(v, i * core::mem::size_of::<u16>())?;
            }
            Ok(0)
        }
        // Set values of all semaphores in the set
        SemCtlCmd::SetAll => {
            let token = {
                let guard = ipcns.sem.lock();
                guard.prepare_setall(semid)?
            };
            let mut vals = Vec::<u16>::new();
            vals.try_reserve_exact(token.nsems())
                .map_err(|_| SystemError::ENOMEM)?;
            vals.resize(token.nsems(), 0);
            let user_buffer_reader = UserBufferReader::new(
                arg as *const u8,
                token.nsems() * core::mem::size_of::<u16>(),
                from_user,
            )?;
            for (i, v) in vals.iter_mut().enumerate() {
                user_buffer_reader.copy_one_from_user(v, i * core::mem::size_of::<u16>())?;
            }
            let mut guard = ipcns.sem.lock();
            guard.setall(token, &vals, &mut wakes)?;
            Ok(0)
        }
        // Invalid command
        SemCtlCmd::Default => Err(SystemError::EINVAL),
    }
}

impl SysSemctlHandle {
    #[inline(always)]
    fn semid(args: &[usize]) -> SemId {
        SemId::new(args[0] as u32 as usize)
    }

    #[inline(always)]
    fn semnum(args: &[usize]) -> usize {
        args[1] as u32 as usize
    }

    #[inline(always)]
    fn cmd(args: &[usize]) -> SemCtlCmd {
        SemCtlCmd::from(args[2] as u32 as usize)
    }

    #[inline(always)]
    fn arg(args: &[usize]) -> usize {
        args[3]
    }
}

impl Syscall for SysSemctlHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        if (args[0] as i32) < 0 {
            return Err(SystemError::EINVAL);
        }
        let semid = Self::semid(args);
        let semnum = Self::semnum(args);
        let cmd = Self::cmd(args);
        let arg = Self::arg(args);
        do_kernel_semctl(semid, semnum, cmd, arg, frame.is_from_user())
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("semid", format!("{}", Self::semid(args).data())),
            FormattedSyscallParam::new("semnum", format!("{}", Self::semnum(args))),
            FormattedSyscallParam::new("cmd", format!("{}", Self::cmd(args))),
            FormattedSyscallParam::new("arg", format!("{:#x}", Self::arg(args))),
        ]
    }
}

declare_syscall!(SYS_SEMCTL, SysSemctlHandle);
