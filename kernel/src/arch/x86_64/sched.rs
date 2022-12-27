use crate::include::bindings::bindings::{enter_syscall_int, SYS_SCHED};

/// @brief 若内核代码不处在中断上下文中，那么将可以使用本函数，发起一个sys_sched系统调用，然后运行调度器。
#[no_mangle]
pub extern "C" fn schedule_immediately() {
    unsafe {
        enter_syscall_int(SYS_SCHED.into(), 0, 0, 0, 0, 0, 0, 0, 0);
    }
}
