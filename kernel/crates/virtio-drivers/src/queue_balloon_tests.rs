//! The three independent queues used by a balloon share no completion/owner
//! state. Exercise the real ring implementation with a deliberately late host.
use super::*;
use crate::{
    hal::fake::FakeHal,
    transport::{
        fake::{FakeTransport, QueueStatus, State},
        DeviceStatus, DeviceType,
    },
};
use std::sync::{Arc, Mutex};

fn fixture(config: &mut [u32; 2]) -> (FakeTransport<[u32; 2]>, Arc<Mutex<State>>) {
    let state = Arc::new(Mutex::new(State {
        queues: vec![
            QueueStatus::default(),
            QueueStatus::default(),
            QueueStatus::default(),
        ],
        status: DeviceStatus::DRIVER_OK,
        ..Default::default()
    }));
    let transport = FakeTransport {
        device_type: DeviceType::MemoryBalloon,
        max_queue_size: 8,
        device_features: (1 << 32) | (1 << 5) | (1 << 2),
        config_space: NonNull::from(config),
        state: state.clone(),
    };
    (transport, state)
}

#[test]
fn stalled_reporting_does_not_block_inflate_or_oom_deflate() {
    let mut config = [0, 0];
    let (mut transport, state) = fixture(&mut config);
    let mut inflate = VirtQueue::<FakeHal, 8>::new(&mut transport, 0, false, false).unwrap();
    let mut deflate = VirtQueue::<FakeHal, 8>::new(&mut transport, 1, false, false).unwrap();
    let mut reporting = VirtQueue::<FakeHal, 8>::new(&mut transport, 2, false, false).unwrap();
    // Polling mode suppresses queue IRQs, while a config ISR can still need
    // acknowledgement independently of the avail-ring notification flag.
    for queue in [&mut inflate, &mut deflate, &mut reporting] {
        queue.set_dev_notify(false);
        assert_eq!(
            unsafe { (*queue.avail.as_ptr()).flags.load(Ordering::Acquire) },
            1
        );
    }
    state.lock().unwrap().interrupt_pending = true;
    assert!(transport.ack_interrupt());
    assert!(!transport.ack_interrupt());
    // PFN 0xffffffff must survive the wire format without truncation/sign extension.
    let pfns = [1u32.to_le_bytes(), u32::MAX.to_le_bytes()].concat();
    let mut reported_memory = Box::new([0xa5; 4096]);
    let report_token = unsafe { reporting.add(&[], &mut [&mut reported_memory[..]]) }.unwrap();
    let inflate_token = unsafe { inflate.add(&[&pfns], &mut []) }.unwrap();
    let received = state.lock().unwrap().read_from_queue::<8>(0);
    assert_eq!(received, [1, 0, 0, 0, 255, 255, 255, 255]);
    assert_eq!(
        unsafe { inflate.pop_used(inflate_token, &[&pfns], &mut []) },
        Ok(8)
    );
    assert!(!reporting.can_pop());
    assert_eq!(reporting.available_desc(), 7);
    let deflate_token = unsafe { deflate.add(&[&pfns], &mut []) }.unwrap();
    assert_eq!(state.lock().unwrap().read_from_queue::<8>(1), pfns);
    assert_eq!(
        unsafe { deflate.pop_used(deflate_token, &[&pfns], &mut []) },
        Ok(8)
    );
    assert!(!reporting.can_pop());
    // Only the reporting acknowledgement permits reusing the physical block.
    state.lock().unwrap().write_to_queue::<8>(2, &[0; 4096]);
    assert_eq!(
        unsafe { reporting.pop_used(report_token, &[], &mut [&mut reported_memory[..]]) },
        Ok(4096)
    );
    assert_eq!(reported_memory[..], [0; 4096]);
    assert_eq!(reporting.available_desc(), 8);
}

