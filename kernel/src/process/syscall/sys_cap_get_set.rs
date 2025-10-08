use crate::{
    arch::syscall::nr::{SYS_CAPGET, SYS_CAPSET},
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::{UserBufferReader, UserBufferWriter},
    },
};
use alloc::format;
use alloc::vec::Vec;
use core::ffi::c_int;
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
#[derive(Clone, Copy)]
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

        // 读取 header
        let hdr_reader = UserBufferReader::new(
            header_ptr as *const CapUserHeader,
            core::mem::size_of::<CapUserHeader>(),
            frame.is_from_user(),
        )?;
        let hdr = *hdr_reader.read_one_from_user::<CapUserHeader>(0)?;

        // 版本协商
        let (tocopy, version_ok) = match hdr.version {
            _LINUX_CAPABILITY_VERSION_1 => (_U32S_1, true),
            _LINUX_CAPABILITY_VERSION_2 | _LINUX_CAPABILITY_VERSION_3 => (_U32S_2_3, true),
            _ => (_U32S_2_3, false),
        };

        if !version_ok {
            // 未知版本：capset 不承担探测职责，直接返回 EINVAL（更贴近 Linux 行为）
            return Err(SystemError::EINVAL);
        }

        // data 不能为空
        if data_ptr.is_null() {
            return Err(SystemError::EFAULT);
        }

        // pid 仅允许当前进程
        let pid = hdr.pid;
        if pid < 0 {
            // 与 Linux 语义一致：负 pid 视为不被允许的目标，返回 EPERM
            return Err(SystemError::EPERM);
        }
        let pcb = ProcessManager::current_pcb();
        if pid != 0 && pid as usize != pcb.raw_pid().data() {
            return Err(SystemError::EPERM);
        }

        // 读取用户数据
        let data_reader = UserBufferReader::new(
            data_ptr as *const CapUserData,
            tocopy * core::mem::size_of::<CapUserData>(),
            frame.is_from_user(),
        )?;
        let kdata = data_reader.read_from_user::<CapUserData>(0)?;
        let (p_e_new, p_p_new, p_i_new) = aggregate_u32s_to_u64(kdata);

        // 获取旧凭据
        let old = pcb.cred();
        let p_e_old = old.cap_effective;
        let p_p_old = old.cap_permitted.bits();
        let p_i_old = old.cap_inheritable.bits();
        let bset = old.cap_bset.bits();
        let _ambient_old = old.cap_ambient.bits();

        // 规则 1：pE_new ⊆ pP_new
        if (p_e_new & !p_p_new) != 0 {
            return Err(SystemError::EPERM);
        }

        // 规则 2：pP_new ⊆ pP_old（不允许提升）
        if (p_p_new & !p_p_old) != 0 {
            return Err(SystemError::EPERM);
        }

        // 规则 3：pI_new 限幅（对齐 Linux：受 CAP_SETPCAP 与 bset 约束）
        // - 拥有 CAP_SETPCAP：pI_new ⊆ (pI_old ∪ pP_old) ∩ bset
        // - 不拥有：pI_new ⊆ (pI_old ∪ pP_old) 且 pI_new ⊆ (pI_old ∪ bset)
        // 使用公开常量 CAP_SETPCAP_BIT 判定是否拥有 CAP_SETPCAP
        let has_setpcap = p_e_old.contains(CAPFlags::CAP_SETPCAP);
        if has_setpcap {
            let inh_cap_allow = (p_i_old | p_p_old) & bset;
            if (p_i_new & !inh_cap_allow) != 0 {
                return Err(SystemError::EPERM);
            }
        } else {
            let inh_cap_allow_1 = p_i_old | p_p_old;
            let inh_cap_allow_2 = p_i_old | bset;
            if (p_i_new & !inh_cap_allow_1) != 0 || (p_i_new & !inh_cap_allow_2) != 0 {
                return Err(SystemError::EPERM);
            }
        }

        // 构造新 cred（克隆老 cred，更新能力集）
        let mut new_cred = (*old).clone();
        new_cred.cap_effective = CAPFlags::from_bits_truncate(p_e_new);
        new_cred.cap_permitted = CAPFlags::from_bits_truncate(p_p_new);
        new_cred.cap_inheritable = CAPFlags::from_bits_truncate(p_i_new);
        // ambient 能力不由 capset 修改，保持不变

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

        let mut kdata: Vec<CapUserData> = Vec::with_capacity(tocopy);
        kdata.push(low);
        if tocopy == _U32S_2_3 {
            kdata.push(high);
        }

        // 写回用户缓冲区
        if data_ptr.is_null() {
            // 与当前 Linux 行为一致：版本合法且 dataptr==NULL 时返回 0
            return Ok(0);
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
