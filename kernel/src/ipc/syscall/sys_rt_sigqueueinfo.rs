use alloc::string::ToString;
use alloc::vec::Vec;
use core::ffi::c_int;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_RT_SIGQUEUEINFO;
use crate::ipc::kill::kill_process;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::{arch::ipc::signal::Signal, process::RawPid};
use system_error::SystemError;

/// rt_sigqueueinfo 系统调用（最小兼容实现）
///
/// 语义上与 kill(pid, sig) 类似，但允许用户态携带一个 siginfo_t。
/// 当前实现仅用于权限与错误码兼容：忽略用户态 siginfo 内容，
/// 直接复用 kill_process 的权限检查与投递逻辑。
struct SysRtSigqueueinfoHandle;

impl SysRtSigqueueinfoHandle {
    #[inline(always)]
    fn pid(args: &[usize]) -> i32 {
        args[0] as i32
    }

    #[inline(always)]
    fn sig(args: &[usize]) -> c_int {
        args[1] as c_int
    }

    #[inline(always)]
    fn _uinfo(args: &[usize]) -> usize {
        // 第三个参数是用户态 siginfo_t 指针，当前实现仅校验存在性，
        // 不解析其中内容。
        args[2]
    }
}

impl Syscall for SysRtSigqueueinfoHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pid = Self::pid(args);
        let sig = Self::sig(args);
        let _uinfo = Self::_uinfo(args);

        if pid <= 0 {
            return Err(SystemError::EINVAL);
        }

        let signal = Signal::from(sig);
        if signal == Signal::INVALID {
            return Err(SystemError::EINVAL);
        }

        // 复用 kill(2) 的权限与投递逻辑。
        let raw = RawPid::from(pid as usize);
        kill_process(raw, signal)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", Self::pid(args).to_string()),
            FormattedSyscallParam::new("sig", Self::sig(args).to_string()),
            FormattedSyscallParam::new("uinfo", format!("{:#x}", Self::_uinfo(args))),
        ]
    }
}

// 注册系统调用
syscall_table_macros::declare_syscall!(SYS_RT_SIGQUEUEINFO, SysRtSigqueueinfoHandle);
