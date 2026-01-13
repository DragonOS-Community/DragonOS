use system_error::SystemError;

use crate::arch::{mm::PageMapper, MMArch};

use super::{
    page::Flusher, syscall::MadvFlags, ucontext::LockedVMA, MemoryManagementArch, VirtAddr, VmFlags,
};

impl LockedVMA {
    pub fn do_madvise(
        &self,
        behavior: MadvFlags,
        mapper: &mut PageMapper,
        mut flusher: impl Flusher<MMArch>,
    ) -> Result<(), SystemError> {
        //TODO https://code.dragonos.org.cn/xref/linux-6.6.21/mm/madvise.c?fi=madvise#do_madvise
        let mut vma = self.lock();
        let mut new_flags = *vma.vm_flags();
        match behavior {
            MadvFlags::MADV_DONTNEED | MadvFlags::MADV_DONTNEED_LOCKED => {
                // MADV_DONTNEED: 释放指定范围内的页面
                // 这是glibc在pthread_create时用来管理线程栈的关键操作
                // 参考: https://code.dragonos.org.cn/xref/linux-6.6.21/mm/madvise.c#madvise_dontneed_single_vma

                let region = *vma.region();
                drop(vma);

                // 遍历VMA覆盖的所有页面，解除映射
                let start_page = region.start();
                let end_page = region.end();
                let mut current_page = start_page;

                while current_page < end_page {
                    let virt_addr = VirtAddr::new(current_page.data());
                    if let Some((_phys_addr, _)) = mapper.translate(virt_addr) {
                        // 只有当页面已经映射时才需要解除映射
                        unsafe {
                            if let Some((_, _, flush)) = mapper.unmap_phys(virt_addr, false) {
                                // 刷新TLB
                                flusher.consume(flush);
                            }
                        }
                    }
                    current_page = VirtAddr::new(current_page.data() + MMArch::PAGE_SIZE);
                }

                return Ok(());
            }

            MadvFlags::MADV_REMOVE => {
                // TODO
            }

            MadvFlags::MADV_WILLNEED => {
                // TODO
            }

            MadvFlags::MADV_COLD => {
                // TODO
            }

            MadvFlags::MADV_PAGEOUT => {
                // TODO
            }

            MadvFlags::MADV_FREE => {
                // TODO
            }

            MadvFlags::MADV_POPULATE_READ | MadvFlags::MADV_POPULATE_WRITE => {
                // TODO
            }

            MadvFlags::MADV_NORMAL => {
                new_flags = new_flags & !VmFlags::VM_RAND_READ & !VmFlags::VM_SEQ_READ
            }

            MadvFlags::MADV_SEQUENTIAL => {
                new_flags = (new_flags & !VmFlags::VM_RAND_READ) | VmFlags::VM_SEQ_READ
            }
            MadvFlags::MADV_RANDOM => {
                new_flags = (new_flags & !VmFlags::VM_SEQ_READ) | VmFlags::VM_RAND_READ
            }

            MadvFlags::MADV_DONTFORK => new_flags |= VmFlags::VM_DONTCOPY,

            MadvFlags::MADV_DOFORK => {
                if vma.vm_flags().contains(VmFlags::VM_IO) {
                    return Err(SystemError::EINVAL);
                }
                new_flags &= !VmFlags::VM_DONTCOPY;
            }

            MadvFlags::MADV_WIPEONFORK => {
                //MADV_WIPEONFORK仅支持匿名映射，后续实现其他映射方式后要在此处添加判断条件
                new_flags |= VmFlags::VM_WIPEONFORK;
            }

            MadvFlags::MADV_KEEPONFORK => new_flags &= !VmFlags::VM_WIPEONFORK,

            MadvFlags::MADV_DONTDUMP => new_flags |= VmFlags::VM_DONTDUMP,

            //MADV_DODUMP不支持巨页映射，后续需要添加判断条件
            MadvFlags::MADV_DODUMP => new_flags &= !VmFlags::VM_DONTDUMP,

            MadvFlags::MADV_MERGEABLE | MadvFlags::MADV_UNMERGEABLE => {}

            MadvFlags::MADV_HUGEPAGE | MadvFlags::MADV_NOHUGEPAGE => {}

            MadvFlags::MADV_COLLAPSE => {}
            _ => {}
        }
        vma.set_vm_flags(new_flags);
        Ok(())
    }
}
