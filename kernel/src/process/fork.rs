use core::ptr::null_mut;

use alloc::boxed::Box;

use crate::{
    arch::x86_64::asm::current::current_pcb,
    include::{
        bindings::bindings::{
            process_control_block, CLONE_CLEAR_SIGHAND, CLONE_SIGHAND, CLONE_THREAD, ENOMEM,
        },
        DragonOS::signal::{sigaction, sighand_struct, signal_struct},
    },
    ipc::signal::DEFAULT_SIGACTION,
    kdebug,
    libs::{
        ffi_convert::FFIBind2Rust,
        refcount::{refcount_inc, RefCount},
        spinlock::{spin_lock_irqsave, spin_unlock_irqrestore}, atomic::atomic_set,
    },
};

#[no_mangle]
pub extern "C" fn process_copy_sighand(clone_flags: u64, pcb: *mut process_control_block) -> i32 {
    kdebug!("process_copy_sighand");
    if (clone_flags & (CLONE_SIGHAND as u64)) != 0 {
        let r = RefCount::convert_mut(unsafe { &mut (*(current_pcb().sighand)).count }).unwrap();
        refcount_inc(r);
    }
    // 在这里使用Box::leak将动态申请的内存的生命周期转换为static的
    let mut sig: &mut sighand_struct = Box::leak(Box::new(sighand_struct::default()));
    if (sig as *mut sighand_struct) == null_mut() {
        return -(ENOMEM as i32);
    }
    // 将新的sighand赋值给pcb
    unsafe {
        (*pcb).sighand = sig as *mut sighand_struct as usize
            as *mut crate::include::bindings::bindings::sighand_struct;
    }

    // 拷贝sigaction
    let mut flags: u64 = 0;
    spin_lock_irqsave(unsafe { &mut (*current_pcb().sighand).siglock }, &mut flags);
    for (index, x) in unsafe { (*current_pcb().sighand).action }
        .iter()
        .enumerate()
    {
        if !(x as *const crate::include::bindings::bindings::sigaction).is_null() {
            sig.action[index] =
                *sigaction::convert_ref(x as *const crate::include::bindings::bindings::sigaction)
                    .unwrap();
        } else {
            sig.action[index] = DEFAULT_SIGACTION;
        }
    }

    spin_unlock_irqrestore(unsafe { &mut (*current_pcb().sighand).siglock }, &flags);

    // 将所有屏蔽的信号的处理函数设置为default
    if (clone_flags & (CLONE_CLEAR_SIGHAND as u64)) != 0 {
        todo!();
    }

    return 0;
}

#[no_mangle]
pub extern "C" fn process_copy_signal(clone_flags: u64, pcb: *mut process_control_block) -> i32 {
    kdebug!("process_copy_signal");
    // 如果克隆的是线程，则不拷贝信号（同一进程的各个线程之间共享信号）
    if (clone_flags & (CLONE_THREAD as u64)) != 0 {
        return 0;
    }
    let sig: &mut signal_struct = Box::leak(Box::new(signal_struct::default()));
    if (sig as *mut signal_struct) == null_mut() {
        return -(ENOMEM as i32);
    }
    atomic_set(&mut sig.sig_cnt, 1);
    // 将sig赋值给pcb中的字段
    unsafe {
        (*pcb).signal = sig as *mut signal_struct as usize
            as *mut crate::include::bindings::bindings::signal_struct;
    }
    return 0;
}

#[no_mangle]
pub extern "C" fn process_exit_signal(pcb: *mut process_control_block) {
    // 回收进程的信号结构体
    unsafe {
        drop((*pcb).sighand as *mut sighand_struct);
        (*pcb).sighand = 0 as *mut crate::include::bindings::bindings::sighand_struct;
    }
}

#[no_mangle]
pub extern "C" fn process_exit_sighand(pcb: *mut process_control_block) {
    // todo: 回收进程的sighand结构体
    unsafe {
        drop((*pcb).signal as *mut signal_struct);
        (*pcb).signal = 0 as *mut crate::include::bindings::bindings::signal_struct;
    }
}
