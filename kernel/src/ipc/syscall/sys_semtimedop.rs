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

/// # SYS_SEMTIMEDOP 系统调用：原子执行一组信号量操作，可指定超时
///
/// ## 参数
///
/// - `semid`: 信号量集合 id
/// - `sops`: 用户态 sembuf 数组指针
/// - `nsops`: 操作数
/// - `timeout`: 指向 struct timespec 的指针，为 NULL 时无限等待
///
/// ## 返回值
///
/// 成功：0
/// 失败：错误码（EAGAIN 超时、EINTR 被信号中断等）
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
        let from_user = frame.is_from_user();

        // 与 Linux 一致：先于任何分配校验 nsops 范围
        if nsops == 0 {
            return Err(SystemError::EINVAL);
        }
        if nsops > crate::ipc::sem::SEMOPM {
            return Err(SystemError::E2BIG);
        }

        let mut sops: Vec<PosixSemBuf> = vec![PosixSemBuf {
            sem_num: 0,
            sem_op: 0,
            sem_flg: 0,
        }; nsops];
        let sops_reader = UserBufferReader::new(
            sops_ptr,
            core::mem::size_of::<PosixSemBuf>() * nsops,
            from_user,
        )?;
        for (i, op) in sops.iter_mut().enumerate() {
            sops_reader.copy_one_from_user(op, i * core::mem::size_of::<PosixSemBuf>())?;
        }

        let timeout_ptr = Self::timeout(args);
        let timeout = if timeout_ptr.is_null() {
            None
        } else {
            let mut ts = PosixTimeSpec::new(0, 0);
            let ts_reader =
                UserBufferReader::new(timeout_ptr, core::mem::size_of::<PosixTimeSpec>(), from_user)?;
            ts_reader.copy_one_from_user(&mut ts, 0)?;
            Some(ts)
        };

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
