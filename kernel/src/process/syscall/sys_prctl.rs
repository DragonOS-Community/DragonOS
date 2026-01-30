use alloc::string::ToString;
use core::{cmp, ffi::c_int, mem};
use num_traits::FromPrimitive;

use alloc::{
    borrow::ToOwned,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::process::cred::Cred;

use crate::{
    arch::{interrupt::TrapFrame, ipc::signal::Signal, syscall::nr::SYS_PRCTL},
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::{UserBufferReader, UserBufferWriter},
    },
};

const TASK_COMM_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, FromPrimitive)]
#[repr(usize)]
enum PrctlOption {
    SetPDeathSig = 1,
    GetPDeathSig = 2,
    GetDumpable = 3,
    SetDumpable = 4,
    SetKeepCaps = 8,
    GetKeepCaps = 9,
    SetName = 15,
    GetName = 16,
    CapBsetRead = 23,
    CapBsetDrop = 24,

    SetMm = 35,

    SetChildSubreaper = 36,
    GetChildSubreaper = 37,

    SetNoNewPrivs = 38,
    GetNoNewPrivs = 39,
}

impl TryFrom<usize> for PrctlOption {
    type Error = SystemError;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        PrctlOption::from_usize(value).ok_or(SystemError::EINVAL)
    }
}

pub struct SysPrctl;

impl Syscall for SysPrctl {
    fn num_args(&self) -> usize {
        5
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        if args.len() < 5 {
            return Err(SystemError::EINVAL);
        }

        let option = PrctlOption::try_from(args[0])?;
        let arg2 = args[1];
        let from_user = frame.is_from_user();
        let current = ProcessManager::current_pcb();

        // prctl 的部分选项在 Linux 中具有“线程组/进程级”语义。
        // DragonOS 当前没有显式的 signal_struct / thread_group 抽象，
        // 这里使用线程组 leader 来承载该类状态。
        let thread_group_leader = get_thread_group_leader(&current);

        match option {
            PrctlOption::GetDumpable => Ok(current.dumpable().into()),
            PrctlOption::SetDumpable => {
                // Linux: PR_SET_DUMPABLE 允许设置为 0/1；2(SUID_DUMP_ROOT) 不允许。
                // 参考 gVisor: RootDumpability / SetGetDumpability。
                let val = arg2 as i32;
                match val {
                    0 | 1 => {
                        current.set_dumpable(val as u8);
                        Ok(0)
                    }
                    _ => Err(SystemError::EINVAL),
                }
            }
            PrctlOption::SetKeepCaps => {
                // Linux: arg2 非 0 表示置位，0 表示清除。
                let v = arg2 as isize;
                let enable = v != 0;
                current.set_keepcaps(enable);
                Ok(0)
            }
            PrctlOption::GetKeepCaps => {
                // Linux: 返回 1 表示置位，0 表示未置位。
                let value: c_int = if current.keepcaps() { 1 } else { 0 };
                // 注意：根据 prctl 的语义，返回值应该直接返回，而不是写入用户空间。
                // PR_GET_KEEPCAPS 的返回值就是当前状态，不需要额外的参数。
                Ok(value as usize)
            }
            PrctlOption::SetPDeathSig => {
                let signal = parse_pdeathsig(arg2)?;
                current.set_pdeath_signal(signal);
                Ok(0)
            }
            PrctlOption::GetPDeathSig => {
                let dest = arg2 as *mut c_int;
                if dest.is_null() {
                    return Err(SystemError::EFAULT);
                }
                let mut writer = UserBufferWriter::new(dest, mem::size_of::<c_int>(), from_user)?;
                let sig = current.pdeath_signal();
                let value: c_int = if sig == Signal::INVALID {
                    0
                } else {
                    sig as c_int
                };
                writer.copy_one_to_user(&value, 0)?;
                Ok(0)
            }
            PrctlOption::SetName => {
                let name_ptr = arg2 as *const u8;
                if name_ptr.is_null() {
                    return Err(SystemError::EFAULT);
                }
                let comm = read_comm_buffer(name_ptr, from_user)?;
                let name = comm_buffer_to_string(&comm);
                current.set_name(name);
                Ok(0)
            }
            PrctlOption::GetName => {
                let dest = arg2 as *mut u8;
                if dest.is_null() {
                    return Err(SystemError::EFAULT);
                }
                let name = current.basic().name().to_string();
                write_comm_buffer(dest, from_user, name.as_bytes())?;
                Ok(0)
            }

            PrctlOption::SetMm => {
                // gVisor: PR_SET_MM 在缺少 CAP_SYS_RESOURCE 时必须返回 EPERM。
                let cred = current.cred();
                if !cred.has_capability(crate::process::cred::CAPFlags::CAP_SYS_RESOURCE) {
                    return Err(SystemError::EPERM);
                }

                // 其余 PR_SET_MM 子操作目前未实现。
                Err(SystemError::EINVAL)
            }

            PrctlOption::SetChildSubreaper => {
                // Linux: 任何非 0 值都表示置 1；0 表示清除。
                let v = arg2 as isize;
                let enable = v != 0;
                thread_group_leader
                    .sig_info_mut()
                    .set_is_child_subreaper(enable);
                Ok(0)
            }
            PrctlOption::GetChildSubreaper => {
                let dest = arg2 as *mut c_int;
                if dest.is_null() {
                    return Err(SystemError::EFAULT);
                }
                let mut writer = UserBufferWriter::new(dest, mem::size_of::<c_int>(), from_user)?;
                let is_subreaper = thread_group_leader.sig_info_irqsave().is_child_subreaper();
                let value: c_int = if is_subreaper { 1 } else { 0 };
                writer.copy_one_to_user(&value, 0)?;
                Ok(0)
            }

            PrctlOption::CapBsetRead => {
                // PR_CAPBSET_READ: 检查某个 capability 是否在 bounding set 中
                // arg2 是 capability 的编号
                use crate::process::cred::CAPFlags;
                let cap_bit = 1usize << arg2;
                let cap_flag =
                    CAPFlags::from_bits(cap_bit as u64).unwrap_or(CAPFlags::CAP_EMPTY_SET);
                let cred = current.cred();
                let has_cap = cred.cap_bset.contains(cap_flag);
                Ok(if has_cap { 1 } else { 0 })
            }
            PrctlOption::CapBsetDrop => {
                // PR_CAPBSET_DROP: 从 bounding set 中删除某个 capability
                // arg2 是 capability 的编号
                use crate::process::cred::CAPFlags;
                let cap_bit = 1usize << arg2;
                let cap_flag =
                    CAPFlags::from_bits(cap_bit as u64).unwrap_or(CAPFlags::CAP_EMPTY_SET);

                // 获取当前 cred，克隆并修改 cap_bset，然后设置回去
                let old_cred = current.cred();
                let new_bset = old_cred.cap_bset & !cap_flag;

                // 创建新的 cred
                let new_cred = Arc::new(Cred {
                    self_ref: Weak::new(),
                    uid: old_cred.uid,
                    gid: old_cred.gid,
                    suid: old_cred.suid,
                    sgid: old_cred.sgid,
                    euid: old_cred.euid,
                    egid: old_cred.egid,
                    groups: old_cred.groups.clone(),
                    fsuid: old_cred.fsuid,
                    fsgid: old_cred.fsgid,
                    cap_inheritable: old_cred.cap_inheritable,
                    cap_permitted: old_cred.cap_permitted,
                    cap_effective: old_cred.cap_effective,
                    cap_bset: new_bset,
                    cap_ambient: old_cred.cap_ambient,
                    group_info: old_cred.group_info.clone(),
                    user_ns: old_cred.user_ns.clone(),
                });

                current.set_cred(new_cred)?;
                Ok(0)
            }

            PrctlOption::SetNoNewPrivs => {
                // Linux: arg2 必须为 1；no_new_privs 一旦置位不可清除。
                if arg2 != 1 {
                    return Err(SystemError::EINVAL);
                }
                current.set_no_new_privs(true);
                Ok(0)
            }
            PrctlOption::GetNoNewPrivs => Ok(current.no_new_privs()),
        }
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        let option_val = args.first().copied().unwrap_or(0);
        let option_str = if let Ok(option) = PrctlOption::try_from(option_val) {
            format!("{:#x} ({:?})", option_val, option)
        } else {
            format!("{:#x}", option_val)
        };

        vec![
            FormattedSyscallParam::new("option", option_str),
            FormattedSyscallParam::new("arg2", format!("{:#x}", args.get(1).copied().unwrap_or(0))),
            FormattedSyscallParam::new("arg3", format!("{:#x}", args.get(2).copied().unwrap_or(0))),
            FormattedSyscallParam::new("arg4", format!("{:#x}", args.get(3).copied().unwrap_or(0))),
            FormattedSyscallParam::new("arg5", format!("{:#x}", args.get(4).copied().unwrap_or(0))),
        ]
    }
}

