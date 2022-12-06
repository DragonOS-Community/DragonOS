use crate::{
    include::bindings::bindings::{atomic_t, spinlock_t},
    ipc::signal::DEFAULT_SIGACTION,
};

use crate::ipc::signal_types::{sighand_struct, signal_struct, MAX_SIG_NUM};

/// @brief 初始进程的signal结构体
#[no_mangle]
pub static INITIAL_SIGNALS: signal_struct = signal_struct {
    sig_cnt: atomic_t { value: 0 },
};

/// @brief 初始进程的sighand结构体
#[no_mangle]
pub static mut INITIAL_SIGHAND: sighand_struct = sighand_struct {
    count: REFCOUNT_INIT!(1),
    siglock: spinlock_t { lock: 1 },
    action: [DEFAULT_SIGACTION; MAX_SIG_NUM as usize],
};
