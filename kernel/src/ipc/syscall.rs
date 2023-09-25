use core::{
    ffi::{c_int, c_void},
    sync::atomic::compiler_fence,
};

use crate::{
    arch::ipc::signal::{SigCode, SigFlags, SigSet, Signal},
    filesystem::vfs::{file::{File, FileMode}, FilePrivateData},
    kdebug, kerror, kwarn,
    process::{Pid, ProcessManager},
    syscall::{user_access::UserBufferWriter, Syscall, SystemError},
};

use super::{
    pipe::{LockedPipeInode, PipeFsPrivateData},
    signal::{DEFAULT_SIGACTION, DEFAULT_SIGACTION_IGNORE},
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
        if flags.contains(FileMode::O_NONBLOCK)
            || flags.contains(FileMode::O_CLOEXEC)
            || flags.contains(FileMode::O_RDONLY)
        {
            let mut user_buffer =
                UserBufferWriter::new(fd, core::mem::size_of::<[c_int; 2]>(), true)?;
            let fd = user_buffer.buffer::<i32>(0)?;
            let pipe_ptr = LockedPipeInode::new();
            let mut read_file = File::new(pipe_ptr.clone(), FileMode::O_RDONLY)?;
            read_file.private_data =
                FilePrivateData::Pipefs(PipeFsPrivateData::new(FileMode::O_RDONLY));
            let mut write_file = File::new(pipe_ptr.clone(), FileMode::O_WRONLY)?;
            write_file.private_data =
                FilePrivateData::Pipefs(PipeFsPrivateData::new(FileMode::O_WRONLY));
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
        } else {
            Err(SystemError::EINVAL)
        }
    }

    pub fn kill(pid: Pid, sig: c_int) -> Result<usize, SystemError> {
        let sig = Signal::from(sig);
        if sig == Signal::INVALID {
            // 传入的signal数值不合法
            kwarn!("Not a valid signal number");
            return Err(SystemError::EINVAL);
        }

        // 初始化signal info
        let mut info = SigInfo::new(sig, 0, SigCode::SI_USER, SigType::Kill(pid));

        compiler_fence(core::sync::atomic::Ordering::SeqCst);

        let retval = sig
            .signal_kill_something_info(Some(&mut info), pid)
            .map(|x| x as usize);

        compiler_fence(core::sync::atomic::Ordering::SeqCst);

        return retval;
    }

    /// @brief 用户程序用于设置信号处理动作的函数（遵循posix2008）
    ///
    /// @param regs->r8 signumber 信号的编号
    /// @param regs->r9 act 新的，将要被设置的sigaction
    /// @param regs->r10 oact 返回给用户的原本的sigaction（内核将原本的sigaction的值拷贝给这个地址）
    ///
    /// @return int 错误码
    #[no_mangle]
    pub fn sigaction(
        sig: c_int,
        act: usize,
        old_act: usize,
        from_user: bool,
    ) -> Result<usize, SystemError> {
        // 请注意：用户态传进来的user_sigaction结构体类型，请注意，这个结构体与内核实际的不一样
        let act = act as *mut UserSigaction;
        let mut old_act = old_act as *mut UserSigaction;
        let mut new_ka: Sigaction = Default::default();
        let mut old_ka: Sigaction = Default::default();

        // 如果传入的，新的sigaction不为空
        if !act.is_null() {
            // 如果参数的范围不在用户空间，则返回错误
            let r = UserBufferWriter::new(act, core::mem::size_of::<Sigaction>(), from_user);
            if r.is_err() {
                return Err(SystemError::EFAULT);
            }
            let mask: SigSet = unsafe { (*act).mask };
            let input_sighandler = unsafe { (*act).handler as u64 };
            // kdebug!("_input_sah={}", _input_sah);
            match input_sighandler {
                USER_SIG_DFL | USER_SIG_IGN => {
                    if input_sighandler == USER_SIG_DFL {
                        new_ka = *DEFAULT_SIGACTION;
                        *new_ka.flags_mut() = (unsafe { (*act).flags }
                            & (!(SigFlags::SA_FLAG_DFL | SigFlags::SA_FLAG_IGN)))
                            | SigFlags::SA_FLAG_DFL;
                    } else {
                        new_ka = *DEFAULT_SIGACTION_IGNORE;
                        *new_ka.flags_mut() = (unsafe { (*act).flags }
                            & (!(SigFlags::SA_FLAG_DFL | SigFlags::SA_FLAG_IGN)))
                            | SigFlags::SA_FLAG_IGN;
                    }

                    let sar = unsafe { (*act).handler };
                    new_ka.set_restorer(Some(sar as u64));
                }
                _ => {
                    // 从用户空间获得sigaction结构体
                    // TODO mask是default还是用户空间传入
                    kdebug!("--receiving function:{:?}", unsafe { (*act).handler }
                        as u64);
                    new_ka = Sigaction::new(
                        SigactionType::SaHandler(SaHandlerType::SigCustomized(unsafe {
                            (*act).handler as u64
                        })),
                        unsafe { (*act).flags },
                        SigSet::default(),
                        unsafe { Some((*act).restorer as u64) },
                    );
                }
            }
            // kdebug!("new_ka={:?}", new_ka);
            // 如果用户手动给了sa_restorer，那么就置位SA_FLAG_RESTORER，否则报错。（用户必须手动指定restorer）
            if new_ka.restorer().is_some() {
                new_ka.flags_mut().insert(SigFlags::SA_FLAG_RESTORER);
            } else {
                kwarn!(
                "pid:{:?}: in sys_sigaction: User must manually sprcify a sa_restorer for signal {}.",
                ProcessManager::current_pcb().pid(),
                sig
            );
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
                Some(&mut old_ka)
            },
        );

        // 将原本的sigaction拷贝到用户程序指定的地址
        if (retval == Ok(())) && (!old_act.is_null()) {
            let r = UserBufferWriter::new(old_act, core::mem::size_of::<Sigaction>(), from_user);
            if r.is_err() {
                return Err(SystemError::EFAULT);
            }
            // ！！！！！！！！！！todo: 检查这里old_ka的mask，是否为SIG_IGN SIG_DFL,如果是，则将_sa_handler字段替换为对应的值
            let sah: u64;
            if old_ka.flags().contains(SigFlags::SA_FLAG_DFL) {
                sah = USER_SIG_DFL;
            } else if old_ka.flags().contains(SigFlags::SA_FLAG_IGN) {
                sah = USER_SIG_IGN;
            } else {
                sah = match old_ka.action() {
                    SigactionType::SaHandler(handler) => {
                        if let SaHandlerType::SigCustomized(hand) = handler {
                            hand
                        } else if handler.is_sig_ignore() {
                            USER_SIG_IGN
                        } else if handler.is_sig_error() {
                            USER_SIG_ERR
                        } else {
                            USER_SIG_DFL
                        }
                    }
                    SigactionType::SaSigaction(_) => {
                        kerror!("unsupported type: SaSigaction");
                        USER_SIG_DFL
                    }
                }
            }
            unsafe {
                (*old_act).handler = sah as *mut c_void;
                (*old_act).flags = old_ka.flags();
                (*old_act).mask = old_ka.mask();
                if old_ka.restorer().is_some() {
                    (*old_act).restorer = old_ka.restorer().unwrap() as *mut c_void;
                } else {
                    kerror!("Saving old SIGACTION restorer failed: Null pointer of restorer");
                }
            }
        }
        return retval.map(|_| 0);
    }
}
