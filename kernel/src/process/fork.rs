use alloc::boxed::Box;

use crate::{
    arch::x86_64::asm::current::current_pcb,
    include::{
        bindings::bindings::{
            process_control_block, spinlock_t, CLONE_CLEAR_SIGHAND, CLONE_SIGHAND,
        },
        DragonOS::signal::{sigaction, sighand_struct},
    },
    ipc::signal::DEFAULT_SIGACTION,
    kdebug,
    libs::{
        ffi_convert::FFIBind2Rust,
        refcount::{refcount_inc, RefCount},
        spinlock::{spin_lock_irqsave, spin_unlock_irqrestore},
    },
};

#[no_mangle]
pub extern "C" fn process_copy_sighand(clone_flags: u64, pcb: *mut process_control_block) -> i32 {
    kdebug!("process_copy_sighand");
    if (clone_flags & (CLONE_SIGHAND as u64)) != 0 {
        let r = RefCount::convert_mut(unsafe { &mut (*(current_pcb().sighand)).count }).unwrap();
        refcount_inc(r);
    }
    let mut sig = Box::new(sighand_struct::default());
    // 将新的sighand赋值给pcb
    unsafe {
        (*pcb).sighand = sig.as_mut() as *mut sighand_struct as usize
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
    unsafe {
        drop((*pcb).sighand as *mut sighand_struct);
    }
    0
}

#[no_mangle]
pub extern "C" fn process_exit_signal(pcb: *mut process_control_block) {
    // todo: 回收进程的信号结构体
}

#[no_mangle]
pub extern "C" fn process_exit_sighand(pcb: *mut process_control_block) {
    // todo: 回收进程的sighand结构体
}
