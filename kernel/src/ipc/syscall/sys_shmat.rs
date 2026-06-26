use crate::alloc::vec::Vec;
use crate::arch::interrupt::TrapFrame;
use crate::syscall::table::FormattedSyscallParam;
use crate::{
    arch::syscall::nr::SYS_SHMAT,
    ipc::shm::{ShmFlags, ShmId},
    mm::{
        syscall::{MapFlags, ProtFlags},
        ucontext::{AddressSpace, FileMappingWithFileArgs},
        MemoryManagementArch, VirtAddr,
    },
    process::ProcessManager,
    syscall::table::Syscall,
};
use syscall_table_macros::declare_syscall;
use system_error::SystemError;
pub struct SysShmatHandle;

/// # SYS_SHMAT系统调用函数，用于连接共享内存段
///
/// ## 参数
///
/// - `id`: 共享内存id
/// - `vaddr`: 连接共享内存的进程虚拟内存区域起始地址
/// - `shmflg`: 共享内存标志
///
/// ## 返回值
///
/// 成功：映射到共享内存的虚拟内存区域起始地址
/// 失败：错误码
pub(super) fn do_kernel_shmat(
    id: ShmId,
    vaddr: VirtAddr,
    shmflg: ShmFlags,
) -> Result<usize, SystemError> {
    let user_supplied_addr = vaddr.data() != 0;
    let mut addr = vaddr;
    let shmlba = crate::arch::MMArch::SHMLBA;

    if user_supplied_addr {
        if !addr.check_aligned(shmlba) {
            if shmflg.contains(ShmFlags::SHM_RND) {
                addr = VirtAddr::new(addr.data() & !(shmlba - 1));
                if addr.data() == 0 && shmflg.contains(ShmFlags::SHM_REMAP) {
                    return Err(SystemError::EINVAL);
                }
            } else {
                return Err(SystemError::EINVAL);
            }
        }
    } else if shmflg.contains(ShmFlags::SHM_REMAP) {
        return Err(SystemError::EINVAL);
    }

    let ipcns = ProcessManager::current_ipcns();
    let current_address_space = AddressSpace::current()?;

    let attach_guard = {
        let mut shm_manager_guard = ipcns.shm.lock();
        shm_manager_guard.attach_begin(
            ipcns.clone(),
            id,
            shmflg.contains(ShmFlags::SHM_RDONLY),
            shmflg.contains(ShmFlags::SHM_EXEC),
        )?
    };
    let size = attach_guard
        .size()
        .checked_add(crate::arch::MMArch::PAGE_SIZE - 1)
        .ok_or(SystemError::EINVAL)?
        & !(crate::arch::MMArch::PAGE_SIZE - 1);
    if user_supplied_addr {
        let end = addr.data().checked_add(size).ok_or(SystemError::EINVAL)?;
        if end > crate::arch::MMArch::USER_END_VADDR.data() {
            return Err(SystemError::EINVAL);
        }
    }

    let readonly = shmflg.contains(ShmFlags::SHM_RDONLY);
    let sysv_attach = attach_guard.create_attach(readonly)?;
    let attach_file = sysv_attach.attach_file();
    let mut prot_flags = ProtFlags::PROT_READ;
    if !readonly {
        prot_flags |= ProtFlags::PROT_WRITE;
    }
    if shmflg.contains(ShmFlags::SHM_EXEC) {
        prot_flags |= ProtFlags::PROT_EXEC;
    }
    let mut map_flags = MapFlags::MAP_SHARED;
    if user_supplied_addr {
        if shmflg.contains(ShmFlags::SHM_REMAP) {
            map_flags |= MapFlags::MAP_FIXED;
        } else {
            // Linux checks the no-remap collision while holding mmap_write_lock.
            // Use DragonOS' no-replace fixed mapping path so the conflict check
            // and VMA insertion are performed atomically under the address-space
            // write lock instead of relying on a syscall-layer pre-check.
            map_flags |= MapFlags::MAP_FIXED_NOREPLACE;
        }
    }

    let mapped = current_address_space
        .file_mapping_with_file_ext(FileMappingWithFileArgs {
            file: attach_file,
            start_vaddr: addr,
            len: size,
            prot_flags,
            map_flags,
            may_exec: true,
            offset: 0,
            round_to_min: !user_supplied_addr,
            allocate_at_once: false,
            sysv_shm: Some(sysv_attach),
            fixed_noreplace_conflict_error_before_mmap_min: if user_supplied_addr
                && !shmflg.contains(ShmFlags::SHM_REMAP)
            {
                Some(SystemError::EINVAL)
            } else {
                None
            },
        })
        .map_err(|err| {
            if err == SystemError::EEXIST && map_flags.contains(MapFlags::MAP_FIXED_NOREPLACE) {
                SystemError::EINVAL
            } else {
                err
            }
        })?;
    attach_guard.finish();
    Ok(mapped.virt_address().data())
}

impl SysShmatHandle {
    #[inline(always)]
    fn id(args: &[usize]) -> ShmId {
        ShmId::new(args[0]) // 更正 ShmIT 为 ShmId
    }

    #[inline(always)]
    fn vaddr(args: &[usize]) -> VirtAddr {
        VirtAddr::new(args[1])
    }
    #[inline(always)]
    fn shmflg(args: &[usize]) -> ShmFlags {
        ShmFlags::from_bits_truncate(args[2] as u32)
    }
}

impl Syscall for SysShmatHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("id", format!("{}", Self::id(args).data())),
            FormattedSyscallParam::new("vaddr", format!("{}", Self::vaddr(args).data())),
            FormattedSyscallParam::new("shmflg", format!("{}", Self::shmflg(args).bits())),
        ]
    }
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let id = Self::id(args);
        let vaddr = Self::vaddr(args);
        let shmflg = Self::shmflg(args);
        do_kernel_shmat(id, vaddr, shmflg)
    }
}

declare_syscall!(SYS_SHMAT, SysShmatHandle);
