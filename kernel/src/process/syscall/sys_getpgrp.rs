use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_GETPGRP;
use crate::process::RawPid;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

use super::sys_getpgid::do_getpgid;

pub struct SysGetPgrp;

impl Syscall for SysGetPgrp {
    fn num_args(&self) -> usize {
        1
    }

    /// # SYS_GETPGRP
    ///
    /// 实现 `getpgrp` 系统调用，用于获取当前进程的进程组 ID (PGID)。
    ///
    /// 根据 POSIX 标准，`getpgrp` 不需要任何参数，直接返回调用者的进程组 ID。
    /// 在实现中，我们通过调用 `do_getpgid` 函数，并传入 `RawPid(0)` 来获取当前进程的 PGID。
    ///
    /// ## 参数
    /// - `_args`: 系统调用的参数列表。对于 `getpgrp`，此参数未被使用，因为该系统调用不需要额外参数。
    /// - `_frame`: 当前中断帧，包含系统调用的上下文信息。在此函数中未被直接使用。
    ///
    /// ## 返回值
    /// - 成功时，返回当前进程的进程组 ID (PGID)，类型为 `usize`。
    /// - 如果发生错误（例如无法获取 PGID），返回 `SystemError` 错误码。
    fn handle(&self, _args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        return do_getpgid(RawPid(0));
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETPGRP, SysGetPgrp);
