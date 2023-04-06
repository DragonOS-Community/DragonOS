use core::{ffi::c_void, ptr::null_mut, sync::atomic::compiler_fence};

use alloc::boxed::Box;

use crate::{
    arch::asm::current::current_pcb,
    include::bindings::bindings::{
        process_control_block, CLONE_CLEAR_SIGHAND, CLONE_SIGHAND, CLONE_THREAD,
    },
    ipc::{
        signal::{flush_signal_handlers, DEFAULT_SIGACTION},
        signal_types::{sigaction, sighand_struct, signal_struct, SigQueue},
    },
    libs::{
        atomic::atomic_set,
        ffi_convert::FFIBind2Rust,
        refcount::{refcount_inc, RefCount},
        spinlock::{spin_lock_irqsave, spin_unlock_irqrestore},
    }, syscall::SystemError,
};

#[no_mangle]
pub extern "C" fn process_copy_sighand(clone_flags: u64, pcb: *mut process_control_block) -> i32 {
    // kdebug!("process_copy_sighand");

    if (clone_flags & (CLONE_SIGHAND as u64)) != 0 {
        let r = RefCount::convert_mut(unsafe { &mut (*(current_pcb().sighand)).count }).unwrap();
        refcount_inc(r);
    }

    // 在这里使用Box::leak将动态申请的内存的生命周期转换为static的
    let mut sig: &mut sighand_struct = Box::leak(Box::new(sighand_struct::default()));
    if (sig as *mut sighand_struct) == null_mut() {
        return SystemError::ENOMEM.to_posix_errno();
    }

    // 将新的sighand赋值给pcb
    unsafe {
        (*pcb).sighand = sig as *mut sighand_struct as usize
            as *mut crate::include::bindings::bindings::sighand_struct;
    }

    // kdebug!("DEFAULT_SIGACTION.sa_flags={}", DEFAULT_SIGACTION.sa_flags);

    // 拷贝sigaction
    let mut flags: u64 = 0;

    spin_lock_irqsave(unsafe { &mut (*current_pcb().sighand).siglock }, &mut flags);
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    for (index, x) in unsafe { (*current_pcb().sighand).action }
        .iter()
        .enumerate()
    {
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        if !(x as *const crate::include::bindings::bindings::sigaction).is_null() {
            sig.action[index] =
                *sigaction::convert_ref(x as *const crate::include::bindings::bindings::sigaction)
                    .unwrap();
        } else {
            sig.action[index] = DEFAULT_SIGACTION;
        }
    }
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    spin_unlock_irqrestore(unsafe { &mut (*current_pcb().sighand).siglock }, &flags);
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    // 将信号的处理函数设置为default(除了那些被手动屏蔽的)
    if (clone_flags & (CLONE_CLEAR_SIGHAND as u64)) != 0 {
        compiler_fence(core::sync::atomic::Ordering::SeqCst);

        flush_signal_handlers(pcb, false);
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    return 0;
}

#[no_mangle]
pub extern "C" fn process_copy_signal(clone_flags: u64, pcb: *mut process_control_block) -> i32 {
    // kdebug!("process_copy_signal");
    // 如果克隆的是线程，则不拷贝信号（同一进程的各个线程之间共享信号）
    if (clone_flags & (CLONE_THREAD as u64)) != 0 {
        return 0;
    }
    let sig: &mut signal_struct = Box::leak(Box::new(signal_struct::default()));
    if (sig as *mut signal_struct) == null_mut() {
        return SystemError::ENOMEM.to_posix_errno();
    }
    atomic_set(&mut sig.sig_cnt, 1);
    // 将sig赋值给pcb中的字段
    unsafe {
        (*pcb).signal = sig as *mut signal_struct as usize
            as *mut crate::include::bindings::bindings::signal_struct;
    }

    // 创建新的sig_pending->sigqueue
    unsafe {
        (*pcb).sig_pending.signal = 0;
        (*pcb).sig_pending.sigqueue =
            Box::leak(Box::new(SigQueue::default())) as *mut SigQueue as *mut c_void;
    }
    return 0;
}

#[no_mangle]
pub extern "C" fn process_exit_signal(pcb: *mut process_control_block) {
    // 回收进程的信号结构体
    unsafe {
        // 回收sighand
        let sighand = Box::from_raw((*pcb).sighand as *mut sighand_struct);

        drop(sighand);
        (*pcb).sighand = 0 as *mut crate::include::bindings::bindings::sighand_struct;

        // 回收sigqueue
        let queue = Box::from_raw((*pcb).sig_pending.sigqueue as *mut SigQueue);
        drop(queue);
    }
}

#[no_mangle]
pub extern "C" fn process_exit_sighand(pcb: *mut process_control_block) {
    // todo: 回收进程的sighand结构体
    unsafe {
        let sig = Box::from_raw((*pcb).signal as *mut signal_struct);
        drop(sig);
        (*pcb).signal = 0 as *mut crate::include::bindings::bindings::signal_struct;
    }
}