fn parse_pdeathsig(value: usize) -> Result<Signal, SystemError> {
    if value == 0 {
        return Ok(Signal::INVALID);
    }
    let sig = Signal::from(value);
    if sig.is_valid() {
        Ok(sig)
    } else {
        Err(SystemError::EINVAL)
    }
}

fn read_comm_buffer(
    ptr_src: *const u8,
    from_user: bool,
) -> Result<[u8; TASK_COMM_LEN], SystemError> {
    let mut comm = [0u8; TASK_COMM_LEN];
    let reader = UserBufferReader::new(ptr_src, TASK_COMM_LEN, from_user)?;
    reader.copy_from_user_protected(&mut comm, 0)?;
    comm[TASK_COMM_LEN - 1] = 0;
    Ok(comm)
}

fn comm_buffer_to_string(buffer: &[u8; TASK_COMM_LEN]) -> String {
    let len = buffer
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(TASK_COMM_LEN - 1);
    let slice = &buffer[..len];
    match core::str::from_utf8(slice) {
        Ok(s) => s.to_owned(),
        Err(_) => String::from_utf8_lossy(slice).into_owned(),
    }
}

fn write_comm_buffer(dest: *mut u8, from_user: bool, name_bytes: &[u8]) -> Result<(), SystemError> {
    let mut comm = [0u8; TASK_COMM_LEN];
    let copy_len = cmp::min(name_bytes.len(), TASK_COMM_LEN - 1);
    comm[..copy_len].copy_from_slice(&name_bytes[..copy_len]);
    let mut writer = UserBufferWriter::new(dest, TASK_COMM_LEN, from_user)?;
    writer.copy_to_user_protected(&comm, 0)?;
    Ok(())
}

syscall_table_macros::declare_syscall!(SYS_PRCTL, SysPrctl);

fn get_thread_group_leader(
    pcb: &alloc::sync::Arc<crate::process::ProcessControlBlock>,
) -> alloc::sync::Arc<crate::process::ProcessControlBlock> {
    let ti = pcb.threads_read_irqsave();
    ti.group_leader().unwrap_or_else(|| pcb.clone())
}
