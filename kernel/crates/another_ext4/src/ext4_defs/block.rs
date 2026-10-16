use crate::constants::*;
use crate::prelude::*;
use core::any::Any;

/// Interface for serializing and deserializing objects to and from bytes.
///
/// # Safety
///
/// This trait is unsafe because it allows arbitrary memory interpretation.
/// Implementors should guarantee the object is saved in the way defined by
/// functions `from_bytes` and `to_bytes`.
pub unsafe trait AsBytes
where
    Self: Sized,
{
    /// Default implementation that deserializes the object from a byte array.
    fn from_bytes(bytes: &[u8]) -> Self {
        unsafe { core::ptr::read(bytes.as_ptr() as *const Self) }
    }
    /// Default implementation that serializes the object to a byte array.
    fn to_bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, size_of::<Self>()) }
    }
}

/// Allocate and initialize the destination directly, without a by-value array
/// temporary on the caller's (potentially small kernel) stack.
pub(crate) fn try_filled_bytes<const N: usize>(value: u8) -> Result<Box<[u8; N]>> {
    let mut image =
        Box::<[u8; N]>::try_new_uninit().map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
    // SAFETY: the allocation owns N writable bytes. Every byte is initialized
    // before the box is exposed, and every bit pattern is valid for u8.
    unsafe {
        image.as_mut_ptr().cast::<u8>().write_bytes(value, N);
        Ok(image.assume_init())
    }
}

/// Copy a complete image into its final allocation, without copying the array
/// by value through the stack.
pub(crate) fn try_copy_bytes<const N: usize>(source: &[u8; N]) -> Result<Box<[u8; N]>> {
    let mut image =
        Box::<[u8; N]>::try_new_uninit().map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
    // SAFETY: source contains N initialized bytes. The new allocation is disjoint
    // from source and is fully initialized by the copy.
    unsafe {
        core::ptr::copy_nonoverlapping(source.as_ptr(), image.as_mut_ptr().cast::<u8>(), N);
        Ok(image.assume_init())
    }
}

/// Common data block descriptor.
#[derive(Debug, Clone)]
pub struct Block {
    /// Physical block id
    pub id: PBlockId,
    /// Raw block data
    pub data: Box<[u8; BLOCK_SIZE]>,
}

impl Default for Block {
    fn default() -> Self {
        Self {
            id: 0,
            data: Self::try_zeroed_data().unwrap_or_else(|_| {
                handle_alloc_error(core::alloc::Layout::new::<[u8; BLOCK_SIZE]>())
            }),
        }
    }
}

impl Block {
    /// Allocate a zeroed filesystem block directly on the heap.
    pub fn try_zeroed_data() -> Result<Box<[u8; BLOCK_SIZE]>> {
        try_filled_bytes(0)
    }

    /// Create new block with given physical block id and data.
    pub fn new(block_id: PBlockId, data: Box<[u8; BLOCK_SIZE]>) -> Self {
        Self { id: block_id, data }
    }

    /// Read `size` bytes from `offset` in block data.
    pub fn read_offset(&self, offset: usize, size: usize) -> &[u8] {
        &self.data[offset..offset + size]
    }

    /// Read bytes from `offset` in block data and interpret it as `T`.
    pub fn read_offset_as<T>(&self, offset: usize) -> T
    where
        T: AsBytes,
    {
        T::from_bytes(&self.data[offset..])
    }

    /// Write block data to `offset` with `size`.
    pub fn write_offset(&mut self, offset: usize, data: &[u8]) {
        self.data[offset..offset + data.len()].copy_from_slice(data);
    }

    /// Transform `T` to bytes and write it to `offset`.
    pub fn write_offset_as<T>(&mut self, offset: usize, value: &T)
    where
        T: AsBytes,
    {
        self.write_offset(offset, value.to_bytes());
    }
}

