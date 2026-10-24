use crate::arch::MMArch;
use crate::mm::dma::{dma_alloc_pages_raw, dma_dealloc_pages_raw, DmaAllocOptions, DmaDirection};
use crate::mm::{MemoryManagementArch, PhysAddr, VirtAddr};
use core::ptr::NonNull;
use virtio_drivers::{BufferDirection, Hal};

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
        Self::try_share(buffer, dma_direction).expect("virtio buffer sharing failed")
    }

    unsafe fn try_share(
        buffer: NonNull<[u8]>,
        dma_direction: BufferDirection,
    ) -> virtio_drivers::Result<usize> {
        let buf_ptr = buffer.as_ptr() as *mut u8;
        let buf_len = buffer.len();
        if buf_len == 0 {
            return Err(virtio_drivers::Error::InvalidParam);
        }
        // This is the fixed direct map, not a translation of arbitrary vmalloc pages.
        if let Some(paddr) = MMArch::virt_2_phys(VirtAddr::new(buf_ptr as usize)) {
            return Ok(paddr.data());
        }
        let pages = buf_len.div_ceil(MMArch::PAGE_SIZE);
        let direction = match dma_direction {
            BufferDirection::DriverToDevice => DmaDirection::ToDevice,
            BufferDirection::DeviceToDriver => DmaDirection::FromDevice,
            BufferDirection::Both => DmaDirection::Bidirectional,
        };
        let (paddr, bounce_vaddr) = crate::mm::dma::try_dma_alloc_pages_raw(
            pages,
            DmaAllocOptions {
                direction,
                use_pool: false,
                ..Default::default()
            },
        )
        .map_err(|_| virtio_drivers::Error::DmaError)?;
        if matches!(
            dma_direction,
            BufferDirection::DriverToDevice | BufferDirection::Both
        ) {
            core::ptr::copy_nonoverlapping(buf_ptr, bounce_vaddr.as_ptr(), buf_len);
        }
        Ok(paddr)
    }

    unsafe fn unshare(paddr: usize, buffer: NonNull<[u8]>, direction: BufferDirection) {
        Self::release_share(paddr, buffer, direction, true);
    }

    unsafe fn cancel_share(paddr: usize, buffer: NonNull<[u8]>, direction: BufferDirection) {
        Self::release_share(paddr, buffer, direction, false);
    }
}

impl HalImpl {
    /// The original buffer and mapping stay stable throughout the HAL share contract.
    /// Raw DMA allocations are direct-mapped and use symmetric page-count rounding, so
    /// the caller's original buffer identifies bounce ownership without a global registry.
    unsafe fn release_share(
        paddr: usize,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        copy_back: bool,
    ) {
        let original = buffer.as_ptr() as *mut u8;
        if MMArch::virt_2_phys(VirtAddr::new(original as usize)).is_some() {
            return;
        }
        let bounce = MMArch::phys_2_virt(PhysAddr::new(paddr))
            .expect("raw DMA allocation must be direct mapped");
        let bounce = NonNull::new(bounce.as_ptr::<u8>()).expect("raw DMA allocation is non-null");
        if copy_back
            && matches!(
                direction,
                BufferDirection::DeviceToDriver | BufferDirection::Both
            )
        {
            core::ptr::copy_nonoverlapping(bounce.as_ptr(), original, buffer.len());
        }
        dma_dealloc_pages_raw(paddr, bounce, buffer.len().div_ceil(MMArch::PAGE_SIZE));
    }
}

/// Coherent ring allocation for audited virtio-fs/blk queue construction, before
/// publishing the device. Runtime sharing delegates to HalImpl without recovery.
/// Other drivers must audit their construction lock chain before using this HAL.
pub struct VirtioQueueHal;

// SAFETY: successful coherent allocations are exclusive, zeroed and page
// aligned; failure returns no pointer. All other contracts delegate unchanged.
unsafe impl Hal for VirtioQueueHal {
    fn dma_alloc(pages: usize, direction: BufferDirection) -> (usize, NonNull<u8>) {
        HalImpl::dma_alloc(pages, direction)
    }

    fn try_dma_alloc(
        pages: usize,
        direction: BufferDirection,
    ) -> virtio_drivers::Result<(usize, NonNull<u8>)> {
        let direction = match direction {
            BufferDirection::DriverToDevice => DmaDirection::ToDevice,
            BufferDirection::DeviceToDriver => DmaDirection::FromDevice,
            BufferDirection::Both => DmaDirection::Bidirectional,
        };
        crate::mm::dma::try_dma_alloc_pages_raw(
            pages,
            DmaAllocOptions {
                direction,
                use_pool: false,
                compaction: crate::mm::dma::DmaCompaction::DirectAndBackground,
                ..Default::default()
            },
        )
        .map_err(|_| virtio_drivers::Error::DmaError)
    }

