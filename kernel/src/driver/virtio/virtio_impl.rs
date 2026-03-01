use crate::arch::MMArch;
use crate::libs::spinlock::SpinLock;
use crate::mm::dma::{dma_alloc_pages_raw, dma_dealloc_pages_raw, DmaAllocOptions, DmaDirection};
use crate::mm::{MemoryManagementArch, PhysAddr, VirtAddr};
use alloc::collections::BTreeMap;
use core::cmp;
use core::ptr::NonNull;
use virtio_drivers::{BufferDirection, Hal};

/// `share` 在 `virt_2_phys` 失败时使用的 bounce buffer 元信息。
struct SharedBounceBuffer {
    vaddr: NonNull<u8>,
    pages: usize,
}

// SAFETY: `SharedBounceBuffer` 仅保存 DMA 分配返回的地址元数据，
// 所有读写都在 `SHARED_BOUNCE_BUFFERS` 自旋锁保护下进行，不会发生并发可变访问。
unsafe impl Send for SharedBounceBuffer {}

/// 记录通过 bounce buffer 共享的 DMA 映射：
/// key = 共享给设备的物理地址（`paddr`）。
static SHARED_BOUNCE_BUFFERS: SpinLock<BTreeMap<usize, SharedBounceBuffer>> =
    SpinLock::new(BTreeMap::new());

pub struct HalImpl;
unsafe impl Hal for HalImpl {
    /// @brief 申请用于DMA的内存页
    /// @param pages 页数（4k一页）
    /// @return PhysAddr 获得的内存页的初始物理地址
    fn dma_alloc(
        pages: usize,
        _direction: BufferDirection,
    ) -> (virtio_drivers::PhysAddr, NonNull<u8>) {
        let direction = match _direction {
            BufferDirection::DriverToDevice => DmaDirection::ToDevice,
            BufferDirection::DeviceToDriver => DmaDirection::FromDevice,
            _ => DmaDirection::Bidirectional,
        };
        let options = DmaAllocOptions {
            direction,
            use_pool: false,
            ..Default::default()
        };
        dma_alloc_pages_raw(pages, options)
    }
    /// @brief 释放用于DMA的内存页
    /// @param paddr 起始物理地址 pages 页数（4k一页）
    /// @return i32 0表示成功
    unsafe fn dma_dealloc(
        paddr: virtio_drivers::PhysAddr,
        vaddr: NonNull<u8>,
        pages: usize,
    ) -> i32 {
        dma_dealloc_pages_raw(paddr, vaddr, pages)
    }
    /// @brief mmio物理地址转换为虚拟地址，不需要使用
    /// @param paddr 起始物理地址
    /// @return NonNull<u8> 虚拟地址的指针
    unsafe fn mmio_phys_to_virt(paddr: virtio_drivers::PhysAddr, _size: usize) -> NonNull<u8> {
        NonNull::new((MMArch::phys_2_virt(PhysAddr::new(paddr))).unwrap().data() as _).unwrap()
    }
    /// @brief 与真实物理设备共享
    /// @param buffer 要共享的buffer _direction：设备到driver或driver到设备
    /// @return buffer在内存中的物理地址
    unsafe fn share(
        buffer: NonNull<[u8]>,
        dma_direction: BufferDirection,
    ) -> virtio_drivers::PhysAddr {
        let buf_ptr = buffer.as_ptr() as *mut u8;
        let buf_len = buffer.len();
        let vaddr = VirtAddr::new(buf_ptr as usize);

        // 直接映射区地址可直接转物理地址。
        if let Some(paddr) = MMArch::virt_2_phys(vaddr) {
            return paddr.data();
        }

        // 非直接映射地址（例如部分堆/栈对象）走 bounce buffer，避免 `unwrap()` panic。
        let pages = buf_len.div_ceil(MMArch::PAGE_SIZE).max(1);
        let buf_direction = match dma_direction {
            BufferDirection::DriverToDevice => DmaDirection::ToDevice,
            BufferDirection::DeviceToDriver => DmaDirection::FromDevice,
            BufferDirection::Both => DmaDirection::Bidirectional,
        };
        let options = DmaAllocOptions {
            direction: buf_direction,
            use_pool: false,
            ..Default::default()
        };
        let (paddr, bounce_vaddr) = dma_alloc_pages_raw(pages, options);

        // Driver->Device 方向需先把原 buffer 内容拷入 bounce buffer。
        if matches!(
            dma_direction,
            BufferDirection::DriverToDevice | BufferDirection::Both
        ) {
            core::ptr::copy_nonoverlapping(buf_ptr as *const u8, bounce_vaddr.as_ptr(), buf_len);
        }

        SHARED_BOUNCE_BUFFERS.lock_irqsave().insert(
            paddr,
            SharedBounceBuffer {
                vaddr: bounce_vaddr,
                pages,
            },
        );
        paddr
    }
    /// @brief 停止共享
    /// @param _paddr share阶段返回的物理地址
    /// @param _buffer 原始buffer
    /// @param _direction buffer方向
    /// @details
    /// - 直通映射路径：无额外状态，直接返回
    /// - bounce路径：按方向执行回拷，并释放DMA页
    unsafe fn unshare(
        _paddr: virtio_drivers::PhysAddr,
        _buffer: NonNull<[u8]>,
        _direction: BufferDirection,
    ) {
        let Some(bounce) = SHARED_BOUNCE_BUFFERS.lock_irqsave().remove(&_paddr) else {
            // 直接映射路径未使用 bounce buffer，无需处理。
            return;
        };

        if matches!(
            _direction,
            BufferDirection::DeviceToDriver | BufferDirection::Both
        ) {
            let dst = _buffer.as_ptr() as *mut u8;
            let max_len = bounce.pages * MMArch::PAGE_SIZE;
            let copy_len = cmp::min(_buffer.len(), max_len);
            core::ptr::copy_nonoverlapping(bounce.vaddr.as_ptr(), dst, copy_len);
        }

        dma_dealloc_pages_raw(_paddr, bounce.vaddr, bounce.pages);
    }
}
