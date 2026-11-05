//! Integration-owned linear packet bytes. Allocation is prepared before emission.
use super::{OwnedBuffer, OwnedBufferResizeError};
use alloc::{boxed::Box, vec::Vec};
use core::ops::{Deref, DerefMut};

/// Initialized storage returned by a packet allocator. The entire slice is
/// writable capacity; PacketBytes separately tracks the published length.
#[derive(Debug)]
pub enum PacketBytesStorage {
    Heap(Vec<u8>),
    Owned(Box<dyn OwnedBuffer<u8>>),
}
impl PacketBytesStorage {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Heap(v) => v,
            Self::Owned(v) => v.as_ref().as_ref(),
        }
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        match self {
            Self::Heap(v) => v,
            Self::Owned(v) => v.as_mut().as_mut(),
        }
    }
}
/// A fallible allocator, without captured interface or task ownership.
pub type PacketBytesAllocator = fn(usize) -> Result<PacketBytesStorage, OwnedBufferResizeError>;

fn heap_allocate(len: usize) -> Result<PacketBytesStorage, OwnedBufferResizeError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| OwnedBufferResizeError)?;
    bytes.resize(len, 0);
    Ok(PacketBytesStorage::Heap(bytes))
}

/// Unique packet bytes supporting zero-copy ownership transfer. Small packets
/// need no trait-object box; integrations may provide mapped backing for large ones.
#[derive(Debug)]
pub struct PacketBytes {
    storage: PacketBytesStorage,
    len: usize,
    allocator: PacketBytesAllocator,
}
impl Default for PacketBytes {
    fn default() -> Self {
        Self::new()
    }
}
impl PacketBytes {
    pub const fn new() -> Self {
        Self::with_allocator(heap_allocate)
    }
    pub const fn with_allocator(allocator: PacketBytesAllocator) -> Self {
        Self {
            storage: PacketBytesStorage::Heap(Vec::new()),
            len: 0,
            allocator,
        }
    }
    pub fn as_slice(&self) -> &[u8] {
        self
    }
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn capacity(&self) -> usize {
        self.storage.bytes().len()
    }
    pub fn clear(&mut self) {
        self.len = 0;
    }
    /// Prepare capacity without changing bytes or length on failure.
    pub fn try_reserve_exact(&mut self, additional: usize) -> Result<(), OwnedBufferResizeError> {
        let needed = self
            .len
            .checked_add(additional)
            .ok_or(OwnedBufferResizeError)?;
        if needed <= self.capacity() {
            return Ok(());
        }
        let mut replacement = (self.allocator)(needed)?;
        if replacement.bytes().len() < needed
            || replacement.bytes_mut().len() != replacement.bytes().len()
        {
            return Err(OwnedBufferResizeError);
        }
        replacement.bytes_mut()[..self.len].copy_from_slice(self);
        self.storage = replacement;
        Ok(())
    }
    /// Uses previously prepared capacity, never allocates.
    pub fn extend_from_slice(&mut self, bytes: &[u8]) {
        let end = self
            .len
            .checked_add(bytes.len())
            .expect("packet length overflow");
        assert!(end <= self.capacity(), "packet capacity was not prepared");
        self.storage.bytes_mut()[self.len..end].copy_from_slice(bytes);
        self.len = end;
    }
    /// Uses previously prepared capacity, never allocates.
    pub fn resize(&mut self, len: usize, value: u8) {
        assert!(len <= self.capacity(), "packet capacity was not prepared");
        if len > self.len {
            self.storage.bytes_mut()[self.len..len].fill(value);
        }
        self.len = len;
    }
    /// Move the bytes while retaining allocation policy for the next packet.
    pub fn take(&mut self) -> Self {
        let allocator = self.allocator;
        core::mem::replace(self, Self::with_allocator(allocator))
    }
    pub fn into_storage(self) -> (PacketBytesStorage, usize) {
        (self.storage, self.len)
    }
    /// Mapped integration storage is not retained idle after a datagram.
    /// Default heap integrations retain their existing reuse policy.
    pub(crate) fn release_owned(&mut self) {
        if matches!(self.storage, PacketBytesStorage::Owned(_)) {
            self.storage = PacketBytesStorage::Heap(Vec::new());
            self.len = 0;
        }
    }
}
impl Deref for PacketBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.storage.bytes()[..self.len]
    }
}
impl DerefMut for PacketBytes {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.storage.bytes_mut()[..self.len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static DROPS: AtomicUsize = AtomicUsize::new(0);
    #[derive(Debug)]
    struct Owned(Vec<u8>);
    impl AsRef<[u8]> for Owned {
        fn as_ref(&self) -> &[u8] {
            &self.0
        }
    }
    impl AsMut<[u8]> for Owned {
        fn as_mut(&mut self) -> &mut [u8] {
            &mut self.0
        }
    }
    impl Drop for Owned {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn owned(len: usize) -> Result<PacketBytesStorage, OwnedBufferResizeError> {
        CALLS.fetch_add(1, Ordering::Relaxed);
        if len == 99 {
            return Err(OwnedBufferResizeError);
        }
        Ok(PacketBytesStorage::Owned(Box::new(Owned(
            alloc::vec![0; len],
        ))))
    }
    fn short(_: usize) -> Result<PacketBytesStorage, OwnedBufferResizeError> {
        Ok(PacketBytesStorage::Heap(Vec::new()))
    }
    #[test]
    fn failed_growth_and_take_preserve_bytes_owner_and_allocation_policy() {
        CALLS.store(0, Ordering::Relaxed);
        DROPS.store(0, Ordering::Relaxed);
        let mut packet = PacketBytes::with_allocator(owned);
        packet.try_reserve_exact(0).unwrap();
        assert_eq!(CALLS.load(Ordering::Relaxed), 0);
        packet.try_reserve_exact(3).unwrap();
        packet.extend_from_slice(&[4, 5, 6]);
        let pointer = packet.as_ptr();
        assert!(packet.try_reserve_exact(96).is_err());
        assert_eq!(packet.as_ptr(), pointer);
        assert_eq!(&packet[..], &[4, 5, 6]);
        assert_eq!(DROPS.load(Ordering::Relaxed), 0);
        let moved = packet.take();
        assert_eq!(moved.as_ptr(), pointer);
        assert!(packet.is_empty());
        packet.try_reserve_exact(2).unwrap();
        drop(moved);
        assert_eq!(DROPS.load(Ordering::Relaxed), 1);
        packet.release_owned();
        assert_eq!(DROPS.load(Ordering::Relaxed), 2);
        assert_eq!(packet.capacity(), 0);
        packet.try_reserve_exact(1).unwrap();
        assert_eq!(CALLS.load(Ordering::Relaxed), 4);
    }
    #[test]
    fn undersized_allocator_and_overflow_are_rejected_without_publication() {
        let mut packet = PacketBytes::with_allocator(short);
        assert!(packet.try_reserve_exact(8).is_err());
        assert_eq!(packet.capacity(), 0);
        assert!(packet.is_empty());
        let mut packet = PacketBytes::new();
        packet.try_reserve_exact(2).unwrap();
        packet.extend_from_slice(&[1, 2]);
        assert!(packet.try_reserve_exact(usize::MAX).is_err());
        assert_eq!(&packet[..], &[1, 2]);
        packet.release_owned();
        assert_eq!(packet.capacity(), 2);
    }
}
