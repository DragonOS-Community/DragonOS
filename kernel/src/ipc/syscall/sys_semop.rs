use crate::alloc::vec::Vec;
use crate::arch::interrupt::TrapFrame;
use crate::syscall::table::FormattedSyscallParam;
use crate::{
    arch::syscall::nr::SYS_SEMOP,
    ipc::sem::{PosixSemBuf, SemId},
    syscall::table::Syscall,
    syscall::user_access::UserBufferReader,
};
use syscall_table_macros::declare_syscall;
use system_error::SystemError;

use super::sys_semtimedop::do_kernel_semtimedop;

pub struct SysSemopHandle;

/// # SYS_SEMOP 系统调用：原子执行一组信号量操作（无限等待）
///
/// 与 SYS_SEMTIMEDOP 共享实现，timeout 为 NULL。
///
/// ## 参数
///
/// - `semid`: 信号量集合 id
/// - `sops`: 用户态 sembuf 数组指针
/// - `nsops`: 操作数
///
/// ## 返回值
///
/// 成功：0
/// 失败：错误码
impl Syscall for SysSemopHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let semid = SemId::new(args[0]);
        let nsops = args[2];
        let from_user = frame.is_from_user();

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
            args[1] as *const u8,
            core::mem::size_of::<PosixSemBuf>() * nsops,
            from_user,
        )?;
        for (i, op) in sops.iter_mut().enumerate() {
            sops_reader.copy_one_from_user(op, i * core::mem::size_of::<PosixSemBuf>())?;
        }

        do_kernel_semtimedop(semid, &sops, None)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("semid", format!("{}", args[0])),
            FormattedSyscallParam::new("sops", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("nsops", format!("{}", args[2])),
        ]
    }
}

declare_syscall!(SYS_SEMOP, SysSemopHandle);