#[test]
fn invalid_balloon_completion_preserves_descriptor_and_buffer_ownership() {
    let mut config = [0, 0];
    let (mut transport, _state) = fixture(&mut config);
    let mut queue = VirtQueue::<FakeHal, 8>::new(&mut transport, 0, false, false).unwrap();
    let pfns = u32::MAX.to_le_bytes();
    let token = unsafe { queue.add(&[&pfns], &mut []) }.unwrap();
    // An elapsed caller deadline leaves the request on the ring; a pop before
    // acknowledgement cannot recycle it or release its DMA mapping.
    assert_eq!(
        unsafe { queue.pop_used(token, &[&pfns], &mut []) },
        Err(Error::NotReady)
    );
    unsafe {
        (*queue.used.as_ptr()).ring[0].id = u32::MAX;
        (*queue.used.as_ptr()).idx.store(1, Ordering::Release);
    }
    assert_eq!(
        unsafe { queue.pop_used(token, &[&pfns], &mut []) },
        Err(Error::IoError)
    );
    assert_eq!(queue.available_desc(), 7);
    assert_eq!(queue.last_used_idx, 0);
    unsafe {
        (*queue.used.as_ptr()).ring[0].id = ((token + 1) % 8) as u32;
    }
    assert_eq!(
        unsafe { queue.pop_used(token, &[&pfns], &mut []) },
        Err(Error::WrongToken)
    );
    assert_eq!(queue.available_desc(), 7);
    assert_eq!(queue.last_used_idx, 0);
    // A valid late acknowledgement still completes the original descriptor.
    unsafe {
        (*queue.used.as_ptr()).ring[0].id = token as u32;
    }
    assert_eq!(unsafe { queue.pop_used(token, &[&pfns], &mut []) }, Ok(0));
    assert_eq!(queue.available_desc(), 8);
}

#[test]
fn unconfirmed_reset_keeps_pending_reporting_dma_live_until_ack() {
    let mut config = [0, 0];
    let (mut transport, state) = fixture(&mut config);
    let mut reporting = VirtQueue::<FakeHal, 8>::new(&mut transport, 2, false, false).unwrap();
    let mut memory = Box::new([0x6d; 4096]);
    let token = unsafe { reporting.add(&[], &mut [&mut memory[..]]) }.unwrap();
    let ring_address = state.lock().unwrap().queues[2].descriptors;
    state.lock().unwrap().reset_delay = Some(usize::MAX);
    assert_eq!(transport.reset(), Err(Error::ResetTimeout));
    assert_eq!(state.lock().unwrap().reset_acks, 0);
    assert_eq!(reporting.available_desc(), 7);
    assert_eq!(state.lock().unwrap().queues[2].descriptors, ring_address);
    // A timed-out reset is not permission to free the ring or target: the
    // delayed device can still discard/write the submitted page afterwards.
    state.lock().unwrap().write_to_queue::<8>(2, &[0; 4096]);
    assert_eq!(
        unsafe { reporting.pop_used(token, &[], &mut [&mut memory[..]]) },
        Ok(4096)
    );
    assert_eq!(memory[..], [0; 4096]);
    state.lock().unwrap().reset_delay = Some(2);
    assert_eq!(transport.reset(), Ok(()));
    assert_eq!(state.lock().unwrap().reset_acks, 1);
    drop(reporting);
    drop(memory);
}

#[test]
fn declined_optional_features_preserve_host_reporting_queue_slots() {
    // CubeSandbox exposes only inflate/deflate/reporting. Hosts exposing
    // stats and/or hint retain those slots even when the guest declines them.
    for (optional_features, host_reporting_index) in [
        (0, 2u16),
        (1 << 1, 3),
        (1 << 3, 3),
        ((1 << 1) | (1 << 3), 4),
    ] {
        let mut config = [0, 0];
        let (mut transport, state) = fixture(&mut config);
        transport.device_features |= optional_features;
        state
            .lock()
            .unwrap()
            .queues
            .resize_with(host_reporting_index as usize + 1, QueueStatus::default);
        let offered = transport.read_device_features();
        // Only VERSION_1 and REPORTING are supported by this fixture guest.
        transport.write_driver_features((1 << 32) | (1 << 5));
        assert_eq!(
            state.lock().unwrap().driver_features & ((1 << 1) | (1 << 3)),
            0
        );
        let queue_index =
            2 + u16::from(offered & (1 << 1) != 0) + u16::from(offered & (1 << 3) != 0);
        assert_eq!(queue_index, host_reporting_index);
        let mut reporting =
            VirtQueue::<FakeHal, 8>::new(&mut transport, queue_index, false, false).unwrap();
        let mut memory = Box::new([0x91; 4096]);
        let token = unsafe { reporting.add(&[], &mut [&mut memory[..]]) }.unwrap();
        transport.notify(queue_index);
        {
            let mut host = state.lock().unwrap();
            for index in 2..host_reporting_index as usize {
                // Unsupported queue slots exist, but the guest never publishes
                // rings or accidentally submits writable report pages to them.
                assert_eq!(host.queues[index].descriptors, 0);
            }
            assert!(host.queues[host_reporting_index as usize]
                .notified
                .load(Ordering::Acquire));
            host.write_to_queue::<8>(host_reporting_index, &[0; 4096]);
        }
        assert_eq!(
            unsafe { reporting.pop_used(token, &[], &mut [&mut memory[..]]) },
            Ok(4096)
        );
        assert_eq!(memory[..], [0; 4096]);
    }
}
