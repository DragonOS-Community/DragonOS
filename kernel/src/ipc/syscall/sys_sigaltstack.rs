use alloc::string::ToString;
use alloc::vec::Vec;
use core::ffi::{c_int, c_void};

use crate::arch::interrupt::TrapFrame;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::{arch::syscall::nr::SYS_SIGALTSTACK, process::ProcessManager};
use system_error::SystemError;

use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};

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

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        //warn!("SYS_SIGALTSTACK has not yet been fully realized and still needs to be supplemented");
        //warn!("SYS_SIGALTSTACK has not yet been fully realized and still needs to be supplemented");
        //warn!("SYS_SIGALTSTACK has not yet been fully realized and still needs to be supplemented");

        let ss = Self::ss(args);
        let old_ss = Self::old_ss(args);

        let binding = ProcessManager::current_pcb();
        let mut stack = binding.sig_altstack_mut();

        if !old_ss.is_null() {
            // 需要从 current() 中读结构体写入 old_ss
            //log::info!("old_ss impl");
            let mut temp = StackUser::new();

            temp.ss_sp = stack.sp as *mut c_void;
            temp.ss_size = stack.size as usize;
            // temp.ss_flags = 0; 这个要根据情况设置

            let mut user_buffer = UserBufferWriter::new(old_ss, size_of::<StackUser>(), true)?;
            user_buffer.copy_one_to_user(&temp, 0)?;
        }

        if !ss.is_null() {
            // 需要向 current() 中结构体写入 ss 的内容
            //log::info!("ss impl");

            let user_buffer = UserBufferReader::new(ss, size_of::<StackUser>(), true)?;
            let sus: &[StackUser] = user_buffer.read_from_user(0)?;
            let ss: StackUser = sus[0];

            stack.sp = ss.ss_sp as usize;
            stack.size = ss.ss_size as u32;
            stack.flags = ss.ss_flags as u32;
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
