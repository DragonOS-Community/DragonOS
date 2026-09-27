use alloc::string::ToString;
use alloc::vec::Vec;
use bitmap::traits::BitMapOps;
use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SCHED_SETAFFINITY;
use crate::libs::cpumask::CpuMask;
use crate::mm::VirtAddr;
use crate::process::{kthread::KernelThreadFlags, ProcessFlags, ProcessManager, RawPid};
use crate::sched::syscall::util::has_sched_setaffinity_permission;
use crate::smp::cpu::smp_cpu_manager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::copy_from_user_protected;

pub struct SysSchedSetaffinity;

impl Syscall for SysSchedSetaffinity {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pid = args[0] as i32;
        let size = args[1] as u32 as usize;
        let set_vaddr = args[2];

        // Linux copies the truncated user mask before looking up the task.
        // A zero length does not touch the pointer, but PID/permission errors
        // still take precedence over the eventual empty-mask error.
        let kernel_mask_bytes = CpuMask::new().inner().size();
        let copy_len = core::cmp::min(size, kernel_mask_bytes);
        let mut user_set = vec![0u8; kernel_mask_bytes];
        if copy_len != 0 {
            unsafe {
                copy_from_user_protected(&mut user_set[..copy_len], VirtAddr::new(set_vaddr))?
            };
        }
        let mut mask = Self::parse_user_mask(&user_set);

        let target_pcb = if pid == 0 {
            ProcessManager::current_pcb()
        } else {
            ProcessManager::find_task_by_vpid(RawPid::from(pid as usize))
                .ok_or(SystemError::ESRCH)?
        };

        if target_pcb.flags().contains(ProcessFlags::KTHREAD) {
            let worker_private = target_pcb.worker_private();
            let is_per_cpu = worker_private
                .as_ref()
                .and_then(|private| private.kernel_thread())
                .is_some_and(|private| private.flags().contains(KernelThreadFlags::IS_PER_CPU));
            if is_per_cpu {
                return Err(SystemError::EINVAL);
            }
        }

        let current_pcb = ProcessManager::current_pcb();
        if !has_sched_setaffinity_permission(&current_pcb, &target_pcb) {
            return Err(SystemError::EPERM);
        }

        mask.bitand_assign(&smp_cpu_manager().online_cpus());

        if mask.is_empty() {
            return Err(SystemError::EINVAL);
        }

        // Keep affinity publication and the corresponding placement decision
        // in one pi_lock critical section inside the process manager.
        ProcessManager::set_cpus_allowed(&target_pcb, mask)?;

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
    fn parse_user_mask(user_set: &[u8]) -> CpuMask {
        let mut mask = CpuMask::new();
        for (byte_index, byte) in user_set.iter().enumerate() {
            if *byte == 0 {
                continue;
            }

            for bit in 0..8 {
                if (byte & (1 << bit)) == 0 {
                    continue;
                }

                let cpu_index = byte_index * 8 + bit;
                let cpu_id = crate::smp::cpu::ProcessorId::new(cpu_index as u32);
                mask.set(cpu_id, true);
            }
        }
        mask
    }
}

syscall_table_macros::declare_syscall!(SYS_SCHED_SETAFFINITY, SysSchedSetaffinity);
