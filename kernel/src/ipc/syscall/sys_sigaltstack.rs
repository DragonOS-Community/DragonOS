use alloc::string::ToString;
use alloc::vec::Vec;
use core::ffi::c_void;

use crate::arch::{
    interrupt::TrapFrame,
    ipc::signal::{SigStackFlags, MINSIGSTKSZ},
};
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::{arch::syscall::nr::SYS_SIGALTSTACK, process::ProcessManager};
use system_error::SystemError;

use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};

/// C 中定义的信号栈, 等于 C 中的 stack_t
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct StackUser {
    pub ss_sp: *mut c_void,      // 栈的基地址
    pub ss_flags: SigStackFlags, // 标志
    pub ss_size: usize,          // 栈的字节数
}

pub struct SysAltStackHandle;

impl SysAltStackHandle {
    #[inline(always)]
    fn ss(args: &[usize]) -> *const StackUser {
        // 第一个参数是 ss
        args[0] as *const StackUser
    }
    #[inline(always)]
    fn old_ss(args: &[usize]) -> *mut StackUser {
        // 第二个参数是 old_ss
        args[1] as *mut StackUser
    }
}

impl Syscall for SysAltStackHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        //warn!("SYS_SIGALTSTACK has not yet been fully realized and still needs to be supplemented");
        //warn!("SYS_SIGALTSTACK has not yet been fully realized and still needs to be supplemented");
        //warn!("SYS_SIGALTSTACK has not yet been fully realized and still needs to be supplemented");

        let ss = Self::ss(args);
        let old_ss = Self::old_ss(args);

        let new_stack_user = if !ss.is_null() {
            let reader = UserBufferReader::new(ss, size_of::<StackUser>(), true)?;
            Some(reader.read_one_from_user::<StackUser>(0)?)
        } else {
            None
        };

        let pcb = ProcessManager::current_pcb();
        let old_stack_user = {
            let mut stack = pcb.sig_altstack_mut();
            let is_on_stack = stack.on_sig_stack(frame.stack_pointer());

            let old_stack_user = if !old_ss.is_null() {
                let mut old_stack_user = StackUser {
                    ss_sp: stack.sp as *mut c_void,
                    ss_size: stack.size,
                    ..Default::default()
                };

                if stack.size == 0 {
                    old_stack_user.ss_flags = SigStackFlags::SS_DISABLE;
                } else if is_on_stack {
                    old_stack_user.ss_flags = SigStackFlags::SS_ONSTACK;
                }
                if stack.flags.contains(SigStackFlags::SS_AUTODISARM) {
                    old_stack_user.ss_flags |= SigStackFlags::SS_AUTODISARM;
                }

                Some(old_stack_user)
            } else {
                None
            };

            if let Some(ss) = new_stack_user {
                if is_on_stack {
                    return Err(SystemError::EPERM);
                }

                let ss_mode = ss.ss_flags.difference(SigStackFlags::SS_AUTODISARM);
                if !(ss_mode.is_empty()
                    || ss_mode == SigStackFlags::SS_DISABLE
                    || ss_mode == SigStackFlags::SS_ONSTACK)
                {
                    return Err(SystemError::EINVAL);
                }
                // If the user requests disabling the alternate stack
                if ss.ss_flags.contains(SigStackFlags::SS_DISABLE) {
                    stack.sp = 0;
                    stack.flags = ss.ss_flags;
                    stack.size = 0;
                } else {
                    // If the user requests setting a new stack
                    if ss.ss_size < MINSIGSTKSZ {
                        return Err(SystemError::ENOMEM);
                    }
                    stack.sp = ss.ss_sp as usize;
                    stack.flags = ss.ss_flags; // Preserve SS_AUTODISARM and other flags
                    stack.size = ss.ss_size;
                }
            }

            old_stack_user
        };

        if let Some(old_stack_user) = old_stack_user {
            let mut writer = UserBufferWriter::new(old_ss, size_of::<StackUser>(), true)?;
            writer.copy_one_to_user(&old_stack_user, 0)?;
        }
        Ok(0)
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("ss: ", "not impl".to_string()),
            FormattedSyscallParam::new("old_ss: ", "not impl".to_string()),
        ]
    }
}

// 注册系统调用
syscall_table_macros::declare_syscall!(SYS_SIGALTSTACK, SysAltStackHandle);
