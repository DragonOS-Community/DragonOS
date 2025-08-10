use crate::arch::syscall::nr::SYS_PRLIMIT64;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::{
    arch::MMArch,
    filesystem::vfs::file::FileDescriptorVec,
    mm::{MemoryManagementArch, ucontext::UserStack},
    process::{
        RawPid,
        resource::{RLimit64, RLimitID},
    },
    syscall::user_access::UserBufferWriter,
};
use alloc::vec::Vec;
use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
pub struct SysPrlimit64;

impl SysPrlimit64 {
    fn pid(args: &[usize]) -> RawPid {
        RawPid::new(args[0])
    }

    fn resource(args: &[usize]) -> usize {
        args[1]
    }

    fn new_limit(args: &[usize]) -> *const RLimit64 {
        args[2] as *const RLimit64
    }

    fn old_limit(args: &[usize]) -> *mut RLimit64 {
        args[3] as *mut RLimit64
    }
}

impl Syscall for SysPrlimit64 {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pid = Self::pid(args);
        let resource = Self::resource(args);
        let new_limit = Self::new_limit(args);
        let old_limit = Self::old_limit(args);

        do_prlimit64(pid, resource, new_limit, old_limit)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", format!("{:#x}", Self::pid(args).data())),
            FormattedSyscallParam::new("resource", format!("{:#x}", Self::resource(args))),
            FormattedSyscallParam::new(
                "new_limit",
                format!("{:#x}", Self::new_limit(args) as usize),
            ),
            FormattedSyscallParam::new(
                "old_limit",
                format!("{:#x}", Self::old_limit(args) as usize),
            ),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_PRLIMIT64, SysPrlimit64);

/// # 设置资源限制
///
/// TODO: 目前暂时不支持设置资源限制，只提供读取默认值的功能
///
/// ## 参数
///
/// - pid: 进程号
/// - resource: 资源类型
/// - new_limit: 新的资源限制
/// - old_limit: 旧的资源限制
///
/// ## 返回值
///
/// - 成功，0
/// - 如果old_limit不为NULL，则返回旧的资源限制到old_limit
///
pub(super) fn do_prlimit64(
    _pid: RawPid,
    resource: usize,
    _new_limit: *const RLimit64,
    old_limit: *mut RLimit64,
) -> Result<usize, SystemError> {
    let resource = RLimitID::try_from(resource)?;
    let mut writer = None;

    if !old_limit.is_null() {
        writer = Some(UserBufferWriter::new(
            old_limit,
            core::mem::size_of::<RLimit64>(),
            true,
        )?);
    }

    match resource {
        RLimitID::Stack => {
            if let Some(mut writer) = writer {
                let mut rlimit = writer.buffer::<RLimit64>(0).unwrap()[0];
                rlimit.rlim_cur = UserStack::DEFAULT_USER_STACK_SIZE as u64;
                rlimit.rlim_max = UserStack::DEFAULT_USER_STACK_SIZE as u64;
            }
            return Ok(0);
        }

        RLimitID::Nofile => {
            if let Some(mut writer) = writer {
                let mut rlimit = writer.buffer::<RLimit64>(0).unwrap()[0];
                rlimit.rlim_cur = FileDescriptorVec::PROCESS_MAX_FD as u64;
                rlimit.rlim_max = FileDescriptorVec::PROCESS_MAX_FD as u64;
            }
            return Ok(0);
        }

        RLimitID::As | RLimitID::Rss => {
            if let Some(mut writer) = writer {
                let mut rlimit = writer.buffer::<RLimit64>(0).unwrap()[0];
                rlimit.rlim_cur = MMArch::USER_END_VADDR.data() as u64;
                rlimit.rlim_max = MMArch::USER_END_VADDR.data() as u64;
            }
            return Ok(0);
        }

        _ => {
            return Err(SystemError::ENOSYS);
        }
    }
}
