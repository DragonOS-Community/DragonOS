use crate::{
    include::{
        bindings::bindings::{atomic_t, spinlock_t, wait_queue_head_t, List},
        DragonOS::signal::{sighand_struct, signal_struct, MAX_SIG_NUM},
    },
    ipc::signal::DEFAULT_SIGACTION,
};

#[no_mangle]
pub static INITIAL_SIGNALS: signal_struct = signal_struct {
    sig_cnt: atomic_t { value: 0 },
};

#[no_mangle]
pub static mut INITIAL_SIGHAND: sighand_struct = sighand_struct {
    count: REFCOUNT_INIT!(1),
    siglock: spinlock_t { lock: 1 },
    action: [DEFAULT_SIGACTION; MAX_SIG_NUM as usize],
};
