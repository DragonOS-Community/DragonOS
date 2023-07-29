use core::{ffi::c_void, ptr::null_mut, sync::atomic::compiler_fence};

use alloc::{boxed::Box, string::ToString, sync::Arc};

use crate::{
    arch::{asm::current::current_pcb, interrupt::TrapFrame},
    filesystem::procfs::procfs_register_pid,
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
        rwlock::RwLock,
        spinlock::{spin_lock_irqsave, spin_unlock_irqrestore},
    },
    process::ProcessFlags,
    syscall::SystemError,
};

use super::{KernelStack, Pid, ProcessControlBlock, ProcessManager};

bitflags! {
    /// 进程克隆标志
    pub struct CloneFlags: u32 {
        /// 在进程间共享文件系统信息
        const CLONE_FS = (1 << 0);
        /// 克隆时，与父进程共享信号结构体
        const CLONE_SIGNAL = (1 << 1);
        /// 克隆时，与父进程共享信号处理结构体
        const CLONE_SIGHAND = (1 << 2);
        /// 克隆时，将原本被设置为SIG_IGNORE的信号，设置回SIG_DEFAULT
        const CLONE_CLEAR_SIGHAND = (1 << 3);
        /// 在进程间共享虚拟内存空间
        const CLONE_VM = (1 << 4);
        /// 拷贝线程
        const CLONE_THREAD = (1 << 5);
        /// 共享打开的文件
        const CLONE_FILES = (1 << 6);
    }
}

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
    let mut flags: usize = 0;

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

    spin_unlock_irqrestore(unsafe { &mut (*current_pcb().sighand).siglock }, flags);
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
            Box::leak(Box::new(SigQueue::new(None))) as *mut SigQueue as *mut c_void;
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

/// 【旧的进程管理】拷贝进程的地址空间
///
/// ## 参数
///
/// - `clone_vm`: 是否与父进程共享地址空间。true表示共享
/// - `new_pcb`: 新进程的pcb
///
/// ## 返回值
///
/// - 成功：返回Ok(())
/// - 失败：返回Err(SystemError)
///
/// ## Panic
///
/// - 如果当前进程没有用户地址空间，则panic
pub fn copy_mm(clone_vm: bool, new_pcb: &mut process_control_block) -> Result<(), SystemError> {
    // kdebug!("copy_mm, clone_vm: {}", clone_vm);
    let old_address_space = current_pcb()
        .address_space()
        .expect("copy_mm: Failed to get address space of current process.");

    if clone_vm {
        unsafe { new_pcb.set_address_space(old_address_space) };
        return Ok(());
    }

    let new_address_space = old_address_space.write().try_clone().unwrap_or_else(|e| {
        panic!(
            "copy_mm: Failed to clone address space of current process, current pid: [{}], new pid: [{}]. Error: {:?}",
            current_pcb().pid, new_pcb.pid, e
        )
    });
    unsafe { new_pcb.set_address_space(new_address_space) };
    return Ok(());
}

impl ProcessManager {
    pub fn fork(current_trapframe: TrapFrame, clone_flags: CloneFlags) -> Result<Pid, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        let new_kstack = KernelStack::new()?;
        let name = current_pcb.basic().name().to_string();
        let pcb = ProcessControlBlock::new(name, new_kstack);

        // 为内核线程设置worker private字段。（也许由内核线程机制去做会更好？）
        if current_pcb.flags().contains(ProcessFlags::KTHREAD) {
            unimplemented!("fork: need to set worker private for new process");
        }

        // todo: 维护父子进程关系

        // 拷贝标志位
        ProcessManager::copy_flags(&clone_flags, &pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy flags from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.basic().pid(), pcb.basic().pid(), e
            )
        });

        // 拷贝用户地址空间
        ProcessManager::copy_mm(&clone_flags, &current_pcb, &pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy mm from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.basic().pid(), pcb.basic().pid(), e
            )
        });

        // 拷贝文件描述符表
        ProcessManager::copy_files(&clone_flags, &current_pcb, &pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy files from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.basic().pid(), pcb.basic().pid(), e
            )
        });

        // todo: 拷贝信号相关数据

        // 拷贝线程
        ProcessManager::copy_thread(&clone_flags, &current_pcb, &pcb, &current_trapframe).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy thread from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.basic().pid(), pcb.basic().pid(), e
            )
        });

        // 向procfs注册进程
        procfs_register_pid(pcb.basic().pid()).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to register pid to procfs, pid: [{:?}]. Error: {:?}",
                pcb.basic().pid(),
                e
            )
        });

        ProcessManager::wakeup(&pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to wakeup new process, pid: [{:?}]. Error: {:?}",
                pcb.basic().pid(),
                e
            )
        });

        return Ok(pcb.basic().pid());
    }

    fn copy_flags(
        clone_flags: &CloneFlags,
        new_pcb: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        if clone_flags.contains(CloneFlags::CLONE_VM) {
            new_pcb.flags().insert(ProcessFlags::VFORK);
        }
        return Ok(());
    }

    /// 拷贝进程的地址空间
    ///
    /// ## 参数
    ///
    /// - `clone_vm`: 是否与父进程共享地址空间。true表示共享
    /// - `new_pcb`: 新进程的pcb
    ///
    /// ## 返回值
    ///
    /// - 成功：返回Ok(())
    /// - 失败：返回Err(SystemError)
    ///
    /// ## Panic
    ///
    /// - 如果当前进程没有用户地址空间，则panic
    fn copy_mm(
        clone_flags: &CloneFlags,
        current_pcb: &Arc<ProcessControlBlock>,
        new_pcb: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        let old_address_space = current_pcb.basic().user_vm().unwrap_or_else(|| {
            panic!(
                "copy_mm: Failed to get address space of current process, current pid: [{:?}]",
                current_pcb.basic().pid()
            )
        });

        if clone_flags.contains(CloneFlags::CLONE_VM) {
            new_pcb.basic_mut().set_user_vm(Some(old_address_space));
            return Ok(());
        }

        let new_address_space = old_address_space.write().try_clone().unwrap_or_else(|e| {
            panic!(
                "copy_mm: Failed to clone address space of current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.basic().pid(), new_pcb.basic().pid(), e
            )
        });
        new_pcb.basic_mut().set_user_vm(Some(new_address_space));
        return Ok(());
    }

    fn copy_files(
        clone_flags: &CloneFlags,
        current_pcb: &Arc<ProcessControlBlock>,
        new_pcb: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        // 如果不共享文件描述符表，则拷贝文件描述符表
        if !clone_flags.contains(CloneFlags::CLONE_FILES) {
            let new_fd_table = current_pcb.basic().fd_table().unwrap().read().clone();
            let new_fd_table = Arc::new(RwLock::new(new_fd_table));
            new_pcb.basic_mut().set_fd_table(Some(new_fd_table));
        }

        // 如果共享文件描述符表，则直接拷贝指针
        new_pcb
            .basic_mut()
            .set_fd_table(current_pcb.basic().fd_table().clone());

        return Ok(());
    }

    fn copy_sighand(
        clone_flags: &CloneFlags,
        current_pcb: &Arc<ProcessControlBlock>,
        new_pcb: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        // todo: 由于信号原来写的太烂，移植到新的进程管理的话，需要改动很多。因此决定重写。这里先空着
        return Ok(());
    }
}
