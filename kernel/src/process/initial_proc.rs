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
    signal_fd_wqh: wait_queue_head_t {
        lock: spinlock_t { lock: 1 },
        wait_list: List {
            prev: unsafe { &INITIAL_SIGHAND.signal_fd_wqh.wait_list as *const List } as *mut List,
            next: unsafe { &INITIAL_SIGHAND.signal_fd_wqh.wait_list as *const List } as *mut List,
        },
    },
    action: [DEFAULT_SIGACTION; MAX_SIG_NUM as usize],
};
