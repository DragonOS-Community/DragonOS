//! Unique CPU bytes whose last owner may be released in any context.
//! Large allocations are prepared in task context without reclaim; their
//! mappings retire only in the existing memory notification worker.

#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
use crate::{arch::MMArch, mm::MemoryManagementArch};
use alloc::vec::Vec;
use core::{
    fmt,
    ops::{Deref, DerefMut},
};
use system_error::SystemError;

#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
use {
    super::kernel_buffer::KernelBuffer,
    crate::libs::{
        prepared_queue::{Node, Queue},
        spinlock::SpinLock,
    },
    alloc::boxed::Box,
};

#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
struct MappedOwner {
    buffer: Option<KernelBuffer>,
    #[cfg(feature = "mm_selftest")]
    retired: Option<alloc::sync::Arc<core::sync::atomic::AtomicBool>>,
}

#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
impl Drop for MappedOwner {
    fn drop(&mut self) {
        drop(self.buffer.take());
        #[cfg(feature = "mm_selftest")]
        if let Some(retired) = &self.retired {
            retired.store(true, core::sync::atomic::Ordering::Release);
        }
    }
}

// SAFETY: byte storage has a single owner and no task-local references. Creation
// remains task-only; ownership transfer never destroys the mapping. Only the
// resource-lock-free memory worker removes and drops the prepared node.
#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
unsafe impl Send for MappedOwner {}

#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
static RETIRED: SpinLock<Queue<MappedOwner>> = SpinLock::new(Queue::new());

/// A virtual-contiguous byte owner; never a DMA allocation. Architectures that
/// do not yet support KernelBuffer retain their original fallible heap route.
pub struct CpuBuffer {
    backend: Backend,
    len: usize,
}

enum Backend {
    Heap(Vec<u8>),
    #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
    Mapped(Option<Box<Node<MappedOwner>>>),
}

impl CpuBuffer {
    pub(crate) const fn new() -> Self {
        Self {
            backend: Backend::Heap(Vec::new()),
            len: 0,
        }
    }

    pub(crate) fn try_with_capacity(capacity: usize) -> Result<Self, SystemError> {
        if capacity > isize::MAX as usize {
            return Err(SystemError::ENOMEM);
        }
        #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
        if capacity > MMArch::PAGE_SIZE {
            if !super::memory_notify::is_ready() || !super::vmalloc::task_context() {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            // Prepare the retirement node before the mapping. Failure has no
            // published owner and may drop both here in eligible task context.
            let mut node = Node::try_new(MappedOwner {
                buffer: None,
                #[cfg(feature = "mm_selftest")]
                retired: None,
            })?;
            let buffer =
                KernelBuffer::try_new_zeroed_virtual(capacity).map_err(|error| match error {
                    super::allocation::MemoryAllocationError::System(error) => error,
                    super::allocation::MemoryAllocationError::Charge(_) => SystemError::ENOMEM,
                })?;
            node.value_mut().buffer = Some(buffer);
            return Ok(Self {
                backend: Backend::Mapped(Some(node)),
                len: 0,
            });
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| SystemError::ENOMEM)?;
        // Initialize the entire owned capacity once; only len bytes are exposed.
        bytes.resize(capacity, 0);
        Ok(Self {
            backend: Backend::Heap(bytes),
            len: 0,
        })
    }

    pub(crate) fn try_from_slice(bytes: &[u8]) -> Result<Self, SystemError> {
        let mut buffer = Self::try_with_capacity(bytes.len())?;
        buffer.extend_from_slice(bytes)?;
        Ok(buffer)
    }

    /// Transfer an already allocated legacy heap owner without a second
    /// allocation after emission. This does not remove the original producer's
    /// physical-contiguity requirement. Ordinary births and explicit growth
    /// continue to select the mapped backend above PAGE, and large imported
    /// owners must not enter a scratch cache.
    pub(crate) fn from_existing_heap(mut bytes: Vec<u8>) -> Self {
        let len = bytes.len();
        // Initialize only spare storage already owned by this Vec. resize
        // cannot allocate because its target is exactly the existing capacity.
        bytes.resize(bytes.capacity(), 0);
        Self {
            backend: Backend::Heap(bytes),
            len,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub(crate) fn capacity(&self) -> usize {
        match &self.backend {
            Backend::Heap(bytes) => bytes.len(),
            #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
            Backend::Mapped(node) => node
                .as_ref()
                .unwrap()
                .value()
                .buffer
                .as_ref()
                .unwrap()
                .len(),
        }
    }
    pub(crate) fn clear(&mut self) {
        self.len = 0;
    }
    pub(crate) fn truncate(&mut self, len: usize) {
        self.len = self.len.min(len);
    }
    pub(crate) fn resize(&mut self, len: usize, value: u8) -> Result<(), SystemError> {
        if len > self.capacity() {
            return Err(SystemError::ENOMEM);
        }
        if len > self.len {
            let old_len = self.len;
            self.storage_mut()[old_len..len].fill(value);
        }
        self.len = len;
        Ok(())
    }
    pub(crate) fn try_reserve_exact(&mut self, additional: usize) -> Result<(), SystemError> {
        let capacity = self
            .len
            .checked_add(additional)
            .ok_or(SystemError::ENOMEM)?;
        if capacity <= self.capacity() {
            return Ok(());
        }
        let mut replacement = Self::try_with_capacity(capacity)?;
        replacement.extend_from_slice(self.as_slice())?;
        *self = replacement;
        Ok(())
    }
    pub(crate) fn extend_from_slice(&mut self, bytes: &[u8]) -> Result<(), SystemError> {
        let end = self
            .len
            .checked_add(bytes.len())
            .ok_or(SystemError::ENOMEM)?;
        if end > self.capacity() {
            return Err(SystemError::ENOMEM);
        }
        let start = self.len;
        self.storage_mut()[start..end].copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }
    fn storage(&self) -> &[u8] {
        match &self.backend {
            Backend::Heap(bytes) => bytes,
            #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
            Backend::Mapped(node) => node.as_ref().unwrap().value().buffer.as_ref().unwrap(),
        }
    }
    fn storage_mut(&mut self) -> &mut [u8] {
        match &mut self.backend {
            Backend::Heap(bytes) => bytes,
            #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
            Backend::Mapped(node) => node.as_mut().unwrap().value_mut().buffer.as_mut().unwrap(),
        }
    }
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.storage()[..self.len]
    }
    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        let len = self.len;
        &mut self.storage_mut()[..len]
    }
}

impl Default for CpuBuffer {
    fn default() -> Self {
        Self::new()
    }
}
impl Deref for CpuBuffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}
impl DerefMut for CpuBuffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.as_mut_slice()
    }
}
impl fmt::Debug for CpuBuffer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CpuBuffer")
            .field("len", &self.len)
            .field("capacity", &self.capacity())
            .finish()
    }
}
impl Drop for CpuBuffer {
    fn drop(&mut self) {
        #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
        if let Backend::Mapped(node) = &mut self.backend {
            if let Some(node) = node.take() {
                RETIRED.lock_irqsave().push_node(node);
                super::memory_notify::request_deferred_drop();
            }
        }
    }
}

