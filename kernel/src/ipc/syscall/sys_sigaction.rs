use super::super::signal_types::{
    SaHandlerType, Sigaction, SigactionType, UserSigaction, USER_SIG_DFL, USER_SIG_ERR,
    USER_SIG_IGN,
};
use crate::arch::syscall::nr::SYS_RT_SIGACTION;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::{
    arch::ipc::signal::{SigFlags, SigSet, Signal},
    mm::VirtAddr,
    process::ProcessManager,
    syscall::user_access::UserBufferWriter,
};
use alloc::vec::Vec;
use core::ffi::{c_int, c_void};
use log::error;
use system_error::SystemError;

pub struct SysSigactionHandle;

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
pub(super) fn do_kernel_sigaction(
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
                new_ka = Sigaction::DEFAULT_SIGACTION;
                *new_ka.flags_mut() = unsafe { (*act).flags };
                new_ka.set_restorer(None);
            }

            USER_SIG_IGN => {
                new_ka = Sigaction::DEFAULT_SIGACTION_IGNORE;
                *new_ka.flags_mut() = unsafe { (*act).flags };

                new_ka.set_restorer(None);
            }
            _ => {
                // 从用户空间获得sigaction结构体
                // TODO mask是default还是用户空间传入
                new_ka = Sigaction::new(
                    SigactionType::SaHandler(SaHandlerType::Customized(unsafe {
                        VirtAddr::new((*act).handler as usize)
                    })),
                    unsafe { (*act).flags },
                    SigSet::default(),
                    unsafe { Some(VirtAddr::new((*act).restorer as usize)) },
                );
            }
        }

        // TODO 如果为空，赋默认值？
        // debug!("new_ka={:?}", new_ka);
        // 如果用户手动给了sa_restorer，那么就置位SA_FLAG_RESTORER，否则报错。（用户必须手动指定restorer）
        if new_ka.restorer().is_some() {
            new_ka.flags_mut().insert(SigFlags::SA_RESTORER);
        } else if new_ka.action().is_customized() {
            error!(
                "pid:{:?}: in sys_sigaction: User must manually sprcify a sa_restorer for signal {}.",
                ProcessManager::current_pcb().pid(),
                sig
            );
            return Err(SystemError::EINVAL);
        }
        *new_ka.mask_mut() = mask;
    }

    let sig = Signal::from(sig);
    // 如果给出的信号值不合法
    if sig == Signal::INVALID {
        return Err(SystemError::EINVAL);
    }

    let retval = super::super::signal::do_sigaction(
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
        let r = UserBufferWriter::new(old_act, core::mem::size_of::<UserSigaction>(), from_user);
        if r.is_err() {
            return Err(SystemError::EFAULT);
        }

        let sigaction_handler = match old_sigaction.action() {
            SigactionType::SaHandler(handler) => {
                if let SaHandlerType::Customized(hand) = handler {
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
                error!("unsupported type: SaSigaction");
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

impl SysSigactionHandle {
    #[inline(always)]
    fn sig(args: &[usize]) -> c_int {
        // 第一个参数是信号值
        args[0] as c_int
    }
    #[inline(always)]
    fn act(args: &[usize]) -> usize {
        // 第二个参数是用户空间传入的 Sigaction 指针
        args[1]
    }
    #[inline(always)]
    fn old_act(args: &[usize]) -> usize {
        // 第三个参数是用户空间传入的用来保存旧 Sigaction 的指针
        args[2]
    }
}

impl Syscall for SysSigactionHandle {
    fn num_args(&self) -> usize {
        3
    }

    #[no_mangle]
    fn handle(&self, args: &[usize], from_user: bool) -> Result<usize, SystemError> {
        let sig = Self::sig(args);
        let act = Self::act(args);
        let old_act = Self::old_act(args);

        do_kernel_sigaction(sig, act, old_act, from_user)
    }
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("sig", format!("{}", Self::sig(args))),
            FormattedSyscallParam::new("act", format!("{:#x}", Self::act(args))),
            FormattedSyscallParam::new("old_act", format!("{:#x}", Self::old_act(args))),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_RT_SIGACTION, SysSigactionHandle);
