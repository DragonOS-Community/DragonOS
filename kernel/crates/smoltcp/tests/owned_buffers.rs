#![cfg(all(feature = "std", feature = "socket-tcp"))]

use smoltcp::{
    iface::{SocketSet, SocketSetStorage, SocketStorage},
    socket::tcp,
    storage::{PacketBuffer, PacketMetadata, RingBuffer},
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

thread_local! {
    static WATCH: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    static FREES: Cell<usize> = const { Cell::new(0) };
}
struct Allocator;
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if WATCH.try_with(Cell::get).unwrap_or(false) {
            let _ = ALLOCATIONS.try_with(|v| v.set(v.get() + 1));
        }
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if WATCH.try_with(Cell::get).unwrap_or(false) {
            let _ = FREES.try_with(|v| v.set(v.get() + 1));
        }
        System.dealloc(ptr, layout)
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

#[derive(Debug)]
struct Storage<T> {
    bytes: Vec<T>,
    drops: Arc<AtomicUsize>,
}
impl<T> AsRef<[T]> for Storage<T> {
    fn as_ref(&self) -> &[T] {
        &self.bytes
    }
}
impl<T> AsMut<[T]> for Storage<T> {
    fn as_mut(&mut self) -> &mut [T] {
        &mut self.bytes
    }
}
impl<T> Drop for Storage<T> {
    fn drop(&mut self) {
        drop(std::mem::take(&mut self.bytes));
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
fn ring(drops: &Arc<AtomicUsize>, capacity: usize) -> RingBuffer<'static, u8> {
    RingBuffer::from_owned(Box::new(Storage {
        bytes: vec![0; capacity],
        drops: drops.clone(),
    }))
}

#[test]
fn single_axis_swap_does_not_retire_or_allocate_other_axis() {
    let original = Arc::new(AtomicUsize::new(0));
    let replacement = Arc::new(AtomicUsize::new(0));
    let mut socket = tcp::Socket::new(ring(&original, 64), ring(&original, 64));
    let tx = ring(&replacement, 128);
    ALLOCATIONS.with(|n| n.set(0));
    FREES.with(|n| n.set(0));
    WATCH.with(|n| n.set(true));
    let old_tx = socket.replace_prepared_send_buffer(tx).unwrap();
    WATCH.with(|n| n.set(false));
    assert_eq!(ALLOCATIONS.with(Cell::get), 0);
    assert_eq!(FREES.with(Cell::get), 0);
    assert_eq!(socket.recv_capacity(), 64);
    assert_eq!(socket.send_capacity(), 128);
    assert_eq!(original.load(Ordering::SeqCst), 0);
    drop(old_tx);
    assert_eq!(original.load(Ordering::SeqCst), 1);
    let rx = ring(&replacement, 128);
    ALLOCATIONS.with(|n| n.set(0));
    FREES.with(|n| n.set(0));
    WATCH.with(|n| n.set(true));
    let old_rx = socket.replace_prepared_receive_buffer(rx).unwrap();
    WATCH.with(|n| n.set(false));
    assert_eq!(ALLOCATIONS.with(Cell::get), 0);
    assert_eq!(FREES.with(Cell::get), 0);
    drop(old_rx);
    assert_eq!(original.load(Ordering::SeqCst), 2);
    drop(socket);
    assert_eq!(replacement.load(Ordering::SeqCst), 2);
}

#[test]
fn prepared_swap_allocates_and_frees_nothing_and_returns_real_owners() {
    let old_drops = Arc::new(AtomicUsize::new(0));
    let new_drops = Arc::new(AtomicUsize::new(0));
    let mut socket = tcp::Socket::new(ring(&old_drops, 64), ring(&old_drops, 64));
    let rx = ring(&new_drops, 128);
    let tx = ring(&new_drops, 128);
    ALLOCATIONS.with(|n| n.set(0));
    FREES.with(|n| n.set(0));
    WATCH.with(|n| n.set(true));
    let retired = socket.replace_prepared_buffers(rx, tx).unwrap();
    WATCH.with(|n| n.set(false));
    assert_eq!(ALLOCATIONS.with(Cell::get), 0);
    assert_eq!(FREES.with(Cell::get), 0);
    assert_eq!(old_drops.load(Ordering::SeqCst), 0);
    drop(retired);
    assert_eq!(old_drops.load(Ordering::SeqCst), 2);
    assert_eq!(new_drops.load(Ordering::SeqCst), 0);
    drop(socket);
    assert_eq!(new_drops.load(Ordering::SeqCst), 2);
}

#[test]
fn owned_resize_is_explicitly_rejected_without_backing_changes() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut buffer = ring(&drops, 64);
    assert_eq!(buffer.enqueue_slice(b"preserved"), 9);
    assert!(buffer.resize(128, 0).is_err());
    assert_eq!(buffer.capacity(), 64);
    let mut bytes = [0; 9];
    assert_eq!(buffer.read_allocated(0, &mut bytes), 9);
    assert_eq!(&bytes, b"preserved");
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(buffer);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn owned_metadata_downcast_does_not_change_backing_identity() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut buffer = ring(&drops, 64);
    let pointer = buffer.owned_storage_mut().unwrap().as_ref().as_ptr();
    let owner = buffer
        .owned_storage_mut()
        .unwrap()
        .as_any_mut()
        .downcast_mut::<Storage<u8>>()
        .unwrap();
    owner.bytes[3] = 4;
    assert_eq!(owner.bytes.as_ptr(), pointer);
    assert_eq!(buffer.capacity(), 64);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let mut borrowed = [0u8; 4];
    assert!(RingBuffer::new(&mut borrowed[..])
        .owned_storage_mut()
        .is_none());
    assert!(RingBuffer::new(vec![0u8; 4]).owned_storage_mut().is_none());
}

#[test]
fn packet_metadata_and_payload_owners_move_with_the_actual_rings() {
    let drops = Arc::new(AtomicUsize::new(0));
    let metadata = Box::new(Storage {
        bytes: vec![PacketMetadata::EMPTY; 4],
        drops: drops.clone(),
    });
    let payload = Box::new(Storage {
        bytes: vec![0u8; 32],
        drops: drops.clone(),
    });
    let mut packet = PacketBuffer::<u32>::from_owned(metadata, payload);
    packet.enqueue(4, 7).unwrap().copy_from_slice(b"data");
    let mut moved = Some(packet);
    let (header, bytes) = moved.as_mut().unwrap().dequeue().unwrap();
    assert_eq!(header, 7);
    assert_eq!(bytes, b"data");
    // Queue consumption does not free either physical allocation.
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(moved);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}

#[test]
fn invalid_prepared_candidate_is_returned_without_free_or_allocation() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut socket = tcp::Socket::new(ring(&drops, 64), ring(&drops, 64));
    let mut rx = ring(&drops, 128);
    assert_eq!(rx.enqueue_slice(b"not empty"), 9);
    let tx = ring(&drops, 128);
    ALLOCATIONS.with(|n| n.set(0));
    FREES.with(|n| n.set(0));
    WATCH.with(|n| n.set(true));
    let candidates = socket.replace_prepared_buffers(rx, tx).unwrap_err();
    WATCH.with(|n| n.set(false));
    assert_eq!(ALLOCATIONS.with(Cell::get), 0);
    assert_eq!(FREES.with(Cell::get), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!((socket.recv_capacity(), socket.send_capacity()), (64, 64));
    assert_eq!(candidates.0.len(), 9);
    drop(candidates);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    drop(socket);
    assert_eq!(drops.load(Ordering::SeqCst), 4);
}

fn prepared_slots(drops: &Arc<AtomicUsize>, count: usize) -> SocketSetStorage<'static, 'static> {
    let mut bytes = Vec::with_capacity(count);
    bytes.resize_with(count, || SocketStorage::EMPTY);
    SocketSetStorage::from_owned(Box::new(Storage {
        bytes,
        drops: drops.clone(),
    }))
}

#[test]
fn borrowed_slot_array_outlives_set_with_owned_socket_buffers() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut slots: [SocketStorage<'static>; 2] = Default::default();
    {
        let mut sockets: SocketSet<'_, 'static> = SocketSet::new(&mut slots[..]);
        let handle = sockets.add(tcp::Socket::new(ring(&drops, 64), ring(&drops, 64)));
        assert_eq!(sockets.get::<tcp::Socket>(handle).recv_capacity(), 64);
    }
    // Dropping the borrowed container must not destroy sockets in its caller's
    // array. Reborrowing that array must still expose their original handles.
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    {
        let sockets = SocketSet::new(&mut slots[..]);
        assert_eq!(sockets.iter().count(), 1);
    }
    drop(slots);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}

#[test]
fn borrowed_slots_can_move_into_prepared_owned_storage_without_retirement() {
    let buffers = Arc::new(AtomicUsize::new(0));
    let backing = Arc::new(AtomicUsize::new(0));
    let mut slots: [SocketStorage<'static>; 2] = Default::default();
    {
        let mut sockets: SocketSet<'_, 'static> = SocketSet::new(&mut slots[..]);
        let handle = sockets.add(tcp::Socket::new(ring(&buffers, 64), ring(&buffers, 64)));
        let retired = sockets
            .replace_prepared_storage(prepared_slots(&backing, 4))
            .unwrap();
        assert_eq!(sockets.storage_slots(), 4);
        assert_eq!(sockets.get::<tcp::Socket>(handle).send_capacity(), 64);
        assert_eq!(buffers.load(Ordering::SeqCst), 0);
        assert_eq!(backing.load(Ordering::SeqCst), 0);
        drop(retired);
        assert_eq!(buffers.load(Ordering::SeqCst), 0);
    }
    assert_eq!(buffers.load(Ordering::SeqCst), 2);
    assert_eq!(backing.load(Ordering::SeqCst), 1);
    let sockets = SocketSet::new(&mut slots[..]);
    assert_eq!(sockets.iter().count(), 0);
}

fn packet_storage(
    drops: &Arc<AtomicUsize>,
    metadata: usize,
    payload: usize,
) -> PacketBuffer<'static, u8> {
    PacketBuffer::from_owned(
        Box::new(Storage {
            bytes: vec![PacketMetadata::EMPTY; metadata],
            drops: drops.clone(),
        }),
        Box::new(Storage {
            bytes: vec![0; payload],
            drops: drops.clone(),
        }),
    )
}

fn expect_packet(packets: &mut PacketBuffer<'static, u8>, expected: (u8, &[u8])) {
    let (header, payload) = packets.dequeue().unwrap();
    assert_eq!((header, &*payload), expected);
}

fn wrapped_packets(drops: &Arc<AtomicUsize>) -> PacketBuffer<'static, u8> {
    let mut packets = packet_storage(drops, 4, 10);
    packets.enqueue(4, 1).unwrap().copy_from_slice(b"aaaa");
    packets.enqueue(4, 2).unwrap().copy_from_slice(b"bbbb");
    expect_packet(&mut packets, (1, b"aaaa"));
    packets.enqueue(4, 3).unwrap().copy_from_slice(b"cccc"); // Real padding at payload end.
    expect_packet(&mut packets, (2, b"bbbb"));
    packets.enqueue(2, 4).unwrap().copy_from_slice(b"dd"); // Metadata and payload wrap.
    packets.enqueue(0, 5).unwrap();
    packets
}

