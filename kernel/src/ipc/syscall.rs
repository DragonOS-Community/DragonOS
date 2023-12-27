use core::{
    ffi::{c_int, c_void},
    sync::atomic::compiler_fence,
};

use system_error::SystemError;

use crate::{
    arch::ipc::signal::{SigCode, SigFlags, SigSet, Signal},
    filesystem::vfs::{
        file::{File, FileMode},
        FilePrivateData,
    },
    kerror, kwarn,
    mm::VirtAddr,
    process::{Pid, ProcessManager},
    syscall::{user_access::UserBufferWriter, Syscall},
};

use super::{
    pipe::{LockedPipeInode, PipeFsPrivateData},
    signal_types::{
        SaHandlerType, SigInfo, SigType, Sigaction, SigactionType, UserSigaction, USER_SIG_DFL,
        USER_SIG_ERR, USER_SIG_IGN,
    },
};

impl Syscall {
    /// # 创建带参数的匿名管道
    ///
    /// ## 参数
    ///
    /// - `fd`: 用于返回文件描述符的数组
    /// - `flags`:设置管道的参数
    pub fn pipe2(fd: *mut i32, flags: FileMode) -> Result<usize, SystemError> {
        if !flags
            .difference(FileMode::O_CLOEXEC | FileMode::O_NONBLOCK | FileMode::O_DIRECT)
            .is_empty()
        {
            return Err(SystemError::EINVAL);
        }

        let mut user_buffer = UserBufferWriter::new(fd, core::mem::size_of::<[c_int; 2]>(), true)?;
        let fd = user_buffer.buffer::<i32>(0)?;
        let pipe_ptr = LockedPipeInode::new();

        let mut read_file = File::new(
            pipe_ptr.clone(),
            FileMode::O_RDONLY | (flags & FileMode::O_NONBLOCK),
        )?;
        read_file.private_data =
            FilePrivateData::Pipefs(PipeFsPrivateData::new(FileMode::O_RDONLY));

        let mut write_file = File::new(
            pipe_ptr.clone(),
            FileMode::O_WRONLY | (flags & (FileMode::O_NONBLOCK | FileMode::O_DIRECT)),
        )?;
        write_file.private_data = FilePrivateData::Pipefs(PipeFsPrivateData::new(
            FileMode::O_WRONLY | (flags & (FileMode::O_NONBLOCK | FileMode::O_DIRECT)),
        ));

        if flags.contains(FileMode::O_CLOEXEC) {
            read_file.set_close_on_exec(true);
            write_file.set_close_on_exec(true);
        }
        let fd_table_ptr = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = fd_table_ptr.write();
        let read_fd = fd_table_guard.alloc_fd(read_file, None)?;
        let write_fd = fd_table_guard.alloc_fd(write_file, None)?;

        drop(fd_table_guard);

        fd[0] = read_fd;
        fd[1] = write_fd;
        Ok(0)
    }

    pub fn kill(pid: Pid, sig: c_int) -> Result<usize, SystemError> {
        let sig = Signal::from(sig);
        if sig == Signal::INVALID {
            // 传入的signal数值不合法
            kwarn!("Not a valid signal number");
            return Err(SystemError::EINVAL);
        }

        // 初始化signal info
        let mut info = SigInfo::new(sig, 0, SigCode::User, SigType::Kill(pid));

        compiler_fence(core::sync::atomic::Ordering::SeqCst);

        let retval = sig
            .send_signal_info(Some(&mut info), pid)
            .map(|x| x as usize);

        compiler_fence(core::sync::atomic::Ordering::SeqCst);

        return retval;
    }

