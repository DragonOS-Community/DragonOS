use super::*;
use crate::{
    hal::fake::FakeHal,
    transport::{
        fake::{FakeTransport, QueueStatus, State},
        DeviceStatus, DeviceType,
    },
    volatile::ReadOnly,
    Error,
};
use alloc::{sync::Arc, vec, vec::Vec};
use core::ptr::NonNull;
use std::{sync::Mutex, thread};

const SIZE: usize = 4;
type Raw = VirtIONetRaw<FakeHal, FakeTransport<Config>, SIZE>;

fn config() -> Config {
    Config {
        mac: ReadOnly::new([2, 0, 0, 0, 0, 1]),
        status: ReadOnly::new(Status::LINK_UP),
        max_virtqueue_pairs: ReadOnly::new(1),
        mtu: ReadOnly::new(1500),
    }
}

fn transport(config: &mut Config, legacy: bool) -> (FakeTransport<Config>, Arc<Mutex<State>>) {
    let state = Arc::new(Mutex::new(State {
        legacy,
        queues: vec![QueueStatus::default(), QueueStatus::default()],
        ..Default::default()
    }));
    (
        FakeTransport {
            device_type: DeviceType::Network,
            max_queue_size: SIZE as u32,
            device_features: SUPPORTED_FEATURES.bits() & if legacy { !(1 << 32) } else { u64::MAX },
            config_space: NonNull::from(config),
            state: state.clone(),
        },
        state,
    )
}

// Used-ring layout is defined by the split-ring ABI, not by the driver type.
fn overwrite_completion(
    state: &Arc<Mutex<State>>,
    queue: usize,
    id: Option<u32>,
    len: Option<u32>,
) {
    let state = state.lock().unwrap();
    let used = state.queues[queue].device_area as *mut u8;
    unsafe {
        if let Some(id) = id {
            used.add(4).cast::<u32>().write_volatile(id);
        }
        if let Some(len) = len {
            used.add(8).cast::<u32>().write_volatile(len);
        }
    }
}

#[test]
fn wire_headers_match_negotiated_interface_on_every_raw_path() {
    for legacy in [true, false] {
        let mut config = config();
        let (transport, state) = transport(&mut config, legacy);
        let mut net = Raw::new(transport).unwrap();
        let len = if legacy { 10 } else { 12 };
        assert_eq!(
            state
                .lock()
                .unwrap()
                .status
                .contains(DeviceStatus::FEATURES_OK),
            !legacy
        );
        let mut tx = vec![0xff; len + 14];
        assert_eq!(net.fill_buffer_header(&mut tx).unwrap(), len);
        assert_eq!(&tx[..len], vec![0; len]);
        let token = unsafe { net.transmit_begin(&tx) }.unwrap();
        assert_eq!(state.lock().unwrap().read_from_queue::<SIZE>(1), tx);
        unsafe { net.transmit_complete(token, &tx) }.unwrap();
        for num_buffers in [0u8, 1] {
            let mut rx = vec![0; MIN_BUFFER_LEN];
            let token = unsafe { net.receive_begin(&mut rx) }.unwrap();
            let mut frame = vec![0; len];
            if !legacy {
                frame[10] = num_buffers;
            }
            frame.extend_from_slice(&[0xaa; 14]);
            state.lock().unwrap().write_to_queue::<SIZE>(0, &frame);
            assert_eq!(
                unsafe { net.receive_complete(token, &mut rx) }.unwrap(),
                (len, 14)
            );
            assert_eq!(&rx[len..len + 14], &[0xaa; 14]);
        }
    }
}

#[test]
fn modern_negotiation_failure_never_publishes_queues() {
    for reject in [false, true] {
        let mut config = config();
        let (mut transport, state) = transport(&mut config, false);
        if reject {
            state.lock().unwrap().reject_features = true;
        } else {
            transport.device_features &= !(1 << 32);
        }
        assert!(matches!(Raw::new(transport), Err(Error::Unsupported)));
        assert_eq!(state.lock().unwrap().queue_sets, 0);
    }
}

