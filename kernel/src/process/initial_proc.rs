use core::ffi::c_void;

use alloc::boxed::Box;

use crate::{
    include::bindings::bindings::{atomic_t, process_control_block, spinlock_t},
    ipc::{signal::DEFAULT_SIGACTION, signal_types::SigQueue},
};

use crate::ipc::signal_types::{sighand_struct, signal_struct, MAX_SIG_NUM};

/// @brief 初始进程的signal结构体
#[no_mangle]
pub static mut INITIAL_SIGNALS: signal_struct = signal_struct {
    sig_cnt: atomic_t { value: 0 },
};

/// @brief 初始进程的sighand结构体
#[no_mangle]
pub static mut INITIAL_SIGHAND: sighand_struct = sighand_struct {
    count: REFCOUNT_INIT!(1),
    siglock: spinlock_t { lock: 1 },
    action: [DEFAULT_SIGACTION; MAX_SIG_NUM as usize],
};

/// @brief 初始化pid=0的进程的信号相关的信息
#[no_mangle]
pub extern "C" fn initial_proc_init_signal(pcb: *mut process_control_block) {
    
    // 所设置的pcb的pid一定为0
    assert_eq!(unsafe { (*pcb).pid }, 0);
    
    // 设置init进程的sighand和signal
    unsafe {
        (*pcb).sighand = &mut INITIAL_SIGHAND as *mut sighand_struct as usize
            as *mut crate::include::bindings::bindings::sighand_struct;
        (*pcb).signal = &mut INITIAL_SIGNALS as *mut signal_struct as usize
            as *mut crate::include::bindings::bindings::signal_struct;
    }
    // 创建新的sig_pending->sigqueue
    unsafe {
        (*pcb).sig_pending.signal = 0;
        (*pcb).sig_pending.sigqueue =
            Box::leak(Box::new(SigQueue::default())) as *mut SigQueue as *mut c_void;
    }
}
