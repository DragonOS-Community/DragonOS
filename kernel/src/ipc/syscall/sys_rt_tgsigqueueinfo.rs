use alloc::string::ToString;
use alloc::vec::Vec;
use core::ffi::c_int;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_RT_TGSIGQUEUEINFO;
use crate::ipc::syscall::sys_tkill::do_tkill;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use system_error::SystemError;

/// rt_tgsigqueueinfo 系统调用（最小兼容实现）
///
/// 语义上与 tgkill(tgid, tid, sig) 相近，这里直接复用 do_tkill 的
/// 线程查找与权限检查逻辑，忽略用户态 siginfo 内容。
struct SysRtTgsigqueueinfoHandle;

impl SysRtTgsigqueueinfoHandle {
    #[inline(always)]
    fn tgid(args: &[usize]) -> i32 {
        args[0] as i32
    }

    #[inline(always)]
    fn tid(args: &[usize]) -> i32 {
        args[1] as i32
    }

    #[inline(always)]
    fn sig(args: &[usize]) -> c_int {
        args[2] as c_int
    }

    #[inline(always)]
    fn _uinfo(args: &[usize]) -> usize {
        args[3]
    }
}

impl Syscall for SysRtTgsigqueueinfoHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let tgid = Self::tgid(args);
        let tid = Self::tid(args);
        let sig = Self::sig(args);
        let _uinfo = Self::_uinfo(args);

        if tgid <= 0 || tid <= 0 {
            return Err(SystemError::EINVAL);
        }

        // 直接复用 do_tkill 的线程定位与权限检查逻辑。
        do_tkill(tgid, tid, sig)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("tgid", Self::tgid(args).to_string()),
            FormattedSyscallParam::new("tid", Self::tid(args).to_string()),
            FormattedSyscallParam::new("sig", Self::sig(args).to_string()),
            FormattedSyscallParam::new("uinfo", format!("{:#x}", Self::_uinfo(args))),
        ]
    }
}

// 注册系统调用
syscall_table_macros::declare_syscall!(SYS_RT_TGSIGQUEUEINFO, SysRtTgsigqueueinfoHandle);