#[test]
fn receive_rejects_short_oversize_and_unnegotiated_offload_requests() {
    for bad in 0..4 {
        let mut config = config();
        let (mut transport, state) = transport(&mut config, false);
        transport.device_features &= !Features::GUEST_CSUM.bits();
        let mut net = Raw::new(transport).unwrap();
        let mut rx = vec![0; MIN_BUFFER_LEN];
        let token = unsafe { net.receive_begin(&mut rx) }.unwrap();
        let mut frame = vec![0; 26];
        if bad == 2 {
            frame[0] = Flags::NEEDS_CSUM.bits();
        }
        if bad == 3 {
            frame[1] = GsoType::TCPV4.0;
        }
        state.lock().unwrap().write_to_queue::<SIZE>(0, &frame);
        if bad < 2 {
            overwrite_completion(
                &state,
                0,
                None,
                Some(if bad == 0 {
                    11
                } else {
                    MIN_BUFFER_LEN as u32 + 1
                }),
            );
        }
        assert!(matches!(
            unsafe { net.receive_complete(token, &mut rx) },
            Err(Error::IoError)
        ));
        // The descriptor was reclaimed before validating device metadata.
        let token = unsafe { net.receive_begin(&mut rx) }.unwrap();
        state.lock().unwrap().write_to_queue::<SIZE>(0, &[0; 26]);
        unsafe { net.receive_complete(token, &mut rx) }.unwrap();
    }
}

#[test]
fn ignored_metadata_does_not_reject_a_complete_software_checksummed_frame() {
    let mut bytes = [0xff; 26];
    bytes[0] = Flags::DATA_VALID.bits();
    bytes[1] = 0;
    assert_eq!(
        HeaderLayout::Modern.receive(&mut bytes, 26, false).unwrap(),
        (12, 14)
    );
}

#[test]
fn blocking_scatter_gather_and_receive_use_the_same_layout() {
    for legacy in [true, false] {
        let mut config = config();
        let (transport, state) = transport(&mut config, legacy);
        let mut net = Raw::new(transport).unwrap();
        let device = state.clone();
        let worker = thread::spawn(move || {
            for payload in [vec![0x55; 14], Vec::new()] {
                State::wait_until_queue_notified(&device, 1);
                let data = device.lock().unwrap().read_from_queue::<SIZE>(1);
                let len = if legacy { 10 } else { 12 };
                assert_eq!(&data[..len], vec![0; len]);
                assert_eq!(&data[len..], payload);
            }
            State::wait_until_queue_notified(&device, 0);
            device
                .lock()
                .unwrap()
                .write_to_queue::<SIZE>(0, &vec![0; if legacy { 24 } else { 26 }]);
        });
        net.send(&[0x55; 14]).unwrap();
        net.send(&[]).unwrap();
        let mut rx = vec![0; MIN_BUFFER_LEN];
        assert_eq!(
            net.receive_wait(&mut rx).unwrap(),
            (if legacy { 10 } else { 12 }, 14)
        );
        worker.join().unwrap();
    }
}

#[cfg(feature = "alloc")]
#[test]
fn buffered_payload_boundaries_and_fail_stop() {
    for legacy in [true, false] {
        let mut config = config();
        let (transport, state) = transport(&mut config, legacy);
        let mut net = VirtIONet::<FakeHal, _, SIZE>::new(transport, MIN_BUFFER_LEN).unwrap();
        let mut frame = vec![0; if legacy { 10 } else { 12 }];
        frame.extend_from_slice(&[0x44; 14]);
        state.lock().unwrap().write_to_queue::<SIZE>(0, &frame);
        let mut rx = net.receive().unwrap();
        assert_eq!(rx.packet(), &[0x44; 14]);
        rx.packet_mut()[0] = 0x88;
        assert_eq!(rx.packet()[0], 0x88);
        net.recycle_rx_buffer(rx).unwrap();
        state.lock().unwrap().write_to_queue::<SIZE>(0, &frame);
        // This is the second used-ring entry.
        {
            let state = state.lock().unwrap();
            unsafe {
                ((state.queues[0].device_area + 16) as *mut u32).write_volatile(1);
            }
        }
        assert!(matches!(net.receive(), Err(Error::IoError)));
        assert!(state.lock().unwrap().status.is_empty());
        assert!(matches!(net.receive(), Err(Error::NotReady)));
    }
}

