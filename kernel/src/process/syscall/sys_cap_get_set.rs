use crate::{
    arch::syscall::nr::{SYS_CAPGET, SYS_CAPSET},
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::{UserBufferReader, UserBufferWriter},
    },
};
use alloc::format;
use core::ffi::c_int;
use core::mem::size_of;
use system_error::SystemError;

use super::super::cred::{CAPFlags, Cred};

/// Linux 用户态结构: cap_user_header_t
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

fn cap_validate_magic_user(
    header_ptr: *mut CapUserHeader,
    from_user: bool,
) -> Result<usize, SystemError> {
    let version_reader = UserBufferReader::new(
        header_ptr.cast::<u32>() as *const u32,
        size_of::<u32>(),
        from_user,
    )?;
    let version = version_reader.buffer_protected(0)?.read_one::<u32>(0)?;

    match version {
        _LINUX_CAPABILITY_VERSION_1 => Ok(_U32S_1),
        _LINUX_CAPABILITY_VERSION_2 | _LINUX_CAPABILITY_VERSION_3 => Ok(_U32S_2_3),
        _ => {
            let mut version_writer =
                UserBufferWriter::new(header_ptr.cast::<u32>(), size_of::<u32>(), from_user)?;
            version_writer
                .buffer_protected(0)?
                .write_one(0, &_KERNEL_CAPABILITY_VERSION)?;
            Err(SystemError::EINVAL)
        }
    }
}

struct SysCapset;

impl Syscall for SysCapset {
    fn num_args(&self) -> usize {
        // capset(header, data)
        2
    }

    fn handle(
        &self,
        args: &[usize],
        frame: &mut crate::arch::interrupt::TrapFrame,
    ) -> Result<usize, SystemError> {
        let header_ptr = args[0] as *mut CapUserHeader;
        let data_ptr = args[1] as *mut CapUserData;
        let from_user = frame.is_from_user();

        let tocopy = cap_validate_magic_user(header_ptr, from_user)?;

        // pid 仅允许当前进程
        let pid_ptr = (header_ptr as usize + size_of::<u32>()) as *const c_int;
        let pid_reader = UserBufferReader::new(pid_ptr, size_of::<c_int>(), from_user)?;
        let pid = pid_reader.buffer_protected(0)?.read_one::<c_int>(0)?;
        let pcb = ProcessManager::current_pcb();
        if pid != 0 && pid as usize != pcb.raw_pid().data() {
            return Err(SystemError::EPERM);
        }

        // 读取用户数据
        let mut kdata = [CapUserData::default(); _U32S_2_3];
        let data_len = tocopy
            .checked_mul(size_of::<CapUserData>())
            .ok_or(SystemError::EINVAL)?;
        let data_reader =
            UserBufferReader::new(data_ptr as *const CapUserData, data_len, from_user)?;
        let kdata_bytes =
            unsafe { core::slice::from_raw_parts_mut(kdata.as_mut_ptr().cast::<u8>(), data_len) };
        let read_len = data_reader
            .buffer_protected(0)?
            .read_from_user(0, kdata_bytes)?;
        if read_len != data_len {
            return Err(SystemError::EFAULT);
        }
        let (p_e_new, p_p_new, p_i_new) = aggregate_u32s_to_u64(&kdata);

        // 获取旧凭据
        let old = pcb.cred();
        let p_p_old = old.cap_permitted.bits();
        let p_i_old = old.cap_inheritable.bits();
        let bset = old.cap_bset.bits();

        // 规则 1：pE_new ⊆ pP_new
        if (p_e_new & !p_p_new) != 0 {
            return Err(SystemError::EPERM);
        }

        // 规则 2：pP_new ⊆ pP_old（不允许提升）
        if (p_p_new & !p_p_old) != 0 {
            return Err(SystemError::EPERM);
        }

        // 规则 3：pI_new 限幅（对齐 Linux 6.6 security/commoncap.c cap_capset）
        let inh_capped = !old.has_capability(CAPFlags::CAP_SETPCAP);
        if inh_capped && (p_i_new & !(p_i_old | p_p_old)) != 0 {
            return Err(SystemError::EPERM);
        }
        if (p_i_new & !(p_i_old | bset)) != 0 {
            return Err(SystemError::EPERM);
        }

        // 构造新 cred（克隆老 cred，更新能力集）
        let mut new_cred = (*old).clone();
        new_cred.cap_effective = CAPFlags::from_bits_truncate(p_e_new);
        new_cred.cap_permitted = CAPFlags::from_bits_truncate(p_p_new);
        new_cred.cap_inheritable = CAPFlags::from_bits_truncate(p_i_new);
        // ambient：与 Linux 一致，裁剪为 permitted ∩ inheritable 的子集
        new_cred.cap_ambient &= CAPFlags::from_bits_truncate(p_p_new);
        new_cred.cap_ambient &= CAPFlags::from_bits_truncate(p_i_new);

        // 原子替换 cred（需要 PCB 暴露 set_cred）
        pcb.set_cred(Cred::new_arc(new_cred))?;

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> alloc::vec::Vec<FormattedSyscallParam> {
        alloc::vec![
            FormattedSyscallParam::new("header", format!("0x{:x}", args[0])),
            FormattedSyscallParam::new("data", format!("0x{:x}", args[1])),
        ]
    }
}

fn aggregate_u32s_to_u64(kdata: &[CapUserData]) -> (u64, u64, u64) {
    // v1: 仅 index 0；v2/v3: index 0 为低 32 位，index 1 为高 32 位
    let low = kdata.first().copied().unwrap_or(CapUserData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    });
    let high = kdata.get(1).copied().unwrap_or(CapUserData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    });

    let e = (low.effective as u64) | ((high.effective as u64) << 32);
    let p = (low.permitted as u64) | ((high.permitted as u64) << 32);
    let i = (low.inheritable as u64) | ((high.inheritable as u64) << 32);

    // DragonOS CAPFlags 仅支持低 41 位，截断高位以保持一致
    let mask = CAPFlags::CAP_FULL_SET.bits();
    (e & mask, p & mask, i & mask)
}

