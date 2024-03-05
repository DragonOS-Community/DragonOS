use crate::arch::mm::LockedFrameAllocator;
use crate::arch::mm::PageMapper;
use crate::arch::MMArch;
use crate::mm::page::PageFlags;
use crate::mm::{PageTableKind, PhysAddr, VirtAddr};
use crate::smp::core::smp_get_processor_id;
use crate::smp::cpu::AtomicProcessorId;
use crate::smp::cpu::ProcessorId;
use core::sync::atomic::{compiler_fence, AtomicUsize, Ordering};
use system_error::SystemError;
use x86::msr;

/// Check if MTRR is supported
pub fn check_ept_features() -> Result<(), SystemError> {
    const MTRR_ENABLE_BIT: u64 = 1 << 11;
    let ia32_mtrr_def_type = unsafe { msr::rdmsr(msr::IA32_MTRR_DEF_TYPE) };
    if (ia32_mtrr_def_type & MTRR_ENABLE_BIT) == 0 {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    Ok(())
}

// pub fn ept_build_mtrr_map() -> Result<(), SystemError> {
// let ia32_mtrr_cap = unsafe { msr::rdmsr(msr::IA32_MTRRCAP) };
// Ok(())
// }

/// 标志当前没有处理器持有内核映射器的锁
/// 之所以需要这个标志，是因为AtomicUsize::new(0)会把0当作一个处理器的id
const EPT_MAPPER_NO_PROCESSOR: ProcessorId = ProcessorId::INVALID;
/// 当前持有内核映射器锁的处理器
static EPT_MAPPER_LOCK_OWNER: AtomicProcessorId = AtomicProcessorId::new(EPT_MAPPER_NO_PROCESSOR);
/// 内核映射器的锁计数器
static EPT_MAPPER_LOCK_COUNT: AtomicUsize = AtomicUsize::new(0);

pub struct EptMapper {
    /// EPT页表映射器
    mapper: PageMapper,
    /// 标记当前映射器是否为只读
    readonly: bool,
    // EPT页表根地址
    // root_hpa: PhysAddr,
}

impl EptMapper {
    fn lock_cpu(cpuid: ProcessorId, mapper: PageMapper) -> Self {
        loop {
            match EPT_MAPPER_LOCK_OWNER.compare_exchange_weak(
                EPT_MAPPER_NO_PROCESSOR,
                cpuid,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                // 当前处理器已经持有了锁
                Err(id) if id == cpuid => break,
                // either CAS failed, or some other hardware thread holds the lock
                Err(_) => core::hint::spin_loop(),
            }
        }

        let prev_count = EPT_MAPPER_LOCK_COUNT.fetch_add(1, Ordering::Relaxed);
        compiler_fence(Ordering::Acquire);

        // 本地核心已经持有过锁，因此标记当前加锁获得的映射器为只读
        let readonly = prev_count > 0;

        return Self { mapper, readonly };
    }

    /// @brief 锁定内核映射器, 并返回一个内核映射器对象
    #[inline(always)]
    pub fn lock() -> Self {
        let cpuid = smp_get_processor_id();
        let mapper = unsafe { PageMapper::current(PageTableKind::EPT, LockedFrameAllocator) };
        return Self::lock_cpu(cpuid, mapper);
    }

    /// 映射guest physical addr(gpa)到指定的host physical addr(hpa)。
    ///
    /// ## 参数
    ///
    /// - `gpa`: 要映射的guest physical addr
    /// - `hpa`: 要映射的host physical addr
    /// - `flags`: 页面标志
    ///
    /// ## 返回
    ///
    /// - 成功：返回Ok(())
    /// - 失败： 如果当前映射器为只读，则返回EAGAIN_OR_EWOULDBLOCK
    pub unsafe fn walk(
        &mut self,
        gpa: u64,
        hpa: u64,
        flags: PageFlags<MMArch>,
    ) -> Result<(), SystemError> {
        if self.readonly {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        self.mapper
            .map_phys(
                VirtAddr::new(gpa as usize),
                PhysAddr::new(hpa as usize),
                flags,
            )
            .unwrap()
            .flush();
        return Ok(());
    }

    // fn get_ept_index(addr: u64, level: usize) -> u64 {
    //     let pt64_level_shift = PAGE_SHIFT + (level - 1) * PT64_LEVEL_BITS;
    //     (addr >> pt64_level_shift) & ((1 << PT64_LEVEL_BITS) - 1)
    // }
}
