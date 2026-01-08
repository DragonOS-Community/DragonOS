//! mlockall 系统调用实现

use crate::arch::{interrupt::TrapFrame, syscall::nr::SYS_MLOCKALL, MMArch};
use crate::mm::MemoryManagementArch;
use crate::mm::VmFlags;
use crate::mm::{mlock::can_do_mlock, syscall::MlockAllFlags, ucontext::AddressSpace};
use crate::process::{resource::RLimitID, ProcessManager};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysMlockallHandle;

impl Syscall for SysMlockallHandle {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let flags = MlockAllFlags::from_bits(args[0] as u32).ok_or(SystemError::EINVAL)?;

        // 检查标志位组合合法性
        // 参考 Linux: mm/mlock.c:do_mlockall()
        // 必须至少指定 MCL_CURRENT 或 MCL_FUTURE 之一
        // MCL_ONFAULT 必须与 MCL_CURRENT 或 MCL_FUTURE 一起使用
        if !flags.intersects(MlockAllFlags::MCL_CURRENT | MlockAllFlags::MCL_FUTURE) {
            return Err(SystemError::EINVAL);
        }

        // 权限检查
        if !can_do_mlock() {
            return Err(SystemError::EPERM);
        }

        let addr_space = AddressSpace::current()?;

        // RLIMIT_MEMLOCK 检查
        // 参考 Linux: mm/mlock.c:do_mlockall()
        // 对于 MCL_CURRENT，需要计算当前所有可访问 VMA 的总大小
        if flags.contains(MlockAllFlags::MCL_CURRENT) {
            let lock_limit = ProcessManager::current_pcb()
                .get_rlimit(RLimitID::Memlock)
                .rlim_cur as usize;

            // 将限制转换为页面数
            let lock_limit_pages = if lock_limit == usize::MAX {
                usize::MAX
            } else {
                lock_limit >> MMArch::PAGE_SHIFT
            };

            // 计算当前已锁定的页面数
            let current_locked = addr_space.read().locked_vm();

            // 计算要锁定的页面数（只计算尚未锁定的可访问 VMA）
            let addr_space_read = addr_space.read();
            let mut pages_to_lock = 0;
            for vma in addr_space_read.mappings.iter_vmas() {
                //for vma in addr_space_read.mappings.vmas.iter() {
                let vma_guard = vma.lock_irqsave();
                let vm_flags = *vma_guard.vm_flags();
                let region = *vma_guard.region();
                drop(vma_guard);

                // 只计算可访问且尚未锁定的 VMA
                if !vma.is_accessible() {
                    continue;
                }

                let already_locked = vm_flags.contains(VmFlags::VM_LOCKED)
                    || vm_flags.contains(VmFlags::VM_LOCKONFAULT);

                if !already_locked {
                    let len = region.end().data() - region.start().data();
                    pages_to_lock += len >> MMArch::PAGE_SHIFT;
                }
            }
            drop(addr_space_read);

            // 检查是否超过限制
            if current_locked + pages_to_lock > lock_limit_pages {
                return Err(SystemError::ENOMEM);
            }
        }

        addr_space.write().mlockall(args[0] as u32)?;

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "flags",
            format!("{:#x}", args[0]),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_MLOCKALL, SysMlockallHandle);
