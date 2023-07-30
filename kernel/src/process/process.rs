use core::{
    ffi::c_void,
    mem::ManuallyDrop,
    ptr::{null_mut, read_volatile, write_volatile},
};

use alloc::{boxed::Box, sync::Arc};

use crate::{
    arch::asm::current::current_pcb,
    filesystem::vfs::{
        file::{File, FileDescriptorVec, FileMode},
        FileType, ROOT_INODE,
    },
    include::bindings::bindings::{
        process_control_block, CLONE_FS, PROC_INTERRUPTIBLE, PROC_RUNNING, PROC_STOPPED,
        PROC_UNINTERRUPTIBLE,
    },
    libs::casting::DowncastArc,
    mm::ucontext::AddressSpace,
    net::socket::SocketInode,
    process::ProcessManager,
    sched::core::{cpu_executing, sched_enqueue},
    smp::core::{smp_get_processor_id, smp_send_reschedule},
    syscall::SystemError,
};

use super::preempt::{preempt_disable, preempt_enable};

/// 判断进程是否已经停止
#[no_mangle]
pub extern "C" fn process_is_stopped(pcb: *const process_control_block) -> bool {
    let state: u64 = unsafe { read_volatile(&(*pcb).state) } as u64;
    if (state & (PROC_STOPPED as u64)) != 0 {
        return true;
    } else {
        return false;
    }
}

/// @brief 尝试唤醒指定的进程。
/// 本函数的行为：If (@_state & @pcb->state) @pcb->state = TASK_RUNNING.
///
/// @param _pcb 要被唤醒的进程的pcb
/// @param _state 如果pcb的state与_state匹配，则唤醒这个进程
/// @param _wake_flags 保留，暂未使用，请置为0
/// @return true: 成功唤醒
///         false: 不符合唤醒条件，无法唤醒
#[no_mangle]
pub extern "C" fn process_try_to_wake_up(
    _pcb: *mut process_control_block,
    _state: u64,
    _wake_flags: i32,
) -> bool {
    preempt_disable();

    let mut retval = false;
    // 获取对pcb的可变引用
    let pcb = unsafe { _pcb.as_mut() }.unwrap();

    // 如果要唤醒的就是当前的进程
    if current_pcb() as *mut process_control_block as usize == _pcb as usize {
        unsafe {
            write_volatile(&mut pcb.state, PROC_RUNNING as u64);
        }
        preempt_enable();
        retval = true;
        return retval;
    }
    // todo: 将来调度器引入ttwu队列之后，需要修改这里的判断条件

    // todo: 为pcb引入pi_lock,然后在这里加锁
    if unsafe { read_volatile(&pcb.state) } & _state != 0 {
        // 可以wakeup
        unsafe {
            write_volatile(&mut pcb.state, PROC_RUNNING as u64);
        }
        sched_enqueue(pcb, true);

        retval = true;
    }
    // todo: 对pcb的pi_lock放锁
    preempt_enable();
    return retval;
}

/// @brief 当进程，满足 (@state & @pcb->state)时，唤醒进程，并设置： @pcb->state = TASK_RUNNING.
///
/// @return true 唤醒成功
/// @return false 唤醒失败
#[no_mangle]
pub extern "C" fn process_wake_up_state(pcb: *mut process_control_block, state: u64) -> bool {
    return process_try_to_wake_up(pcb, state, 0);
}

/// @brief 让一个正在cpu上运行的进程陷入内核
pub fn process_kick(pcb: *mut process_control_block) {
    preempt_disable();
    let cpu = process_cpu(pcb);
    // 如果给定的进程正在别的核心上执行，则立即发送请求，让它陷入内核态，以及时响应信号。
    if cpu != smp_get_processor_id() && process_is_executing(pcb) {
        smp_send_reschedule(cpu);
    }
    preempt_enable();
}

/// @brief 获取给定的进程在哪个cpu核心上运行(使用volatile避免编译器优化)
#[inline]
pub fn process_cpu(pcb: *const process_control_block) -> u32 {
    unsafe { read_volatile(&(*pcb).cpu_id) }
}

