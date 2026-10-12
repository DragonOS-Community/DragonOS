//! Actual socket storage, independent of FD and protocol-handle lifetimes.
//! All preparation uses explicit birth accounts outside resource locks; the
//! resulting owner only exposes a slice, never an unaccounted resize API.

use alloc::{boxed::Box, sync::Arc};
use core::alloc::Layout;
use system_error::SystemError;

use crate::{
    cgroup::memory::{
        accounting::KernelByteKind,
        object::{ObjectAccount, ObjectCharge},
        owned::OwnedBox,
        socket::{SockAccount, SockCharge},
        MemoryAccount,
    },
    mm::{allocation::MemoryAllocationError, allocator::kernel_allocator::allocation_footprint},
};

/// Socket birth captures task context once; subsequent ingress, interface
/// polls and socket migration only use this explicitly retained provenance.
#[derive(Debug, Clone)]
pub(crate) struct SocketMemory {
    pub sock: Arc<SockAccount>,
    pub creator: Arc<ObjectAccount>,
}

impl SocketMemory {
    pub fn prepare(
        account: &Arc<MemoryAccount>,
        creator: Arc<ObjectAccount>,
    ) -> Result<Self, MemoryAllocationError> {
        let sock = Arc::try_new(SockAccount::prepare(account)?).map_err(|_| SystemError::ENOMEM)?;
        Ok(Self { sock, creator })
    }
}

#[derive(Debug)]
struct SockBufferBody<T> {
    // Declaration order is physical backing first, then its unique receipt.
    storage: Box<[T]>,
    charge: SockCharge,
}

/// The erased trait box only holds this thin identity. The body Box and the
/// potentially large backing allocation have separate actual receipts.
#[derive(Debug)]
pub(crate) struct SockBuffer<T> {
    body: OwnedBox<SockBufferBody<T>>,
}

impl<T: Copy> SockBuffer<T> {
    pub fn try_new(
        sock: &Arc<SockAccount>,
        creator: &Arc<ObjectAccount>,
        capacity: usize,
        initial: T,
    ) -> Result<Self, MemoryAllocationError> {
        let layout = Layout::array::<T>(capacity).map_err(|_| SystemError::EINVAL)?;
        let body =
            OwnedBox::<SockBufferBody<T>>::prepare(creator, KernelByteKind::SlabUnreclaimable)?
                .try_allocate()
                .map_err(|_| SystemError::ENOMEM)?;
        let charge = sock.try_charge(allocation_footprint(layout))?;
        let mut storage =
            Box::<[T]>::try_new_uninit_slice(capacity).map_err(|_| SystemError::ENOMEM)?;
        for slot in storage.iter_mut() {
            slot.write(initial);
        }
        // Every element has just been initialized. T: Copy has no fallible
        // clone or destructor side effect; no resource is published yet.
        let storage = unsafe { storage.assume_init() };
        Ok(Self {
            body: body.init(SockBufferBody { storage, charge }),
        })
    }

    pub fn charge_mut(&mut self) -> &mut SockCharge {
        &mut self.body.charge
    }
}

impl<T> AsRef<[T]> for SockBuffer<T> {
    fn as_ref(&self) -> &[T] {
        &self.body.storage
    }
}

impl<T> AsMut<[T]> for SockBuffer<T> {
    fn as_mut(&mut self) -> &mut [T] {
        &mut self.body.storage
    }
}

pub(crate) fn try_owned_buffer<T: Copy + core::fmt::Debug + Send + Sync + 'static>(
    sock: &Arc<SockAccount>,
    creator: &Arc<ObjectAccount>,
    capacity: usize,
    initial: T,
) -> Result<Box<dyn smoltcp::storage::OwnedBuffer<T>>, MemoryAllocationError> {
    let storage = SockBuffer::try_new(sock, creator, capacity, initial)?;
    Ok(Box::try_new(storage).map_err(|_| SystemError::ENOMEM)?)
}

/// The two independently allocated packet rings retain their own physical
/// receipts, including metadata and payload padding. No payload-length based
/// refund is performed when a datagram leaves the ring.
pub(crate) fn try_owned_packet_buffer<H: Copy + core::fmt::Debug + Send + Sync + 'static>(
    memory: &SocketMemory,
    metadata_capacity: usize,
    payload_capacity: usize,
) -> Result<smoltcp::storage::PacketBuffer<'static, H>, MemoryAllocationError> {
    let metadata = try_owned_buffer(
        &memory.sock,
        &memory.creator,
        metadata_capacity,
        smoltcp::storage::PacketMetadata::EMPTY,
    )?;
    let payload = try_owned_buffer(&memory.sock, &memory.creator, payload_capacity, 0u8)?;
    Ok(smoltcp::storage::PacketBuffer::from_owned(
        metadata, payload,
    ))
}

#[derive(Debug)]
struct SocketSetBufferBody {
    slots: Box<[smoltcp::iface::SocketStorage<'static>]>,
    _backing_charge: ObjectCharge,
}

#[derive(Debug)]
struct SocketSetBuffer {
    body: OwnedBox<SocketSetBufferBody>,
}

impl AsRef<[smoltcp::iface::SocketStorage<'static>]> for SocketSetBuffer {
    fn as_ref(&self) -> &[smoltcp::iface::SocketStorage<'static>] {
        &self.body.slots
    }
}

impl AsMut<[smoltcp::iface::SocketStorage<'static>]> for SocketSetBuffer {
    fn as_mut(&mut self) -> &mut [smoltcp::iface::SocketStorage<'static>] {
        &mut self.body.slots
    }
}

/// Prepare real transport-slot capacity before acquiring a SocketSet guard.
/// Entries contain non-Copy protocol state: initialize EMPTY one by one and
/// later move existing slots at the same index, never clone/zero a socket.
pub(crate) fn try_socket_set_storage(
    creator: &Arc<ObjectAccount>,
    capacity: usize,
) -> Result<smoltcp::iface::SocketSetStorage<'static, 'static>, MemoryAllocationError> {
    type Slot = smoltcp::iface::SocketStorage<'static>;
    let layout = Layout::array::<Slot>(capacity).map_err(|_| SystemError::EINVAL)?;
    let body =
        OwnedBox::<SocketSetBufferBody>::prepare(creator, KernelByteKind::SlabUnreclaimable)?
            .try_allocate()
            .map_err(|_| SystemError::ENOMEM)?;
    let backing_charge =
        creator.try_charge_allocation(KernelByteKind::SlabUnreclaimable, layout)?;
    let mut slots =
        Box::<[Slot]>::try_new_uninit_slice(capacity).map_err(|_| SystemError::ENOMEM)?;
    for slot in slots.iter_mut() {
        slot.write(Slot::EMPTY);
    }
    // EMPTY construction is infallible, no active protocol state exists yet.
    let slots = unsafe { slots.assume_init() };
    let storage = SocketSetBuffer {
        body: body.init(SocketSetBufferBody {
            slots,
            _backing_charge: backing_charge,
        }),
    };
    Ok(smoltcp::iface::SocketSetStorage::from_owned(
        Box::try_new(storage).map_err(|_| SystemError::ENOMEM)?,
    ))
}
