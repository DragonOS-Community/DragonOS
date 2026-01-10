//! munlockall 系统调用实现
//!
//! # 系统调用原型
//!
//! ```c
//! int munlockall(void);
//! ```
//!
//! # 功能
//!
//! 解锁进程地址空间的所有内存页，包括：
//! - 通过 mlockall 锁定的所有页面
//! - 通过 MCL_FUTURE 标志设置的默认锁定行为
//!
//! # 返回值
//!
//! - 0: 成功
//! - -1: 失败，设置 errno
//!
//! # 注意
//!
//! - 执行后，def_flags 被清除，后续 mmap 不会自动锁定
//! - 已锁定的页面会被解锁
//! - 如果页面被多次锁定（引用计数 > 1），需要对应次数的 munlock

use crate::arch::{interrupt::TrapFrame, syscall::nr::SYS_MUNLOCKALL};
use crate::mm::ucontext::AddressSpace;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysMunlockallHandle;

impl Syscall for SysMunlockallHandle {
    fn num_args(&self) -> usize {
        0
    }

    fn handle(&self, _args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let addr_space = AddressSpace::current()?;
        addr_space.write().munlockall()?;
        Ok(0)
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![]
    }
}

syscall_table_macros::declare_syscall!(SYS_MUNLOCKALL, SysMunlockallHandle);
