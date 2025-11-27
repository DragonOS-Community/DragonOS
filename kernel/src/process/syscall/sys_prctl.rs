use alloc::string::ToString;
use core::{cmp, ffi::c_int, mem};
use num_traits::FromPrimitive;

use alloc::{borrow::ToOwned, string::String, vec::Vec};
use system_error::SystemError;

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
    SetName = 15,
    GetName = 16,
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

        match option {
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
