use crate::arch::mm::PageMapper;
use system_error::SystemError;

use super::{mmu_gather::MmuGather, syscall::MadvFlags, ucontext::LockedVMA, VmFlags};

impl LockedVMA {
    pub fn madvise_updated_flags(
        &self,
        behavior: MadvFlags,
    ) -> Result<Option<VmFlags>, SystemError> {
        let vma = self.lock();
        let mut new_flags = *vma.vm_flags();
        match behavior {
            MadvFlags::MADV_DONTNEED | MadvFlags::MADV_DONTNEED_LOCKED => {
                debug_assert!(
                    false,
                    "MADV_DONTNEED is a range operation, not a VMA flag update"
                );
                return Ok(None);
            }

            MadvFlags::MADV_REMOVE
            | MadvFlags::MADV_WILLNEED
            | MadvFlags::MADV_COLD
            | MadvFlags::MADV_PAGEOUT
            | MadvFlags::MADV_FREE
            | MadvFlags::MADV_POPULATE_READ
            | MadvFlags::MADV_POPULATE_WRITE => {}

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
                    debug_assert!(
                        false,
                        "MADV_DOFORK on VM_IO must be rejected before VMA split"
                    );
                    return Err(SystemError::EINVAL);
                }
                new_flags &= !VmFlags::VM_DONTCOPY;
            }

            MadvFlags::MADV_WIPEONFORK => {
                //MADV_WIPEONFORK仅支持匿名映射，后续实现其他映射方式后要在此处添加判断条件
                if vma.vm_file().is_some() || vma.vm_flags().contains(VmFlags::VM_SHARED) {
                    return Err(SystemError::EINVAL);
                }
                new_flags |= VmFlags::VM_WIPEONFORK;
            }

            MadvFlags::MADV_KEEPONFORK => new_flags &= !VmFlags::VM_WIPEONFORK,

            MadvFlags::MADV_DONTDUMP => new_flags |= VmFlags::VM_DONTDUMP,

            //MADV_DODUMP不支持巨页映射，后续需要添加判断条件
            MadvFlags::MADV_DODUMP => {
                let special_flags = VmFlags::VM_IO | VmFlags::VM_PFNMAP | VmFlags::VM_DONTEXPAND;
                if !vma.vm_flags().contains(VmFlags::VM_HUGETLB)
                    && vma.vm_flags().intersects(special_flags)
                {
                    return Err(SystemError::EINVAL);
                }
                new_flags &= !VmFlags::VM_DONTDUMP;
            }

            MadvFlags::MADV_MERGEABLE | MadvFlags::MADV_UNMERGEABLE => {}

            MadvFlags::MADV_HUGEPAGE | MadvFlags::MADV_NOHUGEPAGE => {}

            MadvFlags::MADV_COLLAPSE => {}
            _ => {}
        }
        Ok(Some(new_flags))
    }

    pub fn do_madvise(
        &self,
        behavior: MadvFlags,
        _mapper: &mut PageMapper,
        _tlb: &mut MmuGather<'_>,
    ) {
        //TODO https://code.dragonos.org.cn/xref/linux-6.6.21/mm/madvise.c?fi=madvise#do_madvise
        let Ok(Some(new_flags)) = self.madvise_updated_flags(behavior) else {
            return;
        };
        self.lock().set_vm_flags(new_flags);
    }
}
