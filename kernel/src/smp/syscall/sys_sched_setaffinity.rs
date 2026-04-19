use alloc::string::ToString;
use alloc::vec::Vec;
use bitmap::traits::BitMapOps;
use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SCHED_SETAFFINITY;
use crate::libs::cpumask::CpuMask;
use crate::process::{ProcessManager, RawPid};
use crate::sched::syscall::util::has_sched_setaffinity_permission;
use crate::smp::cpu::smp_cpu_manager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferReader;

pub struct SysSchedSetaffinity;

impl Syscall for SysSchedSetaffinity {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pid = args[0] as i32;
        let size = args[1];
        let set_vaddr = args[2];

        if size == 0 {
            return Err(SystemError::EINVAL);
        }

        let target_pcb = if pid == 0 {
            ProcessManager::current_pcb()
        } else {
            ProcessManager::find_task_by_vpid(RawPid::from(pid as usize))
                .ok_or(SystemError::ESRCH)?
        };

        let current_pcb = ProcessManager::current_pcb();
        if !has_sched_setaffinity_permission(&current_pcb, &target_pcb) {
            return Err(SystemError::EPERM);
        }

        let reader = UserBufferReader::new(set_vaddr as *const u8, size, frame.is_from_user())?;
        let user_set = reader.buffer(0)?;
        let mask = Self::parse_user_mask(user_set)?;

        if mask.is_empty() {
            return Err(SystemError::EINVAL);
        }

        let possible_cpus = smp_cpu_manager().possible_cpus();
        for cpu in mask.iter_cpu() {
            if possible_cpus.get(cpu) != Some(true) {
                return Err(SystemError::EINVAL);
            }
        }

        target_pcb.sched_info().set_cpus_allowed(mask);
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", (args[0] as i32).to_string()),
            FormattedSyscallParam::new("size", args[1].to_string()),
            FormattedSyscallParam::new("set", format!("0x{:x}", args[2])),
        ]
    }
}

impl SysSchedSetaffinity {
    fn parse_user_mask(user_set: &[u8]) -> Result<CpuMask, SystemError> {
        let mut mask = CpuMask::new();
        let kernel_mask_bytes = unsafe { mask.inner().as_bytes().len() };
        let parse_len = core::cmp::min(user_set.len(), kernel_mask_bytes);

        for (byte_index, byte) in user_set[..parse_len].iter().enumerate() {
            if *byte == 0 {
                continue;
            }

            for bit in 0..8 {
                if (byte & (1 << bit)) == 0 {
                    continue;
                }

                let cpu_index = byte_index * 8 + bit;
                let cpu_id = crate::smp::cpu::ProcessorId::new(cpu_index as u32);
                if mask.set(cpu_id, true).is_none() {
                    return Err(SystemError::EINVAL);
                }
            }
        }

        if user_set[parse_len..].iter().any(|byte| *byte != 0) {
            return Err(SystemError::EINVAL);
        }

        Ok(mask)
    }
}

syscall_table_macros::declare_syscall!(SYS_SCHED_SETAFFINITY, SysSchedSetaffinity);