/// @brief 判断给定的进程是否正在处理器上执行
///
/// @param pcb 进程的pcb
#[inline]
pub fn process_is_executing(pcb: *const process_control_block) -> bool {
    return cpu_executing(process_cpu(pcb)) as *const process_control_block == pcb;
}

impl process_control_block {
    /// @brief 标记当前pcb已经由其他机制进行管理，调度器将不会将他加入队列(且进程可以被信号打断)
    /// 当我们要把一个进程，交给其他机制管理时，那么就应该调用本函数。
    ///
    /// 由于本函数可能造成进程不再被调度，因此标记为unsafe
    #[allow(dead_code)]
    pub unsafe fn mark_sleep_interruptible(&mut self) {
        self.state = PROC_INTERRUPTIBLE as u64;
    }

    /// @brief 标记当前pcb已经由其他机制进行管理，调度器将不会将他加入队列(且进程不可以被信号打断)
    /// 当我们要把一个进程，交给其他机制管理时，那么就应该调用本函数
    ///
    /// 由于本函数可能造成进程不再被调度，因此标记为unsafe
    #[allow(dead_code)]
    pub unsafe fn mark_sleep_uninterruptible(&mut self) {
        self.state = PROC_UNINTERRUPTIBLE as u64;
    }

    /// 释放pcb中存储的地址空间的指针
    pub unsafe fn drop_address_space(&mut self) {
        let p = self.address_space as *const AddressSpace;
        if p.is_null() {
            return;
        }
        let p: Arc<AddressSpace> = Arc::from_raw(p);
        drop(p);
        self.address_space = null_mut();
    }

    /// 设置pcb中存储的地址空间的指针
    ///
    /// ## panic
    /// 如果当前pcb已经有地址空间，那么panic
    pub unsafe fn set_address_space(&mut self, address_space: Arc<AddressSpace>) {
        assert!(self.address_space.is_null(), "Address space already set");
        self.address_space = Arc::into_raw(address_space) as *mut c_void;
    }

    /// 获取当前进程的地址空间的指针
    pub fn address_space(&self) -> Option<Arc<AddressSpace>> {
        let ptr = self.address_space as *const AddressSpace;
        if ptr.is_null() {
            return None;
        }
        // 为了防止pcb中的指针被释放，这里需要将其包装一下，使得Arc的drop不会被调用
        let arc_wrapper = ManuallyDrop::new(unsafe { Arc::from_raw(ptr) });

        let result = Arc::clone(&arc_wrapper);
        return Some(result);
    }
}

/// @brief 初始化pid=1的进程的stdio
pub fn init_stdio() -> Result<(), SystemError> {
    if current_pcb().pid != 1 {
        return Err(SystemError::EPERM);
    }
    let tty_inode = ROOT_INODE()
        .lookup("/dev/tty0")
        .expect("Init stdio: can't find tty0");
    let stdin =
        File::new(tty_inode.clone(), FileMode::O_RDONLY).expect("Init stdio: can't create stdin");
    let stdout =
        File::new(tty_inode.clone(), FileMode::O_WRONLY).expect("Init stdio: can't create stdout");
    let stderr = File::new(tty_inode.clone(), FileMode::O_WRONLY | FileMode::O_SYNC)
        .expect("Init stdio: can't create stderr");

    /*
       按照规定，进程的文件描述符数组的前三个位置，分别是stdin, stdout, stderr
    */
    assert_eq!(
        ProcessManager::current_pcb()
            .fd_table()
            .write()
            .alloc_fd(stdin, None)
            .unwrap(),
        0
    );
    assert_eq!(
        ProcessManager::current_pcb()
            .fd_table()
            .write()
            .alloc_fd(stdout, None)
            .unwrap(),
        1
    );
    assert_eq!(
        ProcessManager::current_pcb()
            .fd_table()
            .write()
            .alloc_fd(stderr, None)
            .unwrap(),
        2
    );
    return Ok(());
}
