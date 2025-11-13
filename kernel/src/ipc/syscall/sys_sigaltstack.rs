use alloc::string::ToString;
use alloc::vec::Vec;
use core::ffi::{c_int, c_void};

use crate::arch::interrupt::TrapFrame;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::{arch::syscall::nr::SYS_SIGALTSTACK, process::ProcessManager};
use system_error::SystemError;

use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};

// 根据Linux UAPI定义
const SS_ONSTACK: c_int = 1;
const SS_DISABLE: c_int = 2;

// 最小信号栈大小
const MINSIGSTKSZ: usize = 2048;

/// C 中定义的信号栈, 等于 C 中的 stack_t
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct StackUser {
    pub ss_sp: *mut c_void, // 栈的基地址
    pub ss_flags: c_int,    // 标志
    pub ss_size: usize,     // 栈的字节数
}

impl StackUser {
    pub fn new() -> Self {
        Self {
            ss_sp: core::ptr::null_mut(),
            ss_flags: 0,
            ss_size: 0,
        }
    }
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

        let binding = ProcessManager::current_pcb();
        let mut stack = binding.sig_altstack_mut();
        let is_on_stack = stack.on_sig_stack(frame.rsp as usize);

        if !old_ss.is_null() {
            let mut old_stack_user = StackUser::new();
            old_stack_user.ss_sp = stack.sp as *mut c_void;
            old_stack_user.ss_size = stack.size as usize;

            if stack.flags == SS_DISABLE as u32 {
                old_stack_user.ss_flags = SS_DISABLE;
            } else if is_on_stack {
                old_stack_user.ss_flags = SS_ONSTACK;
            } else {
                old_stack_user.ss_flags = 0; // 栈已启用但当前不在其上
            }

            let mut writer = UserBufferWriter::new(old_ss, size_of::<StackUser>(), true)?;
            writer.copy_one_to_user(&old_stack_user, 0)?;
        }

        if !ss.is_null() {
            // 需要向 current() 中结构体写入 ss 的内容
            //log::info!("ss impl");
            if is_on_stack {
                return Err(SystemError::EPERM);
            }

            let reader = UserBufferReader::new(ss, size_of::<StackUser>(), true)?;
            let sus: &[StackUser] = reader.read_from_user(0)?;
            let ss: StackUser = sus[0];

            // stack.sp = ss.ss_sp as usize;
            // stack.flags = ss.ss_flags as u32;
            // stack.size = ss.ss_size as u32;
            if (ss.ss_flags & !SS_DISABLE) != 0 {
                return Err(SystemError::EINVAL);
            }
            // 如果用户请求禁用备用栈
            if ss.ss_flags & SS_DISABLE != 0 {
                stack.flags = SS_DISABLE as u32;
            } else {
                // 如果用户请求设置一个新的栈
                if ss.ss_sp.is_null() {
                    return Err(SystemError::EFAULT); // 或者 EINVAL ?
                }
                if ss.ss_size < MINSIGSTKSZ {
                    return Err(SystemError::ENOMEM);
                }
                stack.sp = ss.ss_sp as usize;
                stack.flags = 0; // 标记为已启用
                stack.size = ss.ss_size as u32;
            }
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
