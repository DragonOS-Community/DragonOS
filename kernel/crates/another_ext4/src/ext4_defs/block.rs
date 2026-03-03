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
            data: Box::new([0; BLOCK_SIZE]),
        }
    }
}

impl Block {
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
}
