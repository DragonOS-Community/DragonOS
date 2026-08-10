use core::{intrinsics::unlikely, mem::ManuallyDrop};

use alloc::sync::{Arc, Weak};
use log::error;
use system_error::SystemError;

use crate::{
    libs::{align::AlignedBox, spinlock::SpinLock},
    mm::{PhysAddr, VirtAddr},
    process::ProcessControlBlock,
};

#[derive(Debug)]
pub struct KernelStack {
    stack: Option<AlignedBox<[u8; KernelStack::SIZE], { KernelStack::ALIGN }>>,
    /// Indicates whether this kernel stack can be freed.
    ty: KernelStackType,
}

#[derive(Debug)]
pub enum KernelStackType {
    KernelSpace(VirtAddr, PhysAddr),
    Static,
    Dynamic,
}

// Why is this lock needed?
// When alloc_from_kernel_space allocates a kernel stack, if the function is
// interrupted and the switched task calls dealloc_from_kernel_space to free a
// kernel stack, acquiring a mutable reference to KernelMapper would fail,
// causing an error.
static KSTACK_LOCK: SpinLock<()> = SpinLock::new(());

unsafe fn alloc_from_kernel_space() -> (VirtAddr, PhysAddr) {
    use crate::arch::MMArch;
    use crate::mm::allocator::page_frame::{allocate_page_frames, PageFrameCount};
    use crate::mm::kernel_mapper::KernelMapper;
    use crate::mm::page::EntryFlags;
    use crate::mm::MemoryManagementArch;

    // Layout
    // ---------------
    // | KernelStack |
    // | guard page  | size == KernelStack::SIZE
    // | KernelStack |
    // | guard page  |
    // | ..........  |
    // ---------------

    let _guard = KSTACK_LOCK.lock_irqsave();
    let need_size = KernelStack::SIZE * 2;
    let page_num = PageFrameCount::new(need_size.div_ceil(MMArch::PAGE_SIZE).next_power_of_two());

    let (paddr, _count) = allocate_page_frames(page_num).expect("kernel stack alloc failed");

    let guard_vaddr = MMArch::phys_2_virt(paddr).unwrap();
    let _kstack_paddr = paddr + KernelStack::SIZE;
    let kstack_vaddr = guard_vaddr + KernelStack::SIZE;

    core::ptr::write_bytes(kstack_vaddr.data() as *mut u8, 0, KernelStack::SIZE);

    let guard_flags = EntryFlags::new();

    let mut kernel_mapper = KernelMapper::lock();
    let kernel_mapper = kernel_mapper.as_mut().unwrap();

    for i in 0..KernelStack::SIZE / MMArch::PAGE_SIZE {
        let guard_page_vaddr = guard_vaddr + i * MMArch::PAGE_SIZE;
        // Map the guard page
        let flusher = kernel_mapper.remap(guard_page_vaddr, guard_flags).unwrap();
        flusher.flush();
    }

    // unsafe {
    //     log::debug!(
    //         "trigger kernel stack guard page :{:#x}",
    //         (kstack_vaddr.data() - 8)
    //     );
    //     let guard_ptr = (kstack_vaddr.data() - 8) as *mut usize;
    //     guard_ptr.write(0xfff); // Invalid
    // }

    // log::info!(
    //     "[kernel stack alloc]: virt: {:#x}, phy: {:#x}",
    //     kstack_vaddr.data(),
    //     _kstack_paddr.data()
    // );
    (guard_vaddr, paddr)
}

unsafe fn dealloc_from_kernel_space(vaddr: VirtAddr, paddr: PhysAddr) {
    use crate::arch::mm::kernel_page_flags;
    use crate::arch::MMArch;
    use crate::mm::allocator::page_frame::{deallocate_page_frames, PageFrameCount, PhysPageFrame};
    use crate::mm::kernel_mapper::KernelMapper;
    use crate::mm::MemoryManagementArch;

    let _guard = KSTACK_LOCK.lock_irqsave();

    let need_size = KernelStack::SIZE * 2;
    let page_num = PageFrameCount::new(need_size.div_ceil(MMArch::PAGE_SIZE).next_power_of_two());

    // log::info!(
    //     "[kernel stack dealloc]: virt: {:#x}, phy: {:#x}",
    //     vaddr.data(),
    //     paddr.data()
    // );

    let mut kernel_mapper = KernelMapper::lock();
    let kernel_mapper = kernel_mapper.as_mut().unwrap();

    // restore the guard page flags
    for i in 0..KernelStack::SIZE / MMArch::PAGE_SIZE {
        let guard_page_vaddr = vaddr + i * MMArch::PAGE_SIZE;
        let flusher = kernel_mapper
            .remap(guard_page_vaddr, kernel_page_flags(vaddr))
            .unwrap();
        flusher.flush();
    }

    // release the physical page
    unsafe { deallocate_page_frames(PhysPageFrame::new(paddr), page_num) };
}

impl KernelStack {
    pub const SIZE: usize = 0x8000;
    pub const ALIGN: usize = 0x8000;

    pub fn new() -> Result<Self, SystemError> {
        if cfg!(feature = "kstack_protect") {
            unsafe {
                let (kstack_vaddr, kstack_paddr) = alloc_from_kernel_space();
                let real_kstack_vaddr = kstack_vaddr + KernelStack::SIZE;
                Ok(Self {
                    stack: Some(
                        AlignedBox::<[u8; KernelStack::SIZE], { KernelStack::ALIGN }>::new_unchecked(
                            real_kstack_vaddr.data() as *mut [u8; KernelStack::SIZE],
                        ),
                    ),
                    ty: KernelStackType::KernelSpace(kstack_vaddr, kstack_paddr),
                })
            }
        } else {
            Ok(Self {
                stack: Some(
                    AlignedBox::<[u8; KernelStack::SIZE], { KernelStack::ALIGN }>::new_zeroed()?,
                ),
                ty: KernelStackType::Dynamic,
            })
        }
    }