/// Common interface for block devices.
pub trait BlockDevice: Send + Sync + Any {
    /// Read a block from disk.
    fn read_block(&self, block_id: PBlockId) -> Result<Block>;
    /// Write a block to disk.
    fn write_block(&self, block: &Block) -> Result<()>;
    /// Read a contiguous physical block range into an exact-sized buffer.
    /// Devices may override this to submit fewer, larger requests.
    fn read_blocks(&self, start: PBlockId, data: &mut [u8]) -> Result<()> {
        if data.is_empty() || !data.len().is_multiple_of(BLOCK_SIZE) {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        for (index, chunk) in data.chunks_exact_mut(BLOCK_SIZE).enumerate() {
            let block_id = start
                .checked_add(index as PBlockId)
                .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
            let block = self.read_block(block_id)?;
            chunk.copy_from_slice(&block.data[..]);
        }
        Ok(())
    }
    /// Write an exact-sized buffer to a contiguous physical block range.
    /// The default preserves the existing one-block-at-a-time semantics.
    fn write_blocks(&self, start: PBlockId, data: &[u8]) -> Result<()> {
        if data.is_empty() || !data.len().is_multiple_of(BLOCK_SIZE) {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        for (index, chunk) in data.chunks_exact(BLOCK_SIZE).enumerate() {
            let block_id = start
                .checked_add(index as PBlockId)
                .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
            let mut image = Block::try_zeroed_data()?;
            image.copy_from_slice(chunk);
            self.write_block(&Block::new(block_id, image))?;
        }
        Ok(())
    }
    /// Make all writes completed before this call durable on stable storage.
    ///
    /// A successful return must not be used for durability unless
    /// [`Self::supports_reliable_flush`] is also true.
    fn flush(&self) -> Result<()>;
    /// Whether [`Self::flush`] provides a power-loss durability barrier.
    fn supports_reliable_flush(&self) -> bool;
}

#[cfg(test)]
mod block_device_tests {
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn heap_images_are_fully_initialized_and_independent() {
        let zero = Block::try_zeroed_data().unwrap();
        assert!(zero.iter().all(|byte| *byte == 0));
        let mut source = try_filled_bytes::<1024>(0xa5).unwrap();
        let copy = try_copy_bytes(&source).unwrap();
        source[0] = 7;
        assert!(copy.iter().all(|byte| *byte == 0xa5));
        let filled = try_filled_bytes::<BLOCK_SIZE>(0xff).unwrap();
        assert!(filled.iter().all(|byte| *byte == 0xff));
    }

    #[test]
    fn heap_image_allocation_failure_preserves_source() {
        let source = try_filled_bytes::<BLOCK_SIZE>(0x5a).unwrap();
        let scope = crate::test_allocator::FailureScope::new(0);
        let filled = try_filled_bytes::<BLOCK_SIZE>(0);
        let copied = try_copy_bytes(&source);
        let attempts = scope.attempts();
        drop(scope);
        assert_eq!(attempts, 2);
        assert_eq!(filled.unwrap_err().code(), ErrCode::ENOMEM);
        assert_eq!(copied.unwrap_err().code(), ErrCode::ENOMEM);
        assert!(source.iter().all(|byte| *byte == 0x5a));
    }

    struct FlushDevice {
        reliable: bool,
        error: Option<ErrCode>,
    }

    impl BlockDevice for FlushDevice {
        fn read_block(&self, block_id: PBlockId) -> Result<Block> {
            Ok(Block::new(block_id, Box::new([0; BLOCK_SIZE])))
        }

        fn write_block(&self, _block: &Block) -> Result<()> {
            Ok(())
        }

        fn flush(&self) -> Result<()> {
            self.error.map_or(Ok(()), |code| Err(Ext4Error::new(code)))
        }

        fn supports_reliable_flush(&self) -> bool {
            self.reliable
        }
    }

    #[test]
    fn flush_capability_is_independent_from_method_presence() {
        let device: &dyn BlockDevice = &FlushDevice {
            reliable: false,
            error: None,
        };
        assert!(!device.supports_reliable_flush());
        assert!(device.flush().is_ok());
    }

    #[test]
    fn flush_error_is_preserved_through_trait_dispatch() {
        let device: &dyn BlockDevice = &FlushDevice {
            reliable: true,
            error: Some(ErrCode::EIO),
        };
        assert!(device.supports_reliable_flush());
        assert_eq!(device.flush().unwrap_err().code(), ErrCode::EIO);
    }

    struct CountingDevice {
        reads: AtomicUsize,
        writes: AtomicUsize,
    }

    impl BlockDevice for CountingDevice {
        fn read_block(&self, block_id: PBlockId) -> Result<Block> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(Block::new(block_id, Box::new([block_id as u8; BLOCK_SIZE])))
        }

        fn write_block(&self, _block: &Block) -> Result<()> {
            self.writes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn flush(&self) -> Result<()> {
            Ok(())
        }

        fn supports_reliable_flush(&self) -> bool {
            true
        }
    }

    #[test]
    fn default_bulk_write_returns_allocation_failure_with_completed_prefix() {
        let data = [7; BLOCK_SIZE * 3];
        for completed in 0..3 {
            let device = CountingDevice {
                reads: AtomicUsize::new(0),
                writes: AtomicUsize::new(0),
            };
            let scope = crate::test_allocator::FailureScope::new(completed);
            let result = device.write_blocks(7, &data);
            drop(scope);
            assert_eq!(result.unwrap_err().code(), ErrCode::ENOMEM);
            assert_eq!(device.writes.load(Ordering::Relaxed), completed);
        }
    }

    #[test]
    fn contiguous_defaults_preserve_block_semantics() {
        let device = CountingDevice {
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
        };
        let mut read = [0u8; BLOCK_SIZE * 3];
        device.read_blocks(7, &mut read).unwrap();
        assert_eq!(device.reads.load(Ordering::Relaxed), 3);
        assert!(read[..BLOCK_SIZE].iter().all(|byte| *byte == 7));
        assert!(read[BLOCK_SIZE..BLOCK_SIZE * 2]
            .iter()
            .all(|byte| *byte == 8));
        device.write_blocks(7, &read).unwrap();
        assert_eq!(device.writes.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn contiguous_defaults_reject_invalid_lengths() {
        let device = CountingDevice {
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
        };
        assert_eq!(
            device.write_blocks(0, &[]).unwrap_err().code(),
            ErrCode::EINVAL
        );
        assert_eq!(
            device
                .write_blocks(0, &[0; BLOCK_SIZE - 1])
                .unwrap_err()
                .code(),
            ErrCode::EINVAL
        );
    }
}
