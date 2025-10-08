use alloc::string::ToString;
use alloc::vec::Vec;
use core::ffi::c_int;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_TGKILL;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use system_error::SystemError;

use crate::ipc::syscall::sys_tkill::do_tkill;

/// tgkill系统调用处理器
///
/// 重构后使用统一的do_tkill核心函数，确保与tkill行为一致
pub struct SysTgkillHandle;

impl SysTgkillHandle {
    #[inline(always)]
    fn tgid(args: &[usize]) -> i32 {
        args[0] as i32
    }

    #[inline(always)]
    fn pid(args: &[usize]) -> i32 {
        args[1] as i32
    }

    #[inline(always)]
    fn sig(args: &[usize]) -> c_int {
        args[2] as c_int
    }
}

impl Syscall for SysTgkillHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let tgid = Self::tgid(args);
        let pid = Self::pid(args);
        let sig = Self::sig(args);

        // 参数合法性检查
        if pid <= 0 || tgid <= 0 {
            return Err(SystemError::EINVAL);
        }

        // 调用通用实现，验证线程组归属
        do_tkill(tgid, pid, sig)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("tgid", Self::tgid(args).to_string()),
            FormattedSyscallParam::new("pid", Self::pid(args).to_string()),
            FormattedSyscallParam::new("sig", Self::sig(args).to_string()),
        ]
    }
}

// 注册系统调用
syscall_table_macros::declare_syscall!(SYS_TGKILL, SysTgkillHandle);