    unsafe fn dma_dealloc(paddr: usize, vaddr: NonNull<u8>, pages: usize) -> i32 {
        HalImpl::dma_dealloc(paddr, vaddr, pages)
    }

    unsafe fn mmio_phys_to_virt(paddr: usize, size: usize) -> NonNull<u8> {
        HalImpl::mmio_phys_to_virt(paddr, size)
    }

    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection) -> usize {
        HalImpl::share(buffer, direction)
    }

    unsafe fn try_share(
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
    ) -> virtio_drivers::Result<usize> {
        HalImpl::try_share(buffer, direction)
    }

    unsafe fn cancel_share(paddr: usize, buffer: NonNull<[u8]>, direction: BufferDirection) {
        HalImpl::cancel_share(paddr, buffer, direction)
    }

    unsafe fn unshare(paddr: usize, buffer: NonNull<[u8]>, direction: BufferDirection) {
        HalImpl::unshare(paddr, buffer, direction)
    }
}

/// Direct and vmalloc buffers exercise the real HAL under allocator denial.
#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
pub(crate) fn heap_denied_virtio_share() -> Result<bool, system_error::SystemError> {
    use crate::mm::{kernel_buffer::KernelBuffer, oom::denial};
    let mut direct =
        crate::mm::dma::DmaBuffer::try_alloc_bytes(2 * MMArch::PAGE_SIZE, Default::default())?;
    direct.as_mut_slice().fill(0x5a);
    {
        let _denied = denial::Scope::enter(true);
        let buffer = NonNull::from(direct.as_mut_slice());
        let address = unsafe { HalImpl::try_share(buffer, BufferDirection::Both) }
            .map_err(|_| system_error::SystemError::ENOMEM)?;
        unsafe { HalImpl::unshare(address, buffer, BufferDirection::Both) };
    }
    if direct.as_slice().iter().any(|byte| *byte != 0x5a) {
        return Ok(false);
    }
    let mut storage = KernelBuffer::new_zeroed_recovering(3 * MMArch::PAGE_SIZE)?;
    storage.fill(0xa5);
    let buffer = NonNull::from(&mut storage[..]);
    if unsafe { MMArch::virt_2_phys(VirtAddr::new(buffer.as_ptr() as *mut u8 as usize)) }.is_some()
    {
        return Ok(false);
    }
    let _denied = denial::Scope::enter(true);
    let denied = denial::probe_denied();
    for direction in [
        BufferDirection::DriverToDevice,
        BufferDirection::DeviceToDriver,
        BufferDirection::Both,
    ] {
        let address = unsafe { HalImpl::try_share(buffer, direction) }
            .map_err(|_| system_error::SystemError::ENOMEM)?;
        let bounce = unsafe { MMArch::phys_2_virt(PhysAddr::new(address)) }
            .ok_or(system_error::SystemError::EFAULT)?;
        if !matches!(direction, BufferDirection::DeviceToDriver)
            && unsafe { core::slice::from_raw_parts(bounce.as_ptr::<u8>(), storage.len()) }
                .iter()
                .any(|byte| *byte != 0xa5)
        {
            unsafe { HalImpl::cancel_share(address, buffer, direction) };
            return Ok(false);
        }
        unsafe {
            core::ptr::write_bytes(bounce.as_ptr::<u8>(), 0x3c, storage.len());
        }
        unsafe { HalImpl::cancel_share(address, buffer, direction) };
        if storage.iter().any(|byte| *byte != 0xa5) {
            return Ok(false);
        }
    }
    let address = unsafe { HalImpl::try_share(buffer, BufferDirection::DeviceToDriver) }
        .map_err(|_| system_error::SystemError::ENOMEM)?;
    let bounce = unsafe { MMArch::phys_2_virt(PhysAddr::new(address)) }
        .ok_or(system_error::SystemError::EFAULT)?;
    unsafe {
        core::ptr::write_bytes(bounce.as_ptr::<u8>(), 0x3c, storage.len());
        HalImpl::unshare(address, buffer, BufferDirection::DeviceToDriver);
    }
    Ok(storage.iter().all(|byte| *byte == 0x3c) && denied && denial::probe_denied())
}
