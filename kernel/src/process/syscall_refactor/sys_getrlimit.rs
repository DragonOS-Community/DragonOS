use system_error::SystemError;

use crate::arch::syscall::nr::SYS_GETRLIMIT;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::{
    arch::MMArch,
    filesystem::vfs::file::FileDescriptorVec,
    mm::{ucontext::UserStack, MemoryManagementArch},
    process::{
        resource::{RLimit64, RLimitID},
        Pid,
    },
    syscall::user_access::UserBufferWriter,
};
use alloc::vec::Vec;

pub struct SysGetRlimit;

impl SysGetRlimit {
    fn resource(args: &[usize]) -> usize {
        args[0]
    }

    fn rlimit(args: &[usize]) -> *mut RLimit64 {
        args[1] as *mut RLimit64
    }
}

impl Syscall for SysGetRlimit {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let resource = Self::resource(args);
        let rlimit = Self::rlimit(args);

        do_prlimit64(
            ProcessManager::current_pcb().pid(),
            resource,
            core::ptr::null::<RLimit64>(),
            rlimit,
        )
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("resource", format!("{:#x}", Self::resource(args))),
            FormattedSyscallParam::new("rlimit", format!("{:#x}", Self::rlimit(args) as usize)),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETRLIMIT, SysGetRlimit);

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
    _pid: Pid,
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
