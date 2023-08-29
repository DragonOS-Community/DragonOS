use core::{ffi::c_void, mem::ManuallyDrop, ptr::null_mut};

use alloc::sync::Arc;

use crate::{
    arch::asm::current::current_pcb,
    filesystem::vfs::{
        file::{File, FileMode},
        ROOT_INODE,
    },
    include::bindings::bindings::{
        process_control_block, PROC_INTERRUPTIBLE, PROC_UNINTERRUPTIBLE,
    },
    mm::ucontext::AddressSpace,
    process::ProcessManager,
    syscall::SystemError,
};

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
    todo!("Implement process_try_to_wake_up in new process manager");
}

/// @brief 当进程，满足 (@state & @pcb->state)时，唤醒进程，并设置： @pcb->state = TASK_RUNNING.
///
/// @return true 唤醒成功
/// @return false 唤醒失败
#[no_mangle]
pub extern "C" fn process_wake_up_state(pcb: *mut process_control_block, state: u64) -> bool {
    return process_try_to_wake_up(pcb, state, 0);
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