/// Called only by the resource-lock-free memory worker. Return true to repost
/// WORK_PENDING even when no further producer arrives after this finite batch.
pub(crate) fn drain_retired() -> bool {
    #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
    {
        for _ in 0..32 {
            let node = RETIRED.lock_irqsave().pop_node();
            let Some(node) = node else {
                return false;
            };
            drop(node);
            crate::sched::cond_resched();
        }
        !RETIRED.lock_irqsave().is_empty()
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "riscv64")))]
    false
}

/// Real backing and real-worker retirement; no private drain or production
/// policy reset. The per-node observer exists only in this test feature.
#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
pub(crate) fn selftest() -> bool {
    use crate::cgroup::memory::{accounting::KernelByteKind, object::ObjectAccount, MemoryAccount};
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    static NEXT_ACCOUNT: AtomicU64 = AtomicU64::new(0x0262_0000_0000_0000);
    fn run() -> Result<bool, SystemError> {
        let mut legacy = Vec::new();
        legacy
            .try_reserve_exact(17)
            .map_err(|_| SystemError::ENOMEM)?;
        legacy.extend_from_slice(b"old");
        let pointer = legacy.as_ptr();
        let capacity = legacy.capacity();
        let mut imported = CpuBuffer::from_existing_heap(legacy);
        if imported.as_slice() != b"old"
            || imported.as_ptr() != pointer
            || imported.capacity() != capacity
        {
            return Ok(false);
        }
        imported.resize(capacity, 0xa5)?;
        if imported[..3] != *b"old" || imported[3..].iter().any(|byte| *byte != 0xa5) {
            return Ok(false);
        }
        let mut small = CpuBuffer::try_from_slice(b"bytes")?;
        let original = small.as_slice().to_vec();
        let capacity = small.capacity();
        if small.extend_from_slice(&[0]).is_ok()
            || small.resize(capacity + 1, 0).is_ok()
            || small.as_slice() != original
            || small.capacity() != capacity
        {
            return Ok(false);
        }
        if small.try_reserve_exact(usize::MAX).is_ok() || small.as_slice() != original {
            return Ok(false);
        }
        small.try_reserve_exact(MMArch::PAGE_SIZE)?;
        if small.as_slice() != original {
            return Ok(false);
        }
        small.resize(MMArch::PAGE_SIZE + 1, 0xa5)?;
        if small[original.len()..].iter().any(|byte| *byte != 0xa5) {
            return Ok(false);
        }
        small.truncate(2);
        small.clear();
        if !small.is_empty() {
            return Ok(false);
        }
        drop(small);

        let account = MemoryAccount::new(NEXT_ACCOUNT.fetch_add(1, Ordering::Relaxed), None)?;
        let owner = ObjectAccount::new(&account)?;
        let mut buffers = Vec::new();
        let mut observers = Vec::new();
        buffers
            .try_reserve_exact(65)
            .map_err(|_| SystemError::ENOMEM)?;
        observers
            .try_reserve_exact(65)
            .map_err(|_| SystemError::ENOMEM)?;
        for _ in 0..65 {
            let retired = Arc::try_new(AtomicBool::new(false)).map_err(|_| SystemError::ENOMEM)?;
            let mut node = Node::try_new(MappedOwner {
                buffer: None,
                retired: Some(retired.clone()),
            })?;
            node.value_mut().buffer = Some(
                KernelBuffer::try_new_zeroed_virtual_in(MMArch::PAGE_SIZE + 1, &owner)
                    .map_err(|_| SystemError::ENOMEM)?,
            );
            let mut buffer = CpuBuffer {
                backend: Backend::Mapped(Some(node)),
                len: 0,
            };
            buffer.resize(MMArch::PAGE_SIZE + 1, 0x5a)?;
            if buffer.iter().any(|byte| *byte != 0x5a) {
                return Ok(false);
            }
            buffers.push(buffer);
            observers.push(retired);
        }
        if account.current_pages() == 0 {
            return Ok(false);
        }
        let irq_lock = SpinLock::new(());
        {
            let _irq = irq_lock.lock_irqsave();
            // Existing retirement guards reject synchronous mapping Drop here.
            // Worker can run on another CPU, so no assertion on queue depth.
            while let Some(buffer) = buffers.pop() {
                drop(buffer);
            }
        }
        let until = crate::time::timer::next_n_us_timer_jiffies(5_000_000);
        while observers.iter().any(|done| !done.load(Ordering::Acquire)) {
            if crate::time::timer::clock() >= until {
                return Ok(false);
            }
            crate::sched::sched_yield();
        }
        let retired = Arc::try_new(AtomicBool::new(false)).map_err(|_| SystemError::ENOMEM)?;
        let mut node = Node::try_new(MappedOwner {
            buffer: None,
            retired: Some(retired.clone()),
        })?;
        let fragmented = crate::mm::allocator::buddy::with_fragmented_buffer_arena(|data_frame| {
            // The real arena has enough isolated order-0 pages, and proves an
            // order-1 allocation cannot succeed before invoking this callback.
            // Above 2*PAGE the existing KernelBuffer test entrance takes its
            // real virtual route, without changing production selection.
            let len = 2 * MMArch::PAGE_SIZE + 31;
            let mut frames = 0;
            let Ok(storage) = KernelBuffer::try_new_zeroed_in_with(len, &owner, &mut || {
                let frame = data_frame()?;
                frames += 1;
                Ok(frame)
            }) else {
                return false;
            };
            if frames != len.div_ceil(MMArch::PAGE_SIZE) {
                // A platform without supported mappings must not make this
                // fragmented-backing test pass via the global heap instead.
                drop(storage);
                return false;
            }
            node.value_mut().buffer = Some(storage);
            let mut buffer = CpuBuffer {
                backend: Backend::Mapped(Some(node)),
                len: 0,
            };
            buffer
                .resize(len, 0)
                .expect("fixed backing was prepared above");
            for (index, byte) in buffer.iter_mut().enumerate() {
                *byte = index.wrapping_mul(37) as u8;
            }
            let contents = buffer
                .iter()
                .enumerate()
                .all(|(index, byte)| *byte == index.wrapping_mul(37) as u8);
            {
                let _irq = irq_lock.lock_irqsave();
                drop(buffer);
            }
            let until = crate::time::timer::next_n_us_timer_jiffies(5_000_000);
            let mut timely = true;
            while !retired.load(Ordering::Acquire) {
                timely &= crate::time::timer::clock() < until;
                // Cleanup must wait for the real mapping owner: returning on
                // timeout would end the arena before its donated pages retire.
                // A broken worker is caught by the enclosing guest watchdog;
                // neither the fixture nor a private drain impersonates it.
                crate::sched::sched_yield();
            }
            contents && timely
        });
        let snapshot = account.snapshot();
        // Shared vmalloc leaf-table charges may legitimately survive this data
        // owner until an unrelated mapping in the same leaf is retired.
        Ok(fragmented
            && snapshot
                .kernel_bytes
                .iter()
                .enumerate()
                .all(|(kind, bytes)| kind == KernelByteKind::PageTables as usize || *bytes == 0))
    }
    run().unwrap_or(false)
}