// 将该系统调用注册到系统调用表
syscall_table_macros::declare_syscall!(SYS_CAPSET, SysCapset);

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
        let from_user = frame.is_from_user();

        let tocopy = match cap_validate_magic_user(header_ptr, from_user) {
            Ok(tocopy) => tocopy,
            Err(SystemError::EINVAL) if data_ptr.is_null() => return Ok(0),
            Err(err) => return Err(err),
        };

        if data_ptr.is_null() {
            return Ok(0);
        }

        // pid 检查
        let pid_ptr = (header_ptr as usize + size_of::<u32>()) as *const c_int;
        let pid_reader = UserBufferReader::new(pid_ptr, size_of::<c_int>(), from_user)?;
        let pid = pid_reader.buffer_protected(0)?.read_one::<c_int>(0)?;
        if pid < 0 {
            return Err(SystemError::EINVAL);
        }

        // 确定目标任务的 cred：pid==0 使用当前进程；pid!=0 返回目标任务 cred
        let cred = if pid != 0 {
            if let Some(task) =
                ProcessManager::find_task_by_vpid(crate::process::RawPid(pid as usize))
            {
                task.cred()
            } else {
                return Err(SystemError::ESRCH);
            }
        } else {
            ProcessManager::current_pcb().cred()
        };
        let e = cred.cap_effective.bits();
        let p = cred.cap_permitted.bits();
        let i = cred.cap_inheritable.bits();

        // v1: 仅低 32 位；v2/v3: 低 32 + 高 32
        let low = CapUserData {
            effective: (e & 0xFFFF_FFFF) as u32,
            permitted: (p & 0xFFFF_FFFF) as u32,
            inheritable: (i & 0xFFFF_FFFF) as u32,
        };
        let high = CapUserData {
            effective: ((e >> 32) & 0xFFFF_FFFF) as u32,
            permitted: ((p >> 32) & 0xFFFF_FFFF) as u32,
            inheritable: ((i >> 32) & 0xFFFF_FFFF) as u32,
        };

        let kdata = [low, high];

        // 写回用户缓冲区
        let data_len = tocopy
            .checked_mul(size_of::<CapUserData>())
            .ok_or(SystemError::EINVAL)?;
        let mut writer = UserBufferWriter::new(data_ptr, data_len, from_user)?;
        let kdata_bytes =
            unsafe { core::slice::from_raw_parts(kdata.as_ptr().cast::<u8>(), data_len) };
        let write_len = writer.buffer_protected(0)?.write_to_user(0, kdata_bytes)?;
        if write_len != data_len {
            return Err(SystemError::EFAULT);
        }

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> alloc::vec::Vec<FormattedSyscallParam> {
        alloc::vec![
            FormattedSyscallParam::new("header", format!("0x{:x}", args[0])),
            FormattedSyscallParam::new("data", format!("0x{:x}", args[1])),
        ]
    }
}

// 将该系统调用注册到系统调用表
syscall_table_macros::declare_syscall!(SYS_CAPGET, SysCapget);
