use crate::{
    include::bindings::bindings::enter_syscall_int,
    kdebug,
    process::{Pid, ProcessManager},
    syscall::SYS_SCHED,
};

/// @brief 若内核代码不处在中断上下文中，那么将可以使用本函数，发起一个sys_sched系统调用，然后运行调度器。
/// 由于只能在中断上下文中进行进程切换，因此需要发起一个系统调用SYS_SCHED。
#[no_mangle]
pub extern "C" fn sched() {
    unsafe {
        enter_syscall_int(SYS_SCHED as u64, 0, 0, 0, 0, 0, 0, 0, 0);
    }
}
