use core::{
    ffi::c_void,
    ptr::{null_mut, read_volatile, write_volatile},
};

use alloc::boxed::Box;

use crate::{
    arch::asm::current::current_pcb,
    filesystem::vfs::file::{File, FileDescriptorVec},
    include::bindings::bindings::{
        process_control_block, CLONE_FS, EBADF, EFAULT, ENFILE, EPERM, PROC_INTERRUPTIBLE,
        PROC_RUNNING, PROC_STOPPED, PROC_UNINTERRUPTIBLE,
    },
    sched::core::{cpu_executing, sched_enqueue},
    smp::core::{smp_get_processor_id, smp_send_reschedule},
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
    /// @brief 初始化进程PCB的文件描述符数组。
    /// 请注意，如果当前进程已经有文件描述符数组，那么本操作将被禁止
    pub fn init_files(&mut self) -> Result<(), i32> {
        if self.fds != null_mut() {
            // 这个操作不被允许，否则会产生内存泄露。
            // 原因是，C的pcb里面，文件描述符数组的生命周期是static的，如果继续执行，会产生内存泄露的问题。
            return Err(-(EPERM as i32));
        }
        let fd_vec: &mut FileDescriptorVec = Box::leak(FileDescriptorVec::new());
        self.fds = fd_vec as *mut FileDescriptorVec as usize as *mut c_void;
        return Ok(());
    }

    /// @brief 拷贝进程的文件描述符
    ///
    /// @param clone_flags 进程fork的克隆标志位
    /// @param from 源pcb。从它里面拷贝文件描述符
    ///
    /// @return Ok(()) 拷贝成功
    /// @return Err(i32) 拷贝失败，错误码
    pub fn copy_files(
        &mut self,
        clone_flags: u64,
        from: &'static process_control_block,
    ) -> Result<(), i32> {
        // 不拷贝父进程的文件描述符
        if clone_flags & CLONE_FS as u64 != 0 {
            // 由于拷贝pcb的时候，直接copy的指针，因此这里置为空
            self.fds = null_mut();
            self.init_files()?;
            return Ok(());
        }
        // 获取源pcb的文件描述符数组的引用
        let old_fds: &mut FileDescriptorVec = if let Some(o_fds) = FileDescriptorVec::from_pcb(from)
        {
            o_fds
        } else {
            return self.init_files();
        };

        // 拷贝文件描述符数组
        let new_fd_vec: &mut FileDescriptorVec = Box::leak(Box::new(old_fds.clone()));

        self.fds = new_fd_vec as *mut FileDescriptorVec as usize as *mut c_void;

        return Ok(());
    }

    /// @brief 释放文件描述符数组。本函数会drop掉整个文件描述符数组，并把pcb的fds字段设置为空指针。
    pub fn exit_files(&mut self) -> Result<(), i32> {
        if self.fds.is_null() {
            return Ok(());
        }

        let old_fds: Box<FileDescriptorVec> =
            unsafe { Box::from_raw(self.fds as *mut FileDescriptorVec) };
        drop(old_fds);
        self.fds = null_mut();
        return Ok(());
    }

    /// @brief 申请文件描述符，并把文件对象存入其中。
    ///
    /// @return Ok(i32) 申请到的文件描述符编号
    /// @return Err(i32) 申请失败，返回错误码，并且，file对象将被drop掉
    pub fn alloc_fd(&mut self, file: File) -> Result<i32, i32> {
        // 获取pcb的文件描述符数组的引用
        let fds: &mut FileDescriptorVec =
            if let Some(f) = FileDescriptorVec::from_pcb(current_pcb()) {
                f
            } else {
                // 如果进程还没有初始化文件描述符数组，那就初始化它
                self.init_files().ok();
                let r: Option<&mut FileDescriptorVec> = FileDescriptorVec::from_pcb(current_pcb());
                if r.is_none() {
                    drop(file);
                    // 初始化失败
                    return Err(-(EFAULT as i32));
                }
                r.unwrap()
            };

        // 寻找空闲的文件描述符
        let mut cnt = 0;
        for x in fds.fds.iter_mut() {
            if x.is_none() {
                *x = Some(Box::new(file));
                return Ok(cnt);
            }
            cnt += 1;
        }
        return Err(-(ENFILE as i32));
    }

    /// @brief 根据文件描述符序号，获取文件结构体的可变引用
    ///
    /// @param fd 文件描述符序号
    ///
    /// @return Option(&mut File) 文件对象的可变引用
    pub fn get_file_mut_by_fd(&self, fd: i32) -> Option<&mut File> {
        if !FileDescriptorVec::validate_fd(fd) {
            return None;
        }
        let r: &mut FileDescriptorVec = FileDescriptorVec::from_pcb(current_pcb()).unwrap();
        return r.fds[fd as usize].as_deref_mut();
    }

    /// @brief 根据文件描述符序号，获取文件结构体的不可变引用
    ///
    /// @param fd 文件描述符序号
    ///
    /// @return Option(&File) 文件对象的不可变引用
    #[allow(dead_code)]
    pub fn get_file_ref_by_fd(&self, fd: i32) -> Option<&File> {
        if !FileDescriptorVec::validate_fd(fd) {
            return None;
        }
        let r: &mut FileDescriptorVec = FileDescriptorVec::from_pcb(current_pcb()).unwrap();
        return r.fds[fd as usize].as_deref();
    }

    /// @brief 释放文件描述符，同时关闭文件。
    ///
    /// @param fd 文件描述符序号
    pub fn drop_fd(&self, fd: i32) -> Result<(), i32> {
        // 判断文件描述符的数字是否超过限制
        if !FileDescriptorVec::validate_fd(fd) {
            return Err(-(EBADF as i32));
        }
        let r: &mut FileDescriptorVec = FileDescriptorVec::from_pcb(current_pcb()).unwrap();

        let f: Option<&File> = r.fds[fd as usize].as_deref();
        if f.is_none() {
            // 如果文件描述符不存在，报错
            return Err(-(EBADF as i32));
        }
        // 释放文件
        drop(f);

        // 把文件描述符数组对应位置设置为空
        r.fds[fd as usize] = None;

        return Ok(());
    }

    /// @brief 标记当前pcb已经由其他机制进行管理，调度器将不会将他加入队列(且进程可以被信号打断)
    /// 当我们要把一个进程，交给其他机制管理时，那么就应该调用本函数。
    ///
    /// 由于本函数可能造成进程不再被调度，因此标记为unsafe
    pub unsafe fn mark_sleep_interruptible(&mut self){
        self.state = PROC_INTERRUPTIBLE as u64;
    }

    /// @brief 标记当前pcb已经由其他机制进行管理，调度器将不会将他加入队列(且进程不可以被信号打断)
    /// 当我们要把一个进程，交给其他机制管理时，那么就应该调用本函数
    ///
    /// 由于本函数可能造成进程不再被调度，因此标记为unsafe
    pub unsafe fn mark_sleep_uninterruptible(&mut self){
        self.state = PROC_UNINTERRUPTIBLE as u64;
    }
}

