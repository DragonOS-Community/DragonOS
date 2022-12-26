use crate::{include::bindings::bindings::pt_regs, arch::asm::current::current_pcb};

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum PidType {
    /// pid类型是进程id
    PID = 1,
    TGID = 2,
    PGID = 3,
    SID = 4,
    MAX = 5,
}

/// 为PidType实现判断相等的trait
impl PartialEq for PidType {
    fn eq(&self, other: &PidType) -> bool {
        *self as u8 == *other as u8
    }
}

/**
 * @brief 获取当前进程的pid
 */
#[no_mangle]
pub extern "C" fn sys_getpid(_regs: &pt_regs)->u64{
    return current_pcb().pid as u64;
}