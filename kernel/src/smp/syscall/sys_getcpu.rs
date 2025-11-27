//! SYS_GETCPU 系统调用实现
//!
//! 获取当前线程运行的 CPU 编号和 NUMA 节点编号。
//! DragonOS 不支持 NUMA，因此所有 CPU 返回节点 0。

use alloc::vec::Vec;
use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_GETCPU;
use crate::smp::core::smp_get_processor_id;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferWriter;

pub struct SysGetcpu;

impl Syscall for SysGetcpu {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let cpu_ptr = args[0] as *mut u32;
        let node_ptr = args[1] as *mut u32;
        let _cache_ptr = args[2] as *mut u8;

        // 获取当前 CPU ID
        let cpu_id = smp_get_processor_id();
        let cpu_num = cpu_id.data();

        // 如果需要返回 CPU 编号
        if !cpu_ptr.is_null() {
            // 验证用户空间指针并写入数据
            let mut writer = UserBufferWriter::new(
                cpu_ptr as *mut u8,
                core::mem::size_of::<u32>(),
                frame.is_from_user(),
            )?;
            let buffer = writer.buffer::<u32>(0)?;
            buffer[0] = cpu_num;
        }

        // 如果需要返回 NUMA 节点编号
        if !node_ptr.is_null() {
            // DragonOS 不支持 NUMA，所有 CPU 都在节点 0
            let mut writer = UserBufferWriter::new(
                node_ptr as *mut u8,
                core::mem::size_of::<u32>(),
                frame.is_from_user(),
            )?;
            let buffer = writer.buffer::<u32>(0)?;
            buffer[0] = 0; // 固定返回节点 0
        }

        // 第三个参数 cache 在 Linux 中也未使用，直接忽略

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("cpu", format!("0x{:x}", args[0])),
            FormattedSyscallParam::new("node", format!("0x{:x}", args[1])),
            FormattedSyscallParam::new("cache", format!("0x{:x}", args[2])),
        ]
    }
}

// 注册系统调用
syscall_table_macros::declare_syscall!(SYS_GETCPU, SysGetcpu);