// =========== 导出到C的函数，在将来，进程管理模块被完全重构之后，需要删掉他们  BEGIN ============

/// @brief 初始化当前进程的文件描述符数组
/// 请注意，如果当前进程已经有文件描述符数组，那么本操作将被禁止
#[no_mangle]
pub extern "C" fn process_init_files() -> i32 {
    let r = current_pcb().init_files();
    if r.is_ok() {
        return 0;
    } else {
        return r.unwrap_err();
    }
}

/// @brief 拷贝当前进程的文件描述符信息
///
/// @param clone_flags 克隆标志位
/// @param pcb 新的进程的pcb
#[no_mangle]
pub extern "C" fn process_copy_files(
    clone_flags: u64,
    from: &'static process_control_block,
) -> i32 {
    let r = current_pcb().copy_files(clone_flags, from);
    if r.is_ok() {
        return 0;
    } else {
        return r.unwrap_err();
    }
}

/// @brief 回收进程的文件描述符数组
///
/// @param pcb 要被回收的进程的pcb
///
/// @return i32
#[no_mangle]
pub extern "C" fn process_exit_files(pcb: &'static mut process_control_block) -> i32 {
    let r: Result<(), i32> = pcb.exit_files();
    if r.is_ok() {
        return 0;
    } else {
        return r.unwrap_err();
    }
}

// =========== 以上为导出到C的函数，在将来，进程管理模块被完全重构之后，需要删掉他们 END ============
