#![allow(unused)]

use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::cell::OnceCell;
use core::fmt::Display;

use crate::libs::once::Once;
use crate::syscall::SystemError;

/// 定义Syscall trait
pub trait Syscall: Send + Sync + 'static {
    /// 系统调用参数数量
    fn num_args(&self) -> usize;
    fn handle(&self, args: &[usize], from_user: bool) -> Result<usize, SystemError>;

    /// Formats the system call parameters for display/debug purposes
    ///
    /// # Arguments
    /// * `args` - System call arguments to format
    ///
    /// # Returns
    /// Vector of formatted parameters with their names and values
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam>;
}

pub struct FormattedSyscallParam {
    pub name: &'static str,
    pub value: String,
}

impl FormattedSyscallParam {
    pub fn new(name: &'static str, value: String) -> Self {
        Self { name, value }
    }
}

impl Display for FormattedSyscallParam {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.name, self.value)
    }
}

/// 系统调用处理句柄
#[repr(C)]
pub struct SyscallHandle {
    pub nr: usize,
    pub inner_handle: &'static dyn Syscall,
    pub name: &'static str,
}

impl SyscallHandle {
    #[inline(never)]
    pub fn args_string(&self, args: &[usize]) -> String {
        let args_slice = self.inner_handle.entry_format(args);
        args_slice
            .iter()
            .map(|p| format!("{}", p))
            .collect::<Vec<String>>()
            .join(", ")
    }
}

/// 系统调用表类型
#[repr(C)]
pub struct SyscallTable {
    entries: [Option<&'static SyscallHandle>; Self::ENTRIES],
}

impl SyscallTable {
    pub const ENTRIES: usize = 512;
    /// 获取系统调用处理函数
    pub fn get(&self, nr: usize) -> Option<&'static SyscallHandle> {
        *self.entries.get(nr)?
    }
}

// 声明外部链接的syscall_table符号
extern "C" {
    fn _syscall_table();
    fn _esyscall_table();
}

/// 全局系统调用表实例
#[used]
#[link_section = ".data"]
static mut SYS_CALL_TABLE: SyscallTable = SyscallTable {
    entries: [None; SyscallTable::ENTRIES],
};

#[inline]
pub(super) fn syscall_table() -> &'static SyscallTable {
    unsafe { &SYS_CALL_TABLE }
}

/// 初始化系统调用表
#[inline(never)]
pub(super) fn syscall_table_init() -> Result<(), SystemError> {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        log::debug!("Initializing syscall table...");

        // 初始化系统调用表
        unsafe {
            let start = _syscall_table as usize;
            let end = _esyscall_table as usize;
            let size = end - start;
            let count = size / core::mem::size_of::<SyscallHandle>();

            if size % core::mem::size_of::<SyscallHandle>() != 0 {
                panic!("Invalid syscall table size: {}", size);
            }

            let handles =
                core::slice::from_raw_parts(_syscall_table as *const SyscallHandle, count);
            for handle in handles {
                if handle.nr < SyscallTable::ENTRIES {
                    SYS_CALL_TABLE.entries[handle.nr] = Some(handle);
                } else {
                    panic!("Invalid syscall number: {}", handle.nr);
                }
            }

            log::debug!("Syscall table (count: {count}) initialized successfully.")
        }
    });
    Ok(())
}
