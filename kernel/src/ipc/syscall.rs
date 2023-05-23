use core::{
    ffi::{c_int, c_void},
    sync::atomic::compiler_fence,
};

use crate::{
    arch::asm::current::current_pcb,
    filesystem::vfs::file::{File, FileMode},
    include::bindings::bindings::{pid_t, verify_area, NULL},
    kwarn,
    syscall::{Syscall, SystemError},
};

use super::{
    pipe::LockedPipeInode,
    signal::{signal_kill_something_info, DEFAULT_SIGACTION, DEFAULT_SIGACTION_IGNORE},
    signal_types::{
        SignalNumber, __siginfo_union, __siginfo_union_data, si_code_val, sigaction,
        sigaction__union_u, siginfo, sigset_init, sigset_t, user_sigaction, SA_FLAG_DFL,
        SA_FLAG_IGN, SA_FLAG_RESTORER, USER_SIG_DFL, USER_SIG_IGN,
    },
};

impl Syscall {
    /// # 创建匿名管道
    ///
    /// ## 参数
    ///
    /// - `fd`: 用于返回文件描述符的数组
    pub fn pipe(fd: &mut [i32]) -> Result<usize, SystemError> {
        let pipe_ptr = LockedPipeInode::new();
        let read_file = File::new(pipe_ptr.clone(), FileMode::O_RDONLY)?;
        let write_file = File::new(pipe_ptr.clone(), FileMode::O_WRONLY)?;

        let read_fd = current_pcb().alloc_fd(read_file, None)?;
        let write_fd = current_pcb().alloc_fd(write_file, None)?;

        fd[0] = read_fd;
        fd[1] = write_fd;

        return Ok(0);
    }

    pub fn kill(pid: pid_t, sig: c_int) -> Result<usize, SystemError> {
        let sig = SignalNumber::from(sig);
        if sig == SignalNumber::INVALID {
            // 传入的signal数值不合法
            kwarn!("Not a valid signal number");
            return Err(SystemError::EINVAL);
        }

        // 初始化signal info
        let mut info = siginfo {
            _sinfo: __siginfo_union {
                data: __siginfo_union_data {
                    si_signo: sig as i32,
                    si_code: si_code_val::SI_USER as i32,
                    si_errno: 0,
                    reserved: 0,
                    _sifields: super::signal_types::__sifields {
                        _kill: super::signal_types::__sifields__kill { _pid: pid },
                    },
                },
            },
        };
        compiler_fence(core::sync::atomic::Ordering::SeqCst);

        let retval = signal_kill_something_info(sig, Some(&mut info), pid).map(|x| x as usize);

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
        _from_user: bool,
    ) -> Result<usize, SystemError> {
        // 请注意：用户态传进来的user_sigaction结构体类型，请注意，这个结构体与内核实际的不一样
        let act = act as *mut user_sigaction;
        let mut old_act = old_act as *mut user_sigaction;
        let mut new_ka: sigaction = Default::default();
        let mut old_ka: sigaction = Default::default();

        // 如果传入的，新的sigaction不为空
        if !act.is_null() {
            // 如果参数的范围不在用户空间，则返回错误
            if unsafe {
                !verify_area(
                    act as usize as u64,
                    core::mem::size_of::<sigaction>() as u64,
                )
            } {
                return Err(SystemError::EFAULT);
            }
            let mask: sigset_t = unsafe { (*act).sa_mask };
            let _input_sah = unsafe { (*act).sa_handler as u64 };
            // kdebug!("_input_sah={}", _input_sah);
            match _input_sah {
                USER_SIG_DFL | USER_SIG_IGN => {
                    if _input_sah == USER_SIG_DFL {
                        new_ka = DEFAULT_SIGACTION;
                        new_ka.sa_flags = (unsafe { (*act).sa_flags }
                            & (!(SA_FLAG_DFL | SA_FLAG_IGN)))
                            | SA_FLAG_DFL;
                    } else {
                        new_ka = DEFAULT_SIGACTION_IGNORE;
                        new_ka.sa_flags = (unsafe { (*act).sa_flags }
                            & (!(SA_FLAG_DFL | SA_FLAG_IGN)))
                            | SA_FLAG_IGN;
                    }

                    let sar = unsafe { (*act).sa_restorer };
                    new_ka.sa_restorer = sar as u64;
                }
                _ => {
                    // 从用户空间获得sigaction结构体
                    new_ka = sigaction {
                        _u: sigaction__union_u {
                            _sa_handler: unsafe { (*act).sa_handler as u64 },
                        },
                        sa_flags: unsafe { (*act).sa_flags },
                        sa_mask: sigset_t::default(),
                        sa_restorer: unsafe { (*act).sa_restorer as u64 },
                    };
                }
            }
            // kdebug!("new_ka={:?}", new_ka);
            // 如果用户手动给了sa_restorer，那么就置位SA_FLAG_RESTORER，否则报错。（用户必须手动指定restorer）
            if new_ka.sa_restorer != NULL as u64 {
                new_ka.sa_flags |= SA_FLAG_RESTORER;
            } else {
                kwarn!(
                "pid:{}: in sys_sigaction: User must manually sprcify a sa_restorer for signal {}.",
                current_pcb().pid,
                sig
            );
            }
            sigset_init(&mut new_ka.sa_mask, mask);
        }

        let sig = SignalNumber::from(sig as i32);
        // 如果给出的信号值不合法
        if sig == SignalNumber::INVALID {
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
            if unsafe {
                !verify_area(
                    old_act as usize as u64,
                    core::mem::size_of::<sigaction>() as u64,
                )
            } {
                return Err(SystemError::EFAULT);
            }
            // ！！！！！！！！！！todo: 检查这里old_ka的mask，是否位SIG_IGN SIG_DFL,如果是，则将_sa_handler字段替换为对应的值
            let sah: u64;
            let flag = old_ka.sa_flags & (SA_FLAG_DFL | SA_FLAG_IGN);
            match flag {
                SA_FLAG_DFL => {
                    sah = USER_SIG_DFL;
                }
                SA_FLAG_IGN => {
                    sah = USER_SIG_IGN;
                }
                _ => sah = unsafe { old_ka._u._sa_handler },
            }
            unsafe {
                (*old_act).sa_handler = sah as *mut c_void;
                (*old_act).sa_flags = old_ka.sa_flags;
                (*old_act).sa_mask = old_ka.sa_mask;
                (*old_act).sa_restorer = old_ka.sa_restorer as *mut c_void;
            }
        }
        return retval.map(|_| 0);
    }
}