#[derive(Default)]
struct Ownership {
    dma: usize,
    shares: usize,
    attempts: usize,
    require_reset_for_unshare: bool,
    fail_share: Option<usize>,
    state: Option<Arc<Mutex<State>>>,
}
std::thread_local! {
    static OWNERSHIP: std::cell::RefCell<Ownership> = std::cell::RefCell::new(Ownership::default());
}
struct TrackingHal;
unsafe impl crate::Hal for TrackingHal {
    fn dma_alloc(pages: usize, direction: crate::BufferDirection) -> (usize, NonNull<u8>) {
        OWNERSHIP.with(|owner| owner.borrow_mut().dma += 1);
        FakeHal::dma_alloc(pages, direction)
    }
    unsafe fn dma_dealloc(address: usize, pointer: NonNull<u8>, pages: usize) -> i32 {
        OWNERSHIP.with(|owner| {
            let mut owner = owner.borrow_mut();
            if let Some(state) = &owner.state {
                assert!(state.lock().unwrap().status.is_empty());
            }
            owner.dma -= 1;
        });
        unsafe { FakeHal::dma_dealloc(address, pointer, pages) }
    }
    unsafe fn mmio_phys_to_virt(address: usize, size: usize) -> NonNull<u8> {
        unsafe { FakeHal::mmio_phys_to_virt(address, size) }
    }
    unsafe fn share(buffer: NonNull<[u8]>, direction: crate::BufferDirection) -> usize {
        OWNERSHIP.with(|owner| owner.borrow_mut().shares += 1);
        unsafe { FakeHal::share(buffer, direction) }
    }
    unsafe fn try_share(
        buffer: NonNull<[u8]>,
        direction: crate::BufferDirection,
    ) -> crate::Result<usize> {
        let fail = OWNERSHIP.with(|owner| {
            let mut owner = owner.borrow_mut();
            owner.attempts += 1;
            owner.fail_share == Some(owner.attempts)
        });
        if fail {
            Err(Error::DmaError)
        } else {
            Ok(unsafe { Self::share(buffer, direction) })
        }
    }
    unsafe fn unshare(address: usize, buffer: NonNull<[u8]>, direction: crate::BufferDirection) {
        OWNERSHIP.with(|owner| {
            let mut owner = owner.borrow_mut();
            if owner.require_reset_for_unshare {
                assert!(owner
                    .state
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .status
                    .is_empty());
            }
            owner.shares -= 1;
        });
        unsafe { FakeHal::unshare(address, buffer, direction) }
    }
}
fn assert_no_dma_owners() {
    OWNERSHIP.with(|owner| {
        let owner = owner.borrow();
        assert_eq!((owner.dma, owner.shares), (0, 0));
    });
}

#[cfg(feature = "alloc")]
#[test]
fn buffered_initialization_failure_and_drop_retire_all_dma_after_reset() {
    for fail in [None, Some(3)] {
        let mut config = config();
        let (transport, state) = transport(&mut config, false);
        state.lock().unwrap().reset_delay = Some(7);
        OWNERSHIP.with(|owner| {
            *owner.borrow_mut() = Ownership {
                fail_share: fail,
                require_reset_for_unshare: true,
                state: Some(state.clone()),
                ..Default::default()
            }
        });
        let device = VirtIONet::<TrackingHal, _, SIZE>::new(transport, MIN_BUFFER_LEN);
        if fail.is_some() {
            assert!(matches!(device, Err(Error::DmaError)));
        }
        drop(device);
        assert_no_dma_owners();
        let state = state.lock().unwrap();
        assert_eq!(state.reset_acks, 1);
        assert_eq!(state.initialization_before_reset_ack, 0);
    }
}

#[test]
fn second_queue_failure_resets_before_first_queue_dma_is_freed() {
    let mut config = config();
    let (transport, state) = transport(&mut config, false);
    state.lock().unwrap().unavailable_queue = Some(0);
    state.lock().unwrap().reset_delay = Some(5);
    OWNERSHIP.with(|owner| {
        *owner.borrow_mut() = Ownership {
            state: Some(state.clone()),
            ..Default::default()
        }
    });
    assert!(matches!(
        VirtIONetRaw::<TrackingHal, _, SIZE>::new(transport),
        Err(Error::InvalidParam)
    ));
    assert_no_dma_owners();
    assert_eq!(state.lock().unwrap().queue_sets, 1);
    assert_eq!(state.lock().unwrap().reset_acks, 1);
}

#[test]
fn blocking_ring_errors_reset_and_detach_caller_mappings() {
    for receive in [false, true] {
        let mut config = config();
        let (transport, state) = transport(&mut config, false);
        OWNERSHIP.with(|owner| {
            *owner.borrow_mut() = Ownership {
                require_reset_for_unshare: true,
                state: Some(state.clone()),
                ..Default::default()
            }
        });
        let mut net = VirtIONetRaw::<TrackingHal, _, SIZE>::new(transport).unwrap();
        let device = state.clone();
        let worker = thread::spawn(move || {
            let queue = if receive { 0 } else { 1 };
            State::wait_until_queue_notified(&device, queue);
            let mut state = device.lock().unwrap();
            // Publish the malformed ID before the used index, avoiding a race
            // in which the driver could consume a valid completion first.
            let used = state.queues[queue as usize].device_area as *mut u8;
            unsafe {
                used.add(4).cast::<u32>().write_volatile(u32::MAX);
                used.add(8).cast::<u32>().write_volatile(0);
                core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
                used.add(2).cast::<u16>().write_volatile(1);
            }
            state.reset_delay = Some(5);
        });
        let result = if receive {
            net.receive_wait(&mut [0; MIN_BUFFER_LEN]).map(|_| ())
        } else {
            net.send(&[0xab; 14])
        };
        assert!(matches!(result, Err(Error::IoError)));
        worker.join().unwrap();
        assert_eq!(OWNERSHIP.with(|owner| owner.borrow().shares), 0);
        assert!(matches!(net.send(&[0; 14]), Err(Error::NotReady)));
        drop(net);
        assert_no_dma_owners();
    }
}

