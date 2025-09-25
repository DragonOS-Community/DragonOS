use crate::{
    arch::syscall::nr::SYS_CAPGET,
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::{UserBufferReader, UserBufferWriter},
    },
};
use alloc::{format, vec::Vec};
use core::ffi::c_int;
use system_error::SystemError;

/// Linux 用户态结构: cap_user_header_t
/// 参考 include/uapi/linux/capability.h
#[repr(C)]
#[derive(Clone, Copy)]
struct CapUserHeader {
    version: u32,
    pid: c_int,
}

/// Linux 用户态结构: cap_user_data_t（数组元素）
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapUserData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

// 版本常量（与 Linux 对齐）
const _LINUX_CAPABILITY_VERSION_1: u32 = 0x19980330;
const _LINUX_CAPABILITY_VERSION_2: u32 = 0x20071026; // deprecated
const _LINUX_CAPABILITY_VERSION_3: u32 = 0x20080522;

// 每版本 u32 数量
const _U32S_1: usize = 1;
const _U32S_2_3: usize = 2;

// DragonOS 支持版本（对齐 Linux v3）
const _KERNEL_CAPABILITY_VERSION: u32 = _LINUX_CAPABILITY_VERSION_3;

struct SysCapget;

impl Syscall for SysCapget {
    fn num_args(&self) -> usize {
        // capget(header, data)
        2
    }

    fn handle(
        &self,
        args: &[usize],
        frame: &mut crate::arch::interrupt::TrapFrame,
    ) -> Result<usize, SystemError> {
        let header_ptr = args[0] as *mut CapUserHeader;
        let data_ptr = args[1] as *mut CapUserData;

        // 读取 header
        let reader = UserBufferReader::new(
            header_ptr as *const CapUserHeader,
            core::mem::size_of::<CapUserHeader>(),
            frame.is_from_user(),
        )?;
        let hdr = *reader.read_one_from_user::<CapUserHeader>(0)?;

        // 版本协商
        let (tocopy, version_ok) = match hdr.version {
            _LINUX_CAPABILITY_VERSION_1 => (_U32S_1, true),
            _LINUX_CAPABILITY_VERSION_2 | _LINUX_CAPABILITY_VERSION_3 => (_U32S_2_3, true),
            _ => (_U32S_2_3, false),
        };

        if !version_ok {
            // 未知版本：写回支持的版本并返回 EINVAL
            let mut writer = UserBufferWriter::new(
                header_ptr,
                core::mem::size_of::<CapUserHeader>(),
                frame.is_from_user(),
            )?;
            let mut new_hdr = hdr;
            new_hdr.version = _KERNEL_CAPABILITY_VERSION;
            writer.copy_one_to_user(&new_hdr, 0)?;

            // 探测模式：dataptr == NULL 且版本不合法时返回 0
            if data_ptr.is_null() {
                return Ok(0);
            }
            return Err(SystemError::EINVAL);
        }

        // pid 检查
        let pid = hdr.pid;
        if pid < 0 {
            return Err(SystemError::EINVAL);
        }

        if pid != 0 {
            // 尝试定位目标任务；不做权限检查
            // 若找不到则返回 ESRCH；找到则继续（但我们不裁剪返回能力集）
            if ProcessManager::find_task_by_vpid(crate::process::RawPid(pid as usize)).is_none() {
                return Err(SystemError::ESRCH);
            }
        }

        // 准备返回数据：不限制权限 -> 全能力集（E/P/I = 0xFFFF_FFFF）
        let full = CapUserData {
            effective: u32::MAX,
            permitted: u32::MAX,
            inheritable: u32::MAX,
        };

        // 根据 tocopy 生成数组（v1:1, v2/v3:2）
        let mut kdata: Vec<CapUserData> = Vec::with_capacity(tocopy);
        for _ in 0..tocopy {
            kdata.push(full);
        }

        // 写回用户缓冲区
        if data_ptr.is_null() {
            // 合法版本但 dataptr 为空，按 Linux 行为返回 EFAULT
            return Err(SystemError::EFAULT);
        }
        let mut writer = UserBufferWriter::new(
            data_ptr,
            tocopy * core::mem::size_of::<CapUserData>(),
            frame.is_from_user(),
        )?;
        writer.copy_to_user(&kdata, 0)?;

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> alloc::vec::Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("header", format!("0x{:x}", args[0])),
            FormattedSyscallParam::new("data", format!("0x{:x}", args[1])),
        ]
    }
}

// 将该系统调用注册到系统调用表
syscall_table_macros::declare_syscall!(SYS_CAPGET, SysCapget);
