use crate::alloc::vec::Vec;
use crate::arch::interrupt::TrapFrame;
use crate::syscall::table::FormattedSyscallParam;
use crate::{
    arch::syscall::nr::SYS_SEMCTL,
    ipc::sem::{PosixSemIdDs, PosixSemInfo, SemCtlCmd, SemId},
    process::ProcessManager,
    syscall::table::Syscall,
    syscall::user_access::{UserBufferReader, UserBufferWriter},
};
use syscall_table_macros::declare_syscall;
use system_error::SystemError;

pub struct SysSemctlHandle;

/// # SYS_SEMCTL 系统调用：控制信号量集合
///
/// ## 参数
///
/// - `semid`: 信号量集合 id
/// - `semnum`: 信号量索引（部分命令使用）
/// - `cmd`: 操作码
/// - `arg`: union semun 的地址（SETVAL 时直接为值）
///
/// ## 返回值
///
/// 成功：命令相关返回值（GETVAL 等返回查询值，其余返回 0）
/// 失败：错误码
pub(super) fn do_kernel_semctl(
    semid: SemId,
    semnum: usize,
    cmd: SemCtlCmd,
    arg: usize,
    from_user: bool,
) -> Result<usize, SystemError> {
    let ipcns = ProcessManager::current_ipcns();

    match cmd {
        // 查看信号量系统信息
        SemCtlCmd::IpcInfo | SemCtlCmd::SemInfo => {
            let (ret, sem_info) = {
                let guard = ipcns.sem.lock();
                guard.sem_info_data()
            };
            let mut user_buffer_writer = UserBufferWriter::new(
                arg as *mut u8,
                core::mem::size_of::<PosixSemInfo>(),
                from_user,
            )?;
            user_buffer_writer.copy_one_to_user(&sem_info, 0)?;
            Ok(ret)
        }
        // 查看 id 对应的信号量集合信息
        SemCtlCmd::IpcStat | SemCtlCmd::SemStat | SemCtlCmd::SemStatAny => {
            let (ret, sem_id_ds) = {
                let guard = ipcns.sem.lock();
                guard.sem_stat_data(semid, semnum, cmd)?
            };
            let mut user_buffer_writer = UserBufferWriter::new(
                arg as *mut u8,
                core::mem::size_of::<PosixSemIdDs>(),
                from_user,
            )?;
            user_buffer_writer.copy_one_to_user(&sem_id_ds, 0)?;
            Ok(ret)
        }
        // 设置权限
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
        // 删除信号量集合
        SemCtlCmd::IpcRmid => {
            let mut guard = ipcns.sem.lock();
            guard.ipc_rmid(semid)?;
            Ok(0)
        }
        // 单信号量查询
        SemCtlCmd::GetVal | SemCtlCmd::GetPid | SemCtlCmd::GetNcnt | SemCtlCmd::GetZcnt => {
            let guard = ipcns.sem.lock();
            guard.sem_get_value(semid, semnum, cmd)
        }
        // 设置单个信号量的值（arg 直接为数值，非指针）
        SemCtlCmd::SetVal => {
            let val = arg as u32 as i32;
            let mut guard = ipcns.sem.lock();
            guard.setval(semid, semnum, val)?;
            Ok(0)
        }
        // 获取集合内所有信号量的值
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
        // 设置集合内所有信号量的值
        SemCtlCmd::SetAll => {
            let n = {
                let guard = ipcns.sem.lock();
                guard.nsems(semid)?
            };
            let mut vals: Vec<u16> = vec![0; n];
            let user_buffer_reader = UserBufferReader::new(
                arg as *const u8,
                n * core::mem::size_of::<u16>(),
                from_user,
            )?;
            for (i, v) in vals.iter_mut().enumerate() {
                user_buffer_reader.copy_one_from_user(v, i * core::mem::size_of::<u16>())?;
            }
            let mut guard = ipcns.sem.lock();
            guard.setall(semid, &vals)?;
            Ok(0)
        }
        // 无效操作码
        SemCtlCmd::Default => Err(SystemError::EINVAL),
    }
}

impl SysSemctlHandle {
    #[inline(always)]
    fn semid(args: &[usize]) -> SemId {
        SemId::new(args[0])
    }

    #[inline(always)]
    fn semnum(args: &[usize]) -> usize {
        args[1]
    }

    #[inline(always)]
    fn cmd(args: &[usize]) -> SemCtlCmd {
        SemCtlCmd::from(args[2])
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
        if args[0] > i32::MAX as usize || args[2] > i32::MAX as usize {
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