#[test]
fn legacy_masks_version_one_and_missing_mac_is_rejected() {
    let mut config = config();
    let (mut transport, state) = transport(&mut config, true);
    transport.device_features |= 1 << 32;
    let net = Raw::new(transport).unwrap();
    assert_eq!(state.lock().unwrap().driver_features & (1 << 32), 0);
    assert_eq!(net.fill_buffer_header(&mut [0; 12]).unwrap(), 10);
    drop(net);
    let (mut transport, state) = self::transport(&mut config, false);
    transport.device_features &= !Features::MAC.bits();
    assert!(matches!(Raw::new(transport), Err(Error::Unsupported)));
    assert_eq!(state.lock().unwrap().queue_sets, 0);
}

#[cfg(feature = "alloc")]
#[test]
fn buffered_ring_and_header_errors_never_double_retire_dma() {
    for malformed_id in [false, true] {
        let mut config = config();
        let (transport, state) = transport(&mut config, false);
        OWNERSHIP.with(|owner| {
            *owner.borrow_mut() = Ownership {
                require_reset_for_unshare: malformed_id,
                state: Some(state.clone()),
                ..Default::default()
            }
        });
        let mut net = VirtIONet::<TrackingHal, _, SIZE>::new(transport, MIN_BUFFER_LEN).unwrap();
        state.lock().unwrap().write_to_queue::<SIZE>(0, &[0; 26]);
        if malformed_id {
            overwrite_completion(&state, 0, Some(u32::MAX), None);
        } else {
            overwrite_completion(&state, 0, None, Some(11));
        }
        assert!(matches!(net.receive(), Err(Error::IoError)));
        drop(net);
        assert_no_dma_owners();
    }
}

#[test]
fn initialization_waits_for_old_device_reset_before_queue_publication() {
    let mut config = config();
    let (transport, state) = transport(&mut config, false);
    {
        let mut state = state.lock().unwrap();
        state.status = DeviceStatus::DRIVER_OK;
        state.reset_delay = Some(5);
    }
    let net = Raw::new(transport).unwrap();
    let state_guard = state.lock().unwrap();
    assert_eq!(state_guard.reset_acks, 1);
    assert_eq!(state_guard.initialization_before_reset_ack, 0);
    drop(state_guard);
    drop(net);
}

// Independent reference: fold carry on every 16-bit addition rather than
// accumulating the production helper's wide sum.
fn checksum_reference(bytes: &[u8]) -> u16 {
    let mut sum = 0u16;
    for word in bytes.chunks(2) {
        let word = u16::from_be_bytes([word[0], *word.get(1).unwrap_or(&0)]);
        let (next, carry) = sum.overflowing_add(word);
        sum = next + u16::from(carry);
    }
    sum
}