#[test]
fn packet_prepared_swap_preserves_padding_wrap_and_empty_candidate_cursor_without_alloc_free() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut packets = wrapped_packets(&drops);
    let mut candidate = packet_storage(&drops, 8, 20);
    candidate.enqueue(1, 99).unwrap()[0] = 99;
    candidate.dequeue().unwrap(); // Both empty rings retain a nonzero read origin.
    ALLOCATIONS.with(|n| n.set(0));
    FREES.with(|n| n.set(0));
    WATCH.with(|n| n.set(true));
    let accepted = packets.swap_prepared(&mut candidate);
    WATCH.with(|n| n.set(false));
    assert!(accepted);
    assert_eq!(ALLOCATIONS.with(Cell::get), 0);
    assert_eq!(FREES.with(Cell::get), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    expect_packet(&mut packets, (3, b"cccc"));
    expect_packet(&mut packets, (4, b"dd"));
    expect_packet(&mut packets, (5, b""));
    assert!(packets.is_empty());
    drop(candidate);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    drop(packets);
    assert_eq!(drops.load(Ordering::SeqCst), 4);
}

#[test]
fn packet_prepared_swap_rejects_either_insufficient_ring_before_any_mutation() {
    for (metadata, payload) in [(3, 20), (8, 7)] {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut packets = wrapped_packets(&drops);
        let mut candidate = packet_storage(&drops, metadata, payload);
        ALLOCATIONS.with(|n| n.set(0));
        FREES.with(|n| n.set(0));
        WATCH.with(|n| n.set(true));
        let accepted = packets.swap_prepared(&mut candidate);
        WATCH.with(|n| n.set(false));
        assert!(!accepted);
        assert_eq!(ALLOCATIONS.with(Cell::get), 0);
        assert_eq!(FREES.with(Cell::get), 0);
        assert!(candidate.is_empty());
        assert_eq!(candidate.payload_capacity(), payload);
        expect_packet(&mut packets, (3, b"cccc"));
        expect_packet(&mut packets, (4, b"dd"));
        expect_packet(&mut packets, (5, b""));
        drop(candidate);
        drop(packets);
        assert_eq!(drops.load(Ordering::SeqCst), 4);
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let mut empty = packet_storage(&drops, 0, 0);
    let mut candidate = packet_storage(&drops, 0, 0);
    assert!(empty.swap_prepared(&mut candidate));
}

#[test]
#[cfg(all(feature = "socket-udp", feature = "proto-ipv4"))]
fn udp_logical_shrink_keeps_queued_datagrams_and_rejects_oversized_growth() {
    use smoltcp::socket::udp;
    use smoltcp::wire::{IpAddress, IpEndpoint};
    let mut socket = udp::Socket::new(
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 32]),
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 32]),
    );
    socket.bind(1234).unwrap();
    let peer = IpEndpoint::new(IpAddress::v4(192, 0, 2, 1), 4321);
    socket.send_slice(b"abcdefgh", peer).unwrap();
    assert!(socket.set_send_buffer_limit(4));
    assert_eq!(socket.send_queue(), 8);
    assert!(!socket.can_send());
    assert_eq!(socket.send(1, peer), Err(udp::SendError::BufferFull));
    assert!(!socket.set_send_buffer_limit(33));
    assert_eq!(socket.send(1, peer), Err(udp::SendError::BufferFull));
    assert!(socket.set_send_buffer_limit(12));
    assert_eq!(socket.send(5, peer), Err(udp::SendError::BufferFull));
    socket.send_slice(b"ijkl", peer).unwrap();
    assert_eq!(socket.send_queue(), 12);
    assert!(!socket.can_send());
    // Existing complete datagrams and the local binding are not reset by a
    // prepared physical replacement, including a zero-length packet.
    socket.send(0, peer).unwrap();
    let mut candidate = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0; 64]);
    ALLOCATIONS.with(|n| n.set(0));
    FREES.with(|n| n.set(0));
    WATCH.with(|n| n.set(true));
    let swapped = socket.swap_prepared_send_buffer(&mut candidate);
    WATCH.with(|n| n.set(false));
    assert!(swapped);
    assert_eq!(ALLOCATIONS.with(Cell::get), 0);
    assert_eq!(FREES.with(Cell::get), 0);
    assert_eq!(socket.endpoint().port, 1234);
    assert_eq!(socket.send_queue(), 12);
    assert_eq!(candidate.dequeue().unwrap().1, b"abcdefgh");
    assert_eq!(candidate.dequeue().unwrap().1, b"ijkl");
    assert_eq!(candidate.dequeue().unwrap().1, b"");
    assert!(candidate.is_empty());
}