    /// 通用信号注册函数
    ///
    /// ## 参数
    ///
    /// - `sig` 信号的值
    /// - `act` 用户空间传入的 Sigaction 指针
    /// - `old_act` 用户空间传入的用来保存旧 Sigaction 的指针
    /// - `from_user` 用来标识这个函数调用是否来自用户空间
    ///
    /// @return int 错误码
    #[no_mangle]
    pub fn sigaction(
        sig: c_int,
        new_act: usize,
        old_act: usize,
        from_user: bool,
    ) -> Result<usize, SystemError> {
        // 请注意：用户态传进来的user_sigaction结构体类型，请注意，这个结构体与内核实际的不一样
        let act: *mut UserSigaction = new_act as *mut UserSigaction;
        let old_act = old_act as *mut UserSigaction;
        let mut new_ka: Sigaction = Default::default();
        let mut old_sigaction: Sigaction = Default::default();
        // 如果传入的，新的sigaction不为空
        if !act.is_null() {
            // 如果参数的范围不在用户空间，则返回错误
            let r = UserBufferWriter::new(act, core::mem::size_of::<Sigaction>(), from_user);
            if r.is_err() {
                return Err(SystemError::EFAULT);
            }
            let mask: SigSet = unsafe { (*act).mask };
            let input_sighandler = unsafe { (*act).handler as u64 };
            match input_sighandler {
                USER_SIG_DFL => {
                    new_ka = Sigaction::DEFAULT_SIGACTION.clone();
                    *new_ka.flags_mut() = unsafe { (*act).flags };
                    new_ka.set_restorer(None);
                }

                USER_SIG_IGN => {
                    new_ka = Sigaction::DEFAULT_SIGACTION_IGNORE.clone();
                    *new_ka.flags_mut() = unsafe { (*act).flags };

                    new_ka.set_restorer(None);
                }
                _ => {
                    // 从用户空间获得sigaction结构体
                    // TODO mask是default还是用户空间传入
                    new_ka = Sigaction::new(
                        SigactionType::SaHandler(SaHandlerType::SigCustomized(unsafe {
                            VirtAddr::new((*act).handler as usize)
                        })),
                        unsafe { (*act).flags },
                        SigSet::default(),
                        unsafe { Some(VirtAddr::new((*act).restorer as usize)) },
                    );
                }
            }

            // TODO 如果为空，赋默认值？
            // kdebug!("new_ka={:?}", new_ka);
            // 如果用户手动给了sa_restorer，那么就置位SA_FLAG_RESTORER，否则报错。（用户必须手动指定restorer）
            if new_ka.restorer().is_some() {
                new_ka.flags_mut().insert(SigFlags::SA_RESTORER);
            } else if new_ka.action().is_customized() {
                kerror!(
                "pid:{:?}: in sys_sigaction: User must manually sprcify a sa_restorer for signal {}.",
                ProcessManager::current_pcb().pid(),
                sig
            );
                return Err(SystemError::EINVAL);
            }
            *new_ka.mask_mut() = mask;
        }

        let sig = Signal::from(sig as i32);
        // 如果给出的信号值不合法
        if sig == Signal::INVALID {
            return Err(SystemError::EINVAL);
        }

        let retval = super::signal::do_sigaction(
            sig,
            if act.is_null() {
                None
            } else {
                Some(&mut new_ka)
            },
            if old_act.is_null() {
                None
            } else {
                Some(&mut old_sigaction)
            },
        );

        //
        if (retval == Ok(())) && (!old_act.is_null()) {
            let r =
                UserBufferWriter::new(old_act, core::mem::size_of::<UserSigaction>(), from_user);
            if r.is_err() {
                return Err(SystemError::EFAULT);
            }

            let sigaction_handler: VirtAddr;
            sigaction_handler = match old_sigaction.action() {
                SigactionType::SaHandler(handler) => {
                    if let SaHandlerType::SigCustomized(hand) = handler {
                        hand
                    } else if handler.is_sig_ignore() {
                        VirtAddr::new(USER_SIG_IGN as usize)
                    } else if handler.is_sig_error() {
                        VirtAddr::new(USER_SIG_ERR as usize)
                    } else {
                        VirtAddr::new(USER_SIG_DFL as usize)
                    }
                }
                SigactionType::SaSigaction(_) => {
                    kerror!("unsupported type: SaSigaction");
                    VirtAddr::new(USER_SIG_DFL as usize)
                }
            };

            unsafe {
                (*old_act).handler = sigaction_handler.data() as *mut c_void;
                (*old_act).flags = old_sigaction.flags();
                (*old_act).mask = old_sigaction.mask();
                if old_sigaction.restorer().is_some() {
                    (*old_act).restorer = old_sigaction.restorer().unwrap().data() as *mut c_void;
                }
            }
        }
        return retval.map(|_| 0);
    }
}
