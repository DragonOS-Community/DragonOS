use system_error::SystemError;

use super::{page::EntryFlags, PageTableKind, PhysAddr, VirtAddr};
use crate::{
    arch::{
        mm::{LockedFrameAllocator, PageMapper},
        CurrentIrqArch,
    },
    exception::InterruptArch,
    libs::align::page_align_up,
    mm::{allocator::page_frame::PageFrameCount, MMArch, MemoryManagementArch},
    smp::{
        core::smp_get_processor_id,
        cpu::{AtomicProcessorId, ProcessorId},
    },
};
use core::{
    ops::Deref,
    sync::atomic::{compiler_fence, AtomicUsize, Ordering},
};

/// 标志当前没有处理器持有内核映射器的锁
/// 之所以需要这个标志，是因为 AtomicProcessorId::new(0) 会把0当作一个处理器的id
const KERNEL_MAPPER_NO_PROCESSOR: ProcessorId = ProcessorId::INVALID;
/// 当前持有内核映射器锁的处理器
static KERNEL_MAPPER_LOCK_OWNER: AtomicProcessorId =
    AtomicProcessorId::new(KERNEL_MAPPER_NO_PROCESSOR);
/// 内核映射器的锁计数器
static KERNEL_MAPPER_LOCK_COUNT: AtomicUsize = AtomicUsize::new(0);

pub struct KernelMapper {
    /// 内核空间映射器
    mapper: PageMapper,
    /// 标记当前映射器是否为只读
    readonly: bool,
}

impl KernelMapper {
    fn lock_cpu(cpuid: ProcessorId, mapper: PageMapper) -> Self {
        // owner/count 必须在关中断的临界区内一起更新。否则存在如下窗口：
        // 本 CPU 完成 owner CAS 后、尚未 fetch_add 计数时发生中断；中断处理
        // 里再次获取 mapper，看到 owner==cpuid、count==0，于是拿到可写守卫，
        // 释放时又把 owner 清成 INVALID（count 也回到 0）。原调用恢复后把
        // count 加到 1，此时 owner==INVALID、count==1；另一个 CPU 随后可以
        // 成功 CAS owner 并因 prev_count>0 被误判为 readonly。该只读守卫在
        // 释放 MMIO 时会返回 EAGAIN，最终触发 panic。
        //
        // 这里只在真正尝试获取时关中断；若锁被其它 CPU 持有，先恢复中断再
        // 自旋，避免长时间关中断。
        loop {
            let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
            let already_owned = match KERNEL_MAPPER_LOCK_OWNER.compare_exchange_weak(
                KERNEL_MAPPER_NO_PROCESSOR,
                cpuid,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => false,
                // 当前处理器已经持有了锁
                Err(id) if id == cpuid => true,
                // either CAS failed, or some other hardware thread holds the lock
                Err(_) => {
                    drop(irq_guard);
                    core::hint::spin_loop();
                    continue;
                }
            };

            let prev_count = KERNEL_MAPPER_LOCK_COUNT.fetch_add(1, Ordering::Relaxed);
            compiler_fence(Ordering::Acquire);

            // 关中断后 owner 与 count 的不变量必须成立：新获取时 count 为 0，
            // 重入时 count 必大于 0。若此处不成立，说明还有其它破坏该不变量的
            // 路径，应在 debug 构建中尽早暴露，而不是把它变成 readonly 守卫。
            debug_assert_eq!(
                already_owned,
                prev_count > 0,
                "kernel mapper lock invariant violated: owner={:?}, count={}",
                KERNEL_MAPPER_LOCK_OWNER.load(Ordering::Relaxed),
                prev_count
            );

            // 本地核心已经持有过锁，因此标记当前加锁获得的映射器为只读
            let readonly = prev_count > 0;
            drop(irq_guard);

            return Self { mapper, readonly };
        }
    }

    /// @brief 锁定内核映射器, 并返回一个内核映射器对象
    #[inline(always)]
    pub fn lock() -> Self {
        let cpuid = smp_get_processor_id();
        let mapper = unsafe { PageMapper::current(PageTableKind::Kernel, LockedFrameAllocator) };
        return Self::lock_cpu(cpuid, mapper);
    }

    /// @brief 获取内核映射器的page mapper的可变引用。如果当前映射器为只读，则返回 None
    #[inline(always)]
    pub fn as_mut(&mut self) -> Option<&mut PageMapper> {
        if self.readonly {
            return None;
        } else {
            return Some(&mut self.mapper);
        }
    }

    /// @brief 获取内核映射器的page mapper的不可变引用
    #[inline(always)]
    pub fn as_ref(&self) -> &PageMapper {
        return &self.mapper;
    }

    /// 映射一段物理地址到指定的虚拟地址。
    ///
    /// ## 参数
    ///
    /// - `vaddr`: 要映射的虚拟地址
    /// - `paddr`: 要映射的物理地址
    /// - `size`: 要映射的大小（字节，必须是页大小的整数倍，否则会向上取整）
    /// - `flags`: 页面标志
    /// - `flush`: 是否刷新TLB
    ///
    /// ## 返回
    ///
    /// - 成功：返回Ok(())
    /// - 失败： 如果当前映射器为只读，则返回EAGAIN_OR_EWOULDBLOCK
    pub unsafe fn map_phys_with_size(
        &mut self,
        mut vaddr: VirtAddr,
        mut paddr: PhysAddr,
        size: usize,
        flags: EntryFlags<MMArch>,
        flush: bool,
    ) -> Result<(), SystemError> {
        if self.readonly {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        let count = PageFrameCount::new(page_align_up(size) / MMArch::PAGE_SIZE);
        // debug!("kernel mapper: map_phys: vaddr: {vaddr:?}, paddr: {paddr:?}, count: {count:?}, flags: {flags:?}");
        for _ in 0..count.data() {
            let flusher = self.mapper.map_phys(vaddr, paddr, flags).unwrap();

            if flush {
                flusher.flush();
            }

            vaddr += MMArch::PAGE_SIZE;
            paddr += MMArch::PAGE_SIZE;
        }
        compiler_fence(Ordering::SeqCst);
        return Ok(());
    }
}

impl Drop for KernelMapper {
    fn drop(&mut self) {
        // 为了防止fetch_sub和store之间，由于中断，导致store错误清除了owner，导致错误，因此需要关中断。
        let guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        let prev_count = KERNEL_MAPPER_LOCK_COUNT.fetch_sub(1, Ordering::Relaxed);
        if prev_count == 1 {
            KERNEL_MAPPER_LOCK_OWNER.store(KERNEL_MAPPER_NO_PROCESSOR, Ordering::Release);
        }
        drop(guard);
        compiler_fence(Ordering::Release);
    }
}

impl Deref for KernelMapper {
    type Target = PageMapper;

    fn deref(&self) -> &Self::Target {
        return self.as_ref();
    }
}