    /// Construct a kernel stack struct from an existing memory region.
    ///
    /// Only used during BSP startup to construct the kernel stack for the idle
    /// process. Using this function at any other time is very likely to cause
    /// errors!
    pub unsafe fn from_existed(base: VirtAddr) -> Result<Self, SystemError> {
        if base.is_null() || !base.check_aligned(Self::ALIGN) {
            return Err(SystemError::EFAULT);
        }

        Ok(Self {
            stack: Some(
                AlignedBox::<[u8; KernelStack::SIZE], { KernelStack::ALIGN }>::new_unchecked(
                    base.data() as *mut [u8; KernelStack::SIZE],
                ),
            ),
            ty: KernelStackType::Static,
        })
    }

    pub fn guard_page_address(&self) -> Option<VirtAddr> {
        match self.ty {
            KernelStackType::KernelSpace(kstack_virt_addr, _) => {
                return Some(kstack_virt_addr);
            }
            _ => {
                // Static and dynamic kernel stacks do not have a guard page.
                return None;
            }
        }
    }

    pub fn guard_page_size(&self) -> Option<usize> {
        match self.ty {
            KernelStackType::KernelSpace(_, _) => {
                return Some(KernelStack::SIZE);
            }
            _ => {
                // Static and dynamic kernel stacks do not have a guard page.
                return None;
            }
        }
    }

    /// Returns the start virtual address (low address) of the kernel stack.
    pub fn start_address(&self) -> VirtAddr {
        return VirtAddr::new(self.stack.as_ref().unwrap().as_ptr() as usize);
    }

    /// Returns the end virtual address (high address, exclusive) of the kernel stack.
    pub fn stack_max_address(&self) -> VirtAddr {
        return VirtAddr::new(self.stack.as_ref().unwrap().as_ptr() as usize + Self::SIZE);
    }

    pub unsafe fn set_pcb(&mut self, pcb: Weak<ProcessControlBlock>) -> Result<(), SystemError> {
        // Place a Weak<ProcessControlBlock> pointer at the lowest address of the
        // kernel stack.
        let p: *const ProcessControlBlock = Weak::into_raw(pcb);
        let stack_bottom_ptr = self.start_address().data() as *mut *const ProcessControlBlock;

        // If the lowest address of the kernel stack already has a PCB pointer,
        // do not overwrite it and return an error.
        if unlikely(unsafe { !(*stack_bottom_ptr).is_null() }) {
            error!("kernel stack bottom is not null: {:p}", *stack_bottom_ptr);
            return Err(SystemError::EPERM);
        }
        // Store the PCB pointer at the lowest address of the kernel stack.
        unsafe {
            *stack_bottom_ptr = p;
        }

        return Ok(());
    }

    /// Clear the PCB pointer on the kernel stack.
    ///
    /// ## Parameters
    ///
    /// - `force`: If true, the PCB pointer will be forcibly cleared even if it
    ///   is not null, without properly handling the Weak pointer.
    pub unsafe fn clear_pcb(&mut self, force: bool) {
        let stack_bottom_ptr = self.start_address().data() as *mut *const ProcessControlBlock;
        if unlikely(unsafe { (*stack_bottom_ptr).is_null() }) {
            return;
        }

        if !force {
            let pcb_ptr: Weak<ProcessControlBlock> = Weak::from_raw(*stack_bottom_ptr);
            drop(pcb_ptr);
        }

        *stack_bottom_ptr = core::ptr::null();
    }

    /// Returns an `Arc` pointer to the PCB stored on the current kernel stack.
    #[allow(dead_code)]
    pub unsafe fn pcb(&self) -> Option<Arc<ProcessControlBlock>> {
        // Retrieve the PCB pointer from the lowest address of the kernel stack.
        let p = self.stack.as_ref().unwrap().as_ptr() as *const *const ProcessControlBlock;
        if unlikely(unsafe { (*p).is_null() }) {
            return None;
        }

        // Wrap the pointer in ManuallyDrop to prevent Arc::drop from being called,
        // protecting the kernel stack PCB pointer from premature release.
        let weak_wrapper: ManuallyDrop<Weak<ProcessControlBlock>> =
            ManuallyDrop::new(Weak::from_raw(*p));

        let new_arc: Arc<ProcessControlBlock> = weak_wrapper.upgrade()?;
        return Some(new_arc);
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        if let Some(stack) = &self.stack {
            let ptr = stack.as_ptr() as *const *const ProcessControlBlock;
            if unsafe { !(*ptr).is_null() } {
                let pcb_ptr: Weak<ProcessControlBlock> = unsafe { Weak::from_raw(*ptr) };
                drop(pcb_ptr);
            }
        }
        match self.ty {
            KernelStackType::KernelSpace(kstack_virt_addr, kstack_phy_addr) => {
                // Free the kernel stack.
                unsafe {
                    dealloc_from_kernel_space(kstack_virt_addr, kstack_phy_addr);
                }
                let bx = self.stack.take();
                core::mem::forget(bx);
            }
            KernelStackType::Static => {
                let bx = self.stack.take();
                core::mem::forget(bx);
            }
            KernelStackType::Dynamic => {}
        }
    }
}
