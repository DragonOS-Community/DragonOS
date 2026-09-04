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

impl SysSemtimedopHandle {
    #[inline(always)]
    fn semid(args: &[usize]) -> SemId {
        SemId::new(args[0])
    }

    #[inline(always)]
    fn sops(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }

    #[inline(always)]
    fn nsops(args: &[usize]) -> usize {
        args[2]
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

        // Match Linux: after copying a non-null timeout, validate nsops before allocating sops.
        if nsops == 0 {
            return Err(SystemError::EINVAL);
        }
        if nsops > crate::ipc::sem::SEMOPM {
            return Err(SystemError::E2BIG);
        }

        let mut sops: Vec<PosixSemBuf> = vec![
            PosixSemBuf {
                sem_num: 0,
                sem_op: 0,
                sem_flg: 0,
            };
            nsops
        ];
        let sops_reader = UserBufferReader::new(
            sops_ptr,
            core::mem::size_of::<PosixSemBuf>() * nsops,
            from_user,
        )?;
        for (i, op) in sops.iter_mut().enumerate() {
            sops_reader.copy_one_from_user(op, i * core::mem::size_of::<PosixSemBuf>())?;
        }

        do_kernel_semtimedop(semid, &sops, timeout)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("semid", format!("{}", Self::semid(args).data())),
            FormattedSyscallParam::new("sops", format!("{:#x}", Self::sops(args) as usize)),
            FormattedSyscallParam::new("nsops", format!("{}", Self::nsops(args))),
            FormattedSyscallParam::new("timeout", format!("{:#x}", Self::timeout(args) as usize)),
        ]
    }
}

declare_syscall!(SYS_SEMTIMEDOP, SysSemtimedopHandle);