#[test]
fn prepared_socket_set_storage_preserves_handles_without_allocation_or_free() {
    let storage_drops = Arc::new(AtomicUsize::new(0));
    let buffer_drops = Arc::new(AtomicUsize::new(0));
    let mut set = SocketSet::new(vec![SocketStorage::EMPTY]);
    let handle = set
        .try_add_no_grow(tcp::Socket::new(
            ring(&buffer_drops, 16),
            ring(&buffer_drops, 16),
        ))
        .unwrap();
    let candidate = prepared_slots(&storage_drops, 4);
    ALLOCATIONS.with(|n| n.set(0));
    FREES.with(|n| n.set(0));
    WATCH.with(|n| n.set(true));
    let retired = set.replace_prepared_storage(candidate).unwrap();
    WATCH.with(|n| n.set(false));
    assert_eq!(ALLOCATIONS.with(Cell::get), 0);
    assert_eq!(FREES.with(Cell::get), 0);
    assert_eq!(set.get::<tcp::Socket>(handle).send_capacity(), 16);
    assert_eq!(set.available_slots(), 3);
    assert_eq!(buffer_drops.load(Ordering::SeqCst), 0);
    drop(retired);
    drop(set);
    assert_eq!(storage_drops.load(Ordering::SeqCst), 1);
    assert_eq!(buffer_drops.load(Ordering::SeqCst), 2);
}

