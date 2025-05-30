use crate::alloc::vec::Vec;
use crate::syscall::table::FormattedSyscallParam;
use crate::{
    arch::syscall::nr::SYS_SHMAT,
    arch::MMArch,
    ipc::shm::{shm_manager_lock, ShmFlags, ShmId},
    libs::align::page_align_up,
    mm::{
        allocator::page_frame::{PageFrameCount, PhysPageFrame, VirtPageFrame},
        page::{page_manager_lock_irqsave, EntryFlags, PageFlushAll},
        syscall::ProtFlags,
        ucontext::{AddressSpace, VMA},
        VirtAddr, VmFlags,
    },
    syscall::{table::Syscall, user_access::UserBufferReader},
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
    let mut shm_manager_guard = shm_manager_lock();
    let current_address_space = AddressSpace::current()?;
    let mut address_write_guard = current_address_space.write();

    let kernel_shm = shm_manager_guard.get_mut(&id).ok_or(SystemError::EINVAL)?;
    let size = page_align_up(kernel_shm.size());
    let mut phys = PhysPageFrame::new(kernel_shm.start_paddr());
    let count = PageFrameCount::from_bytes(size).unwrap();
    let r = match vaddr.data() {
        // 找到空闲区域并映射到共享内存
        0 => {
            // 找到空闲区域
            let region = address_write_guard
                .mappings
                .find_free(vaddr, size)
                .ok_or(SystemError::EINVAL)?;
            let vm_flags = VmFlags::from(shmflg);
            let destination = VirtPageFrame::new(region.start());
            let page_flags: EntryFlags<MMArch> =
                EntryFlags::from_prot_flags(ProtFlags::from(vm_flags), true);
            let flusher: PageFlushAll<MMArch> = PageFlushAll::new();

            // 将共享内存映射到对应虚拟区域
            let vma = VMA::physmap(
                phys,
                destination,
                count,
                vm_flags,
                page_flags,
                &mut address_write_guard.user_mapper.utable,
                flusher,
            )?;

            // 将VMA加入到当前进程的VMA列表中
            address_write_guard.mappings.insert_vma(vma);

            region.start().data()
        }
        // 指定虚拟地址
        _ => {
            // 获取对应vma
            let vma = address_write_guard
                .mappings
                .contains(vaddr)
                .ok_or(SystemError::EINVAL)?;
            if vma.lock_irqsave().region().start() != vaddr {
                return Err(SystemError::EINVAL);
            }

            // 验证用户虚拟内存区域是否有效
            let _ = UserBufferReader::new(vaddr.data() as *const u8, size, true)?;

            // 必须在取消映射前获取到EntryFlags
            let page_flags = address_write_guard
                .user_mapper
                .utable
                .translate(vaddr)
                .ok_or(SystemError::EINVAL)?
                .1;

            // 取消原映射
            let flusher: PageFlushAll<MMArch> = PageFlushAll::new();
            vma.unmap(&mut address_write_guard.user_mapper.utable, flusher);

            // 将该虚拟内存区域映射到共享内存区域
            let mut page_manager_guard = page_manager_lock_irqsave();
            let mut virt = VirtPageFrame::new(vaddr);
            for _ in 0..count.data() {
                let r = unsafe {
                    address_write_guard.user_mapper.utable.map_phys(
                        virt.virt_address(),
                        phys.phys_address(),
                        page_flags,
                    )
                }
                .expect("Failed to map zero, may be OOM error");
                r.flush();

                // 将vma加入到对应Page的anon_vma
                page_manager_guard
                    .get_unwrap(&phys.phys_address())
                    .write_irqsave()
                    .insert_vma(vma.clone());

                phys = phys.next();
                virt = virt.next();
            }

            // 更新vma的映射状态
            vma.lock_irqsave().set_mapped(true);

            vaddr.data()
        }
    };

    // 更新最后一次连接时间
    kernel_shm.update_atim();

    // 映射计数增加
    kernel_shm.increase_count();

    Ok(r)
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
    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let id = Self::id(args);
        let vaddr = Self::vaddr(args);
        let shmflg = Self::shmflg(args);
        do_kernel_shmat(id, vaddr, shmflg)
    }
}

declare_syscall!(SYS_SHMAT, SysShmatHandle);