fn partial_frame(
    legacy: bool,
    ipv6: bool,
    udp: bool,
    payload_len: usize,
) -> (Vec<u8>, Vec<u8>, usize) {
    let layout = if legacy {
        HeaderLayout::Legacy
    } else {
        HeaderLayout::Modern
    };
    let protocol = if udp { 17 } else { 6 };
    let ip_len = if ipv6 { 40 } else { 20 };
    let transport_len = if udp { 8 } else { 20 };
    let segment_len = transport_len + payload_len;
    let offset = if udp { 6 } else { 16 };
    let start = 14 + ip_len;
    let mut pseudo = Vec::new();
    if ipv6 {
        pseudo.extend_from_slice(&[0x20, 1, 0xdb, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        pseudo.extend_from_slice(&[0x20, 1, 0xdb, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        pseudo.extend_from_slice(&(segment_len as u32).to_be_bytes());
        pseudo.extend_from_slice(&[0, 0, 0, protocol]);
    } else {
        pseudo.extend_from_slice(&[192, 0, 2, 1, 192, 0, 2, 2, 0, protocol]);
        pseudo.extend_from_slice(&(segment_len as u16).to_be_bytes());
    }
    let mut frame = vec![0; layout.len() + start + segment_len];
    frame[0] = Flags::NEEDS_CSUM.bits();
    let encode = |value: u16| {
        if legacy {
            value.to_ne_bytes()
        } else {
            value.to_le_bytes()
        }
    };
    frame[6..8].copy_from_slice(&encode(start as u16));
    frame[8..10].copy_from_slice(&encode(offset as u16));
    let packet = &mut frame[layout.len()..];
    packet[..6].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
    packet[6..12].copy_from_slice(&[2, 0, 0, 0, 0, 2]);
    packet[12..14].copy_from_slice(&if ipv6 { 0x86ddu16 } else { 0x0800u16 }.to_be_bytes());
    if ipv6 {
        packet[14] = 0x60;
        packet[18..20].copy_from_slice(&(segment_len as u16).to_be_bytes());
        packet[20] = protocol;
        packet[21] = 64;
        packet[22..54].copy_from_slice(&pseudo[..32]);
    } else {
        packet[14] = 0x45;
        packet[16..18].copy_from_slice(&((ip_len + segment_len) as u16).to_be_bytes());
        packet[22] = 64;
        packet[23] = protocol;
        packet[26..34].copy_from_slice(&pseudo[..8]);
        let ip_sum = !checksum_reference(&packet[14..34]);
        packet[24..26].copy_from_slice(&ip_sum.to_be_bytes());
    }
    let segment = &mut packet[start..];
    segment[..4].copy_from_slice(&[0x12, 0x34, 0x56, 0x78]);
    if udp {
        segment[4..6].copy_from_slice(&(segment_len as u16).to_be_bytes());
    } else {
        segment[12] = 0x50;
        segment[13] = 0x18;
    }
    for (index, byte) in segment[transport_len..].iter_mut().enumerate() {
        *byte = (index as u8).wrapping_mul(3).wrapping_add(1);
    }
    segment[offset..offset + 2].copy_from_slice(&checksum_reference(&pseudo).to_be_bytes());
    (frame, pseudo, start)
}

#[test]
fn partial_checksums_complete_ipv4_ipv6_tcp_udp_with_pseudo_header_seed() {
    for legacy in [true, false] {
        for ipv6 in [false, true] {
            for udp in [false, true] {
                let mut config = config();
                let (transport, state) = transport(&mut config, legacy);
                let mut net = Raw::new(transport).unwrap();
                assert_ne!(
                    state.lock().unwrap().driver_features & Features::GUEST_CSUM.bits(),
                    0
                );
                for payload_len in [0, 1, 17, 18] {
                    let (frame, mut pseudo, start) = partial_frame(legacy, ipv6, udp, payload_len);
                    let mut rx = vec![0; MIN_BUFFER_LEN];
                    let token = unsafe { net.receive_begin(&mut rx) }.unwrap();
                    state.lock().unwrap().write_to_queue::<SIZE>(0, &frame);
                    let (header, packet) = unsafe { net.receive_complete(token, &mut rx) }.unwrap();
                    assert_eq!(rx[0] & Flags::NEEDS_CSUM.bits(), 0);
                    pseudo.extend_from_slice(&rx[header + start..header + packet]);
                    assert_eq!(checksum_reference(&pseudo), u16::MAX);
                }
            }
        }
    }
}

#[test]
fn checksum_metadata_bounds_odd_positions_and_large_buffers_match_linux_helper() {
    for legacy in [true, false] {
        let layout = if legacy {
            HeaderLayout::Legacy
        } else {
            HeaderLayout::Modern
        };
        for (start, offset, length) in [(0u16, 0u16, 2usize), (1, 1, 7), (3, 2, 8), (0, 2, 262145)]
        {
            let mut frame = vec![0xff; layout.len() + length];
            frame[..layout.len()].fill(0);
            frame[0] = Flags::NEEDS_CSUM.bits();
            let encode = |value: u16| {
                if legacy {
                    value.to_ne_bytes()
                } else {
                    value.to_le_bytes()
                }
            };
            frame[6..8].copy_from_slice(&encode(start));
            frame[8..10].copy_from_slice(&encode(offset));
            let expected = !checksum_reference(&frame[layout.len() + start as usize..]);
            let expected = if expected == 0 { u16::MAX } else { expected };
            let used = frame.len();
            layout.receive(&mut frame, used, true).unwrap();
            let field = layout.len() + start as usize + offset as usize;
            assert_eq!(&frame[field..field + 2], &expected.to_be_bytes());
        }
        for (start, offset, used_len) in [
            (0u16, 0u16, 1usize),
            (1, 1, 3),
            (u16::MAX, 1, 20),
            (1, u16::MAX, 20),
        ] {
            let mut frame = vec![0; layout.len() + 32];
            frame[0] = Flags::NEEDS_CSUM.bits();
            let encode = |value: u16| {
                if legacy {
                    value.to_ne_bytes()
                } else {
                    value.to_le_bytes()
                }
            };
            frame[6..8].copy_from_slice(&encode(start));
            frame[8..10].copy_from_slice(&encode(offset));
            let before = frame.clone();
            assert!(matches!(
                layout.receive(&mut frame, layout.len() + used_len, true),
                Err(Error::IoError)
            ));
            assert_eq!(frame, before);
        }
    }
}

#[test]
fn checksum_completion_rejects_unnegotiated_and_segmented_metadata_before_mutation() {
    for kind in 0..3 {
        let (mut frame, _, _) = partial_frame(false, false, false, 3);
        if kind == 1 {
            frame[0] |= Flags::RSC_INFO.bits();
        }
        if kind == 2 {
            frame[1] = GsoType::TCPV4.0;
        }
        let before = frame.clone();
        let len = frame.len();
        assert!(matches!(
            HeaderLayout::Modern.receive(&mut frame, len, kind != 0),
            Err(Error::IoError)
        ));
        assert_eq!(frame, before);
    }
}

#[test]
fn blocking_receive_completes_partial_checksum() {
    let mut config = config();
    let (transport, state) = transport(&mut config, false);
    let mut net = Raw::new(transport).unwrap();
    let (frame, mut pseudo, start) = partial_frame(false, true, true, 19);
    let worker = thread::spawn(move || {
        State::wait_until_queue_notified(&state, 0);
        state.lock().unwrap().write_to_queue::<SIZE>(0, &frame);
    });
    let mut rx = vec![0; MIN_BUFFER_LEN];
    let (header, length) = net.receive_wait(&mut rx).unwrap();
    worker.join().unwrap();
    pseudo.extend_from_slice(&rx[header + start..header + length]);
    assert_eq!(checksum_reference(&pseudo), u16::MAX);
}

#[cfg(feature = "alloc")]
#[test]
fn buffered_receive_completes_partial_checksum_and_retires_malformed_metadata() {
    for malformed in [false, true] {
        let mut config = config();
        let (transport, state) = transport(&mut config, false);
        OWNERSHIP.with(|owner| {
            *owner.borrow_mut() = Ownership {
                state: Some(state.clone()),
                ..Default::default()
            }
        });
        let mut net = VirtIONet::<TrackingHal, _, SIZE>::new(transport, MIN_BUFFER_LEN).unwrap();
        let (mut frame, mut pseudo, start) = partial_frame(false, false, false, 21);
        if malformed {
            frame[6..8].copy_from_slice(&u16::MAX.to_le_bytes());
        }
        state.lock().unwrap().write_to_queue::<SIZE>(0, &frame);
        if malformed {
            assert!(matches!(net.receive(), Err(Error::IoError)));
        } else {
            let rx = net.receive().unwrap();
            assert_eq!(rx.header().flags.bits() & Flags::NEEDS_CSUM.bits(), 0);
            pseudo.extend_from_slice(&rx.packet()[start..]);
            assert_eq!(checksum_reference(&pseudo), u16::MAX);
            net.recycle_rx_buffer(rx).unwrap();
        }
        drop(net);
        assert_no_dma_owners();
    }
}

fn offer_tcp_offloads(transport: &mut FakeTransport<Config>) {
    transport.device_features |=
        (Features::GUEST_TSO4 | Features::GUEST_TSO6 | Features::GUEST_ECN).bits();
}

fn make_gso(frame: &mut [u8], legacy: bool, kind: u8, size: u16) {
    frame[1] = kind;
    frame[4..6].copy_from_slice(&if legacy {
        size.to_ne_bytes()
    } else {
        size.to_le_bytes()
    });
    // hdr_len is a hint, not a trusted packet boundary.
    frame[2..4].copy_from_slice(&[0xff, 0xff]);
}

#[test]
fn receive_segmentation_is_opt_in_and_uses_actual_negotiated_features() {
    for legacy in [true, false] {
        let mut config = config();
        let (mut device, state) = transport(&mut config, legacy);
        offer_tcp_offloads(&mut device);
        let default = Raw::new(device).unwrap();
        assert_eq!(
            state.lock().unwrap().driver_features
                & (Features::GUEST_TSO4 | Features::GUEST_TSO6 | Features::GUEST_ECN).bits(),
            0
        );
        let (mut frame, _, _) = partial_frame(legacy, false, false, 20);
        make_gso(&mut frame, legacy, 1, 1448);
        let used = frame.len();
        assert!(matches!(
            default.receive_parser().prepare(&mut frame, used),
            Err(Error::IoError)
        ));
        drop(default);

        let (mut device, state) = transport(&mut config, legacy);
        device.device_features |= Features::GUEST_TSO4.bits();
        let net = Raw::new_with_rx_offloads(device, RxOffloads::all()).unwrap();
        assert_ne!(
            state.lock().unwrap().driver_features & Features::GUEST_TSO4.bits(),
            0
        );
        assert_eq!(
            state.lock().unwrap().driver_features
                & (Features::GUEST_TSO6 | Features::GUEST_ECN).bits(),
            0
        );
        assert!(net
            .receive_parser()
            .prepare(&mut frame, used)
            .unwrap()
            .2
            .gso
            .is_some());
        make_gso(&mut frame, legacy, 4, 1448);
        assert!(matches!(
            net.receive_parser().prepare(&mut frame, used),
            Err(Error::IoError)
        ));
        make_gso(&mut frame, legacy, 0x81, 1448);
        assert!(matches!(
            net.receive_parser().prepare(&mut frame, used),
            Err(Error::IoError)
        ));
    }
}

#[test]
fn receive_segmentation_requires_checksum_and_valid_ecn_dependency() {
    let mut config = config();
    let (mut device, state) = transport(&mut config, false);
    offer_tcp_offloads(&mut device);
    device.device_features &= !Features::GUEST_CSUM.bits();
    assert!(matches!(
        Raw::new_with_rx_offloads(device, RxOffloads::all()),
        Err(Error::Unsupported)
    ));
    assert_eq!(state.lock().unwrap().queue_sets, 0);
    let (device, state) = transport(&mut config, false);
    assert!(matches!(
        Raw::new_with_rx_offloads(device, RxOffloads::ECN),
        Err(Error::InvalidParam)
    ));
    assert_eq!(state.lock().unwrap().queue_sets, 0);
}

#[test]
fn receive_segmentation_checks_wire_capacity_before_dma_publication() {
    let mut config = config();
    let (mut device, state) = transport(&mut config, false);
    offer_tcp_offloads(&mut device);
    let mut net = Raw::new_with_rx_offloads(device, RxOffloads::all()).unwrap();
    let mut too_small = vec![0; MIN_GSO_RX_BUFFER_LEN - 1];
    assert!(matches!(
        unsafe { net.receive_begin(&mut too_small) },
        Err(Error::InvalidParam)
    ));
    let mut buffer = vec![0; MIN_GSO_RX_BUFFER_LEN];
    let token = unsafe { net.receive_begin(&mut buffer) }.unwrap();
    let (mut frame, _, _) = partial_frame(false, false, false, 19);
    make_gso(&mut frame, false, 1, 1448);
    state.lock().unwrap().write_to_queue::<SIZE>(0, &frame);
    let parser = net.receive_parser();
    let used = unsafe { net.reclaim_receive(token, &mut buffer) }.unwrap();
    // An immutable parser remains usable without any reference to a driver
    // lock or live device once this descriptor has been reclaimed.
    drop(net);
    assert!(parser.prepare(&mut buffer, used).unwrap().2.gso.is_some());
}

#[test]
fn gso_metadata_preserves_seed_and_decodes_both_wire_endiannesses() {
    for legacy in [true, false] {
        for (kind, expected) in [
            (1, TcpGsoKind::TcpV4),
            (4, TcpGsoKind::TcpV6),
            (0x81, TcpGsoKind::TcpV4),
            (0x84, TcpGsoKind::TcpV6),
        ] {
            let mut config = config();
            let (mut device, _) = transport(&mut config, legacy);
            offer_tcp_offloads(&mut device);
            let net = Raw::new_with_rx_offloads(device, RxOffloads::all()).unwrap();
            let (mut frame, _, start) =
                partial_frame(legacy, expected == TcpGsoKind::TcpV6, false, 21);
            make_gso(&mut frame, legacy, kind, 1234);
            let original = frame.clone();
            let used = frame.len();
            let (header, packet, metadata) =
                net.receive_parser().prepare(&mut frame, used).unwrap();
            assert_eq!(
                (header, packet),
                (if legacy { 10 } else { 12 }, used - header)
            );
            assert_eq!(
                metadata.gso,
                Some(TcpGsoMetadata {
                    kind: expected,
                    ecn: kind & 0x80 != 0,
                    gso_size: 1234,
                    checksum: Some(PartialChecksum {
                        start: start as u16,
                        offset: 16
                    }),
                })
            );
            assert_eq!(frame, original);
            frame[0] &= !Flags::NEEDS_CSUM.bits();
            // Fields not enabled by NEEDS_CSUM are not interpreted as a
            // checksum location, even when an aggregate has a full checksum.
            frame[6..10].fill(0xff);
            assert_eq!(
                net.receive_parser()
                    .prepare(&mut frame, used)
                    .unwrap()
                    .2
                    .gso
                    .unwrap()
                    .checksum,
                None
            );
        }
    }
}

#[test]
fn invalid_gso_metadata_is_rejected_after_reclaim_without_mutating_or_losing_slot() {
    for bad in 0..7 {
        let mut config = config();
        let (mut device, state) = transport(&mut config, false);
        offer_tcp_offloads(&mut device);
        let mut net = Raw::new_with_rx_offloads(device, RxOffloads::all()).unwrap();
        let mut buffer = vec![0; MIN_GSO_RX_BUFFER_LEN];
        let token = unsafe { net.receive_begin(&mut buffer) }.unwrap();
        let (mut frame, _, _) = partial_frame(false, false, false, 21);
        make_gso(&mut frame, false, 1, 1448);
        match bad {
            0 => frame[4..6].fill(0),
            1 => frame[4..6].fill(0xff),
            2 => frame[1] = GsoType::UDP.0,
            3 => frame[1] = 0x80,
            4 => frame[0] |= Flags::RSC_INFO.bits(),
            5 => frame[6..8].fill(0xff),
            6 => frame[8..10].fill(0xff),
            _ => unreachable!(),
        }
        state.lock().unwrap().write_to_queue::<SIZE>(0, &frame);
        let used = unsafe { net.reclaim_receive(token, &mut buffer) }.unwrap();
        let before = buffer.clone();
        assert!(matches!(
            net.receive_parser().prepare(&mut buffer, used),
            Err(Error::IoError)
        ));
        assert_eq!(buffer, before);
        // Parsing failure happened after DMA retirement. Reusing the same
        // allocation is safe without a device reset or duplicate detach.
        let token = unsafe { net.receive_begin(&mut buffer) }.unwrap();
        state.lock().unwrap().write_to_queue::<SIZE>(0, &[0; 26]);
        unsafe { net.receive_complete_with_metadata(token, &mut buffer) }.unwrap();
    }
}

#[test]
fn old_packet_completion_rejects_gso_while_metadata_completion_returns_it() {
    let mut config = config();
    let (mut device, state) = transport(&mut config, false);
    offer_tcp_offloads(&mut device);
    let mut net = Raw::new_with_rx_offloads(device, RxOffloads::all()).unwrap();
    let mut buffer = vec![0; MIN_GSO_RX_BUFFER_LEN];
    let (mut frame, _, _) = partial_frame(false, false, false, 21);
    make_gso(&mut frame, false, 1, 1448);
    let token = unsafe { net.receive_begin(&mut buffer) }.unwrap();
    state.lock().unwrap().write_to_queue::<SIZE>(0, &frame);
    assert!(matches!(
        unsafe { net.receive_complete(token, &mut buffer) },
        Err(Error::IoError)
    ));
    let token = unsafe { net.receive_begin(&mut buffer) }.unwrap();
    state.lock().unwrap().write_to_queue::<SIZE>(0, &frame);
    assert!(
        unsafe { net.receive_complete_with_metadata(token, &mut buffer) }
            .unwrap()
            .2
            .gso
            .is_some()
    );
}

#[test]
fn metadata_api_queue_error_keeps_dma_ownership_until_real_reclamation() {
    let mut config = config();
    let (mut device, state) = transport(&mut config, false);
    offer_tcp_offloads(&mut device);
    OWNERSHIP.with(|owner| {
        *owner.borrow_mut() = Ownership {
            state: Some(state.clone()),
            ..Default::default()
        }
    });
    let mut net =
        VirtIONetRaw::<TrackingHal, _, SIZE>::new_with_rx_offloads(device, RxOffloads::all())
            .unwrap();
    let mut buffer = vec![0; MIN_GSO_RX_BUFFER_LEN];
    let token = unsafe { net.receive_begin(&mut buffer) }.unwrap();
    let (mut frame, _, _) = partial_frame(false, false, false, 21);
    make_gso(&mut frame, false, 1, 1448);
    state.lock().unwrap().write_to_queue::<SIZE>(0, &frame);
    overwrite_completion(&state, 0, Some(u32::MAX), None);
    assert!(matches!(
        unsafe { net.reclaim_receive(token, &mut buffer) },
        Err(Error::IoError)
    ));
    assert_eq!(OWNERSHIP.with(|owner| owner.borrow().shares), 1);
    // Restore the fake device's ID: the failed pop must neither advance the
    // used ring nor detach the original buffer's mapping.
    overwrite_completion(&state, 0, Some(token as u32), None);
    let used = unsafe { net.reclaim_receive(token, &mut buffer) }.unwrap();
    assert_eq!(OWNERSHIP.with(|owner| owner.borrow().shares), 0);
    assert!(net
        .receive_parser()
        .prepare(&mut buffer, used)
        .unwrap()
        .2
        .gso
        .is_some());
    drop(net);
    assert_no_dma_owners();
}
