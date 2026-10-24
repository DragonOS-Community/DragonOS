//! Adapt the protocol stack's linear CPU storage to the kernel's unique owner.
use super::PacketBuffer;
use alloc::{boxed::Box, vec::Vec};
use smoltcp::storage::{OwnedBufferResizeError, PacketBytes, PacketBytesStorage};
use system_error::SystemError;

pub(super) fn allocate(len: usize) -> Result<PacketBytesStorage, OwnedBufferResizeError> {
    if len <= crate::libs::paged_vec::CHUNK_BYTES {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(len)
            .map_err(|_| OwnedBufferResizeError)?;
        bytes.resize(len, 0);
        return Ok(PacketBytesStorage::Heap(bytes));
    }
    let mut bytes = PacketBuffer::try_with_capacity(len).map_err(|_| OwnedBufferResizeError)?;
    bytes.resize(len, 0).map_err(|_| OwnedBufferResizeError)?;
    let bytes = Box::try_new(bytes).map_err(|_| OwnedBufferResizeError)?;
    Ok(PacketBytesStorage::Owned(bytes))
}

pub(crate) fn take(bytes: PacketBytes) -> Result<PacketBuffer, SystemError> {
    let (storage, len) = bytes.into_storage();
    let mut buffer = match storage {
        PacketBytesStorage::Heap(mut bytes) => {
            bytes.truncate(len);
            PacketBuffer::from_existing_heap(bytes)
        }
        PacketBytesStorage::Owned(mut owner) => {
            let bytes = owner
                .as_mut()
                .as_any_mut()
                .downcast_mut::<PacketBuffer>()
                .ok_or(SystemError::EINVAL)?;
            core::mem::take(bytes)
        }
    };
    buffer.truncate(len);
    Ok(buffer)
}

pub(super) fn configure(iface: &mut smoltcp::iface::Interface) {
    iface
        .set_packet_buffer_allocator(allocate)
        .expect("a new interface has no pending fragmented datagram");
}

pub(super) fn new_interface(
    config: smoltcp::iface::Config,
    device: &mut (impl smoltcp::phy::Device + ?Sized),
    now: smoltcp::time::Instant,
) -> smoltcp::iface::Interface {
    let mut iface = smoltcp::iface::Interface::new(config, device, now);
    configure(&mut iface);
    iface
}
