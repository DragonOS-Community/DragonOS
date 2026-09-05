use crate::alloc::vec::Vec;
use crate::arch::interrupt::TrapFrame;
use crate::ipc::sem::SemManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::{
    arch::syscall::nr::SYS_SEMTIMEDOP,
    ipc::sem::{PosixSemBuf, SemId},
    process::ProcessManager,
    syscall::table::Syscall,
    syscall::user_access::UserBufferReader,
    time::{Duration, PosixTimeSpec},
};
use syscall_table_macros::declare_syscall;
use system_error::SystemError;

pub struct SysSemtimedopHandle;

/// # SYS_SEMTIMEDOP syscall: atomically execute a group of semaphore operations with an
/// optional timeout
///
/// ## Parameters
///
/// - `semid`: semaphore set ID
/// - `sops`: userspace `sembuf` array pointer
/// - `nsops`: number of operations
/// - `timeout`: pointer to `struct timespec`; NULL waits indefinitely
///
/// ## Return value
///
/// On success: 0.
/// On failure: error code (EAGAIN on timeout, EINTR on signal interruption, etc.)
pub(super) fn do_kernel_semtimedop(
    semid: SemId,
    sops: &[PosixSemBuf],
    timeout: Option<PosixTimeSpec>,
) -> Result<usize, SystemError> {
    let timeout = match timeout {
        None => None,
        Some(ts) => {
            if !ts.is_valid_timeout() {
                return Err(SystemError::EINVAL);
            }
            if ts.tv_sec == 0 && ts.tv_nsec == 0 {
                Some(Duration::ZERO)
            } else {
                let micros = ts.to_ktime_ns().div_ceil(1000);
                Some(Duration::from_micros(micros))
            }
        }
    };

    let ipcns = ProcessManager::current_ipcns();
    SemManager::semtimedop(&ipcns, semid, sops, timeout)
}

// Match Linux do_semtimedop: validate count and copy sops before rejecting
// a negative semid or validating the already-copied timeout value.
pub(super) fn do_user_semtimedop(
    semid: i32,
    sops_ptr: *const u8,
    nsops: usize,
    timeout: Option<PosixTimeSpec>,
    from_user: bool,
) -> Result<usize, SystemError> {
    if nsops > crate::ipc::sem::SEMOPM {
        return Err(SystemError::E2BIG);
    }
    if nsops == 0 {
        return Err(SystemError::EINVAL);
    }
    let mut sops = Vec::new();
    sops.try_reserve_exact(nsops)
        .map_err(|_| SystemError::ENOMEM)?;
    sops.resize(
        nsops,
        PosixSemBuf {
            sem_num: 0,
            sem_op: 0,
            sem_flg: 0,
        },
    );
    let reader = UserBufferReader::new(
        sops_ptr,
        core::mem::size_of::<PosixSemBuf>() * nsops,
        from_user,
    )?;
    for (i, op) in sops.iter_mut().enumerate() {
        reader.copy_one_from_user(op, i * core::mem::size_of::<PosixSemBuf>())?;
    }
    if semid < 0 {
        return Err(SystemError::EINVAL);
    }
    do_kernel_semtimedop(SemId::new(semid as usize), &sops, timeout)
}

impl SysSemtimedopHandle {
    #[inline(always)]
    fn semid(args: &[usize]) -> i32 {
        args[0] as i32
    }

    #[inline(always)]
    fn sops(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }

    #[inline(always)]
    fn nsops(args: &[usize]) -> usize {
        args[2] as u32 as usize
    }

    #[inline(always)]
    fn timeout(args: &[usize]) -> *const u8 {
        args[3] as *const u8
    }
}

impl Syscall for SysSemtimedopHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let semid = Self::semid(args);
        let nsops = Self::nsops(args);
        let sops_ptr = Self::sops(args);
        let timeout_ptr = Self::timeout(args);
        let from_user = frame.is_from_user();

        let timeout = if timeout_ptr.is_null() {
            None
        } else {
            let mut ts = PosixTimeSpec::new(0, 0);
            let ts_reader = UserBufferReader::new(
                timeout_ptr,
                core::mem::size_of::<PosixTimeSpec>(),
                from_user,
            )?;
            ts_reader.copy_one_from_user(&mut ts, 0)?;
            Some(ts)
        };

        do_user_semtimedop(semid, sops_ptr, nsops, timeout, from_user)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("semid", format!("{}", Self::semid(args))),
            FormattedSyscallParam::new("sops", format!("{:#x}", Self::sops(args) as usize)),
            FormattedSyscallParam::new("nsops", format!("{}", Self::nsops(args))),
            FormattedSyscallParam::new("timeout", format!("{:#x}", Self::timeout(args) as usize)),
        ]
    }
}

declare_syscall!(SYS_SEMTIMEDOP, SysSemtimedopHandle);