#[test]
fn prepared_socket_slots_remain_reserved_across_storage_replacement_without_alloc_free() {
    let storage_drops = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let mut set = SocketSet::new(vec![SocketStorage::EMPTY, SocketStorage::EMPTY]);
    let reserved = set.reserve_slot_no_grow().unwrap();
    let occupied = set
        .try_add_no_grow(tcp::Socket::new(ring(&drops, 8), ring(&drops, 8)))
        .unwrap();
    assert_ne!(reserved, occupied);
    assert_eq!(set.available_slots(), 0);
    assert_eq!(set.iter().count(), 1);
    let candidate = prepared_slots(&storage_drops, 4);
    let protocol = tcp::Socket::new(ring(&drops, 16), ring(&drops, 16));
    ALLOCATIONS.with(|n| n.set(0));
    FREES.with(|n| n.set(0));
    WATCH.with(|n| n.set(true));
    let retired = set.replace_prepared_storage(candidate).unwrap();
    let committed = set.add_reserved(reserved, protocol);
    let cancel = set.reserve_slot_no_grow().unwrap();
    set.cancel_reserved(cancel);
    WATCH.with(|n| n.set(false));
    assert_eq!(ALLOCATIONS.with(Cell::get), 0);
    assert_eq!(FREES.with(Cell::get), 0);
    assert_eq!(committed, reserved);
    assert_eq!(set.get::<tcp::Socket>(occupied).send_capacity(), 8);
    assert_eq!(set.get::<tcp::Socket>(reserved).send_capacity(), 16);
    assert_eq!(set.available_slots(), 2);
    assert_eq!(set.iter().count(), 2);
    drop(retired);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(set);
    assert_eq!(drops.load(Ordering::SeqCst), 4);
    assert_eq!(storage_drops.load(Ordering::SeqCst), 1);
}

