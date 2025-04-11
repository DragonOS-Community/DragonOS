use system_error::SystemError;

pub mod nr;

/// 系统调用初始化
pub fn arch_syscall_init() -> Result<(), SystemError> {
    todo!("la64:arch_syscall_init");
}