#[test]
fn occupied_return_slot_survives_remove_and_array_move_without_extra_capacity() {
    let drops = Arc::new(AtomicUsize::new(0));
    let storage_drops = Arc::new(AtomicUsize::new(0));
    let mut source = SocketSet::new(vec![SocketStorage::EMPTY]);
    let original = source
        .try_add_no_grow(tcp::Socket::new(ring(&drops, 16), ring(&drops, 16)))
        .unwrap();
    let mut target = SocketSet::new(vec![SocketStorage::EMPTY]);
    let target_slot = target.reserve_slot_no_grow().unwrap();
    let replacement = prepared_slots(&storage_drops, 2);
    ALLOCATIONS.with(|n| n.set(0));
    FREES.with(|n| n.set(0));
    WATCH.with(|n| n.set(true));
    // Rejected before removal: dedicated occupied unreserve preserves the
    // socket and does not invoke the empty-vacancy cancellation contract.
    source.reserve_existing(original);
    source.unreserve_existing(original);
    assert_eq!(source.get::<tcp::Socket>(original).send_capacity(), 16);
    source.reserve_existing(original);
    let moved = source.remove(original);
    assert_eq!(source.available_slots(), 0);
    target.add_reserved_socket(target_slot, moved);
    let retired = source.replace_prepared_storage(replacement).unwrap();
    assert_eq!(source.available_slots(), 1);
    source.add_reserved_socket(original, target.remove(target_slot));
    WATCH.with(|n| n.set(false));
    assert_eq!(ALLOCATIONS.with(Cell::get), 0);
    assert_eq!(FREES.with(Cell::get), 0);
    assert_eq!(source.get::<tcp::Socket>(original).send_capacity(), 16);
    assert_eq!(target.iter().count(), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(retired);
    drop(target);
    drop(source);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert_eq!(storage_drops.load(Ordering::SeqCst), 1);
}

#[test]
fn full_set_add_and_too_small_replacement_return_the_real_candidates() {
    let storage_drops = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let mut set = SocketSet::new(vec![SocketStorage::EMPTY, SocketStorage::EMPTY]);
    let a = set
        .try_add_no_grow(tcp::Socket::new(ring(&drops, 16), ring(&drops, 16)))
        .unwrap();
    let b = set
        .try_add_no_grow(tcp::Socket::new(ring(&drops, 32), ring(&drops, 32)))
        .unwrap();
    let socket = tcp::Socket::new(ring(&drops, 64), ring(&drops, 64));
    let replacement = prepared_slots(&storage_drops, 1);
    ALLOCATIONS.with(|n| n.set(0));
    FREES.with(|n| n.set(0));
    WATCH.with(|n| n.set(true));
    let rejected_socket = set.try_add_no_grow(socket).unwrap_err();
    let rejected_storage = set.replace_prepared_storage(replacement).unwrap_err();
    WATCH.with(|n| n.set(false));
    assert_eq!(ALLOCATIONS.with(Cell::get), 0);
    assert_eq!(FREES.with(Cell::get), 0);
    assert_eq!(set.get::<tcp::Socket>(a).send_capacity(), 16);
    assert_eq!(set.get::<tcp::Socket>(b).send_capacity(), 32);
    assert_eq!(rejected_socket.send_capacity(), 64);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(rejected_storage);
    drop(rejected_socket);
    drop(set);
    assert_eq!(storage_drops.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 6);
}
