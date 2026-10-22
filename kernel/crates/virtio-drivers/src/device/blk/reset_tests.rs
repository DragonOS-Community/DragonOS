use super::*;
use crate::{transport::DeviceStatus, BufferDirection, Hal, PhysAddr};
use core::cell::{Cell, RefCell};

std::thread_local! {
    static DMA_LIVE: Cell<usize> = const { Cell::new(0) };
    static SHARES: Cell<usize> = const { Cell::new(0) };
    static EARLY_RETIRE: Cell<usize> = const { Cell::new(0) };
    static DEVICE: RefCell<Option<Arc<Mutex<State>>>> = const { RefCell::new(None) };
}

fn observe_retirement() {
    DEVICE.with(|device| {
        if device
            .borrow()
            .as_ref()
            .is_some_and(|state| state.lock().unwrap().reset_acks == 0)
        {
            EARLY_RETIRE.with(|count| count.set(count.get() + 1));
        }
    });
}

struct TrackedHal;
unsafe impl Hal for TrackedHal {
    fn dma_alloc(pages: usize, direction: BufferDirection) -> (PhysAddr, NonNull<u8>) {
        let result = FakeHal::dma_alloc(pages, direction);
        DMA_LIVE.with(|count| count.set(count.get() + 1));
        result
    }
    unsafe fn dma_dealloc(paddr: PhysAddr, vaddr: NonNull<u8>, pages: usize) -> i32 {
        observe_retirement();
        DMA_LIVE.with(|count| count.set(count.get() - 1));
        unsafe { FakeHal::dma_dealloc(paddr, vaddr, pages) }
    }
    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, size: usize) -> NonNull<u8> {
        unsafe { FakeHal::mmio_phys_to_virt(paddr, size) }
    }
    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection) -> PhysAddr {
        SHARES.with(|count| count.set(count.get() + 1));
        unsafe { FakeHal::share(buffer, direction) }
    }
    unsafe fn unshare(paddr: PhysAddr, buffer: NonNull<[u8]>, direction: BufferDirection) {
        observe_retirement();
        SHARES.with(|count| count.set(count.get() - 1));
        unsafe { FakeHal::unshare(paddr, buffer, direction) }
    }
}

fn transport_fixture(
    indirect: bool,
) -> (FakeTransport<BlkConfig>, Arc<Mutex<State>>, Box<BlkConfig>) {
    DMA_LIVE.with(|count| count.set(0));
    SHARES.with(|count| count.set(0));
    EARLY_RETIRE.with(|count| count.set(0));
    let mut config = Box::new(BlkConfig {
        capacity_low: Volatile::new(66),
        capacity_high: Volatile::new(0),
        size_max: Volatile::new(0),
        seg_max: Volatile::new(0),
        cylinders: Volatile::new(0),
        heads: Volatile::new(0),
        sectors: Volatile::new(0),
        blk_size: Volatile::new(0),
        physical_block_exp: Volatile::new(0),
        alignment_offset: Volatile::new(0),
        min_io_size: Volatile::new(0),
        opt_io_size: Volatile::new(0),
    });
    let state = Arc::new(Mutex::new(State {
        queues: vec![QueueStatus::default()],
        ..Default::default()
    }));
    DEVICE.with(|device| *device.borrow_mut() = Some(state.clone()));
    let features = BlkFeature::FLUSH
        | if indirect {
            BlkFeature::RING_INDIRECT_DESC
        } else {
            BlkFeature::empty()
        };
    let transport = FakeTransport {
        device_type: DeviceType::Block,
        max_queue_size: QUEUE_SIZE.into(),
        device_features: features.bits(),
        config_space: NonNull::from(config.as_mut()),
        state: state.clone(),
    };
    (transport, state, config)
}

fn fixture(
    indirect: bool,
) -> (
    VirtIOBlk<TrackedHal, FakeTransport<BlkConfig>>,
    Arc<Mutex<State>>,
    Box<BlkConfig>,
) {
    let (transport, state, config) = transport_fixture(indirect);
    (VirtIOBlk::new(transport).unwrap(), state, config)
}

#[test]
fn initialization_confirms_reset_before_negotiating_or_publishing_queue() {
    let (transport, state, _config) = transport_fixture(true);
    {
        let mut state = state.lock().unwrap();
        state.status = DeviceStatus::DRIVER_OK;
        state.reset_delay = Some(3);
    }
    let blk = VirtIOBlk::<TrackedHal, _>::new(transport).unwrap();
    {
        let state = state.lock().unwrap();
        assert_eq!(state.reset_acks, 1);
        assert_eq!(state.reset_zero_writes, 1);
        assert_eq!(state.initialization_before_reset_ack, 0);
        assert_eq!(state.queue_sets, 1);
        assert_eq!(state.config_reads, 1);
    }
    drop(blk);
    assert_eq!(DMA_LIVE.with(Cell::get), 0);
    assert_eq!(EARLY_RETIRE.with(Cell::get), 0);
}

#[test]
fn initialization_reset_timeout_publishes_no_queue_or_dma() {
    let (transport, state, _config) = transport_fixture(false);
    {
        let mut state = state.lock().unwrap();
        state.status = DeviceStatus::DRIVER_OK;
        state.reset_delay = Some(usize::MAX);
    }
    assert!(matches!(
        VirtIOBlk::<TrackedHal, _>::new(transport),
        Err(Error::ResetTimeout)
    ));
    let state = state.lock().unwrap();
    assert_eq!(state.reset_acks, 0);
    assert_eq!(state.reset_zero_writes, 2);
    assert_eq!(state.transport_drops, 0);
    assert_eq!(state.initialization_before_reset_ack, 0);
    assert_eq!(state.queue_sets, 0);
    assert_eq!(state.config_reads, 0);
    assert_eq!(DMA_LIVE.with(Cell::get), 0);
    assert_eq!(SHARES.with(Cell::get), 0);
}

#[test]
fn constructor_errors_retain_transport_when_cleanup_reset_cannot_ack() {
    for config_failure in [true, false] {
        let (mut transport, state, _config) = transport_fixture(false);
        {
            let mut state = state.lock().unwrap();
            state.reset_delay = Some(usize::MAX);
            if config_failure {
                state.config_error = Some(Error::ConfigSpaceMissing);
            }
        }
        if !config_failure {
            transport.max_queue_size = 1;
        }
        let expected = if config_failure {
            Error::ConfigSpaceMissing
        } else {
            Error::InvalidParam
        };
        assert!(
            matches!(VirtIOBlk::<TrackedHal, _>::new(transport), Err(error) if error == expected)
        );
        let state = state.lock().unwrap();
        assert_eq!(state.transport_drops, 0);
        assert_eq!(state.reset_acks, 0);
        assert_eq!(state.queue_sets, 0);
        assert_eq!(DMA_LIVE.with(Cell::get), 0);
    }
}

#[test]
fn constructor_errors_drop_transport_after_successful_cleanup_reset() {
    for config_failure in [true, false] {
        let (mut transport, state, _config) = transport_fixture(false);
        {
            let mut state = state.lock().unwrap();
            state.reset_delay = Some(3);
            if config_failure {
                state.config_error = Some(Error::ConfigSpaceMissing);
            }
        }
        if !config_failure {
            transport.max_queue_size = 1;
        }
        let expected = if config_failure {
            Error::ConfigSpaceMissing
        } else {
            Error::InvalidParam
        };
        assert!(
            matches!(VirtIOBlk::<TrackedHal, _>::new(transport), Err(error) if error == expected)
        );
        let state = state.lock().unwrap();
        assert_eq!(state.transport_drops, 1);
        assert_eq!(state.reset_acks, 1);
        assert_eq!(state.queue_sets, 0);
        assert_eq!(DMA_LIVE.with(Cell::get), 0);
    }
}

#[test]
fn reset_ack_precedes_cancel_and_dma_retirement() {
    for indirect in [false, true] {
        let (mut blk, state, _config) = fixture(indirect);
        let mut req = BlkReq::default();
        let mut resp = BlkResp::default();
        let mut buffer = [0; SECTOR_SIZE];
        let token = unsafe { blk.read_blocks_nb(0, &mut req, &mut buffer, &mut resp) }.unwrap();
        assert_eq!(
            unsafe { blk.cancel_read_blocks(token, &req, &mut buffer, &mut resp) },
            Err(Error::NotReady)
        );
        let shares = SHARES.with(Cell::get);
        assert!(shares >= 3); // FakeHal shares bounce buffers, including indirect tables.
        state.lock().unwrap().reset_delay = Some(3);
        blk.reset().unwrap();
        assert_eq!(state.lock().unwrap().reset_acks, 1);
        assert_eq!(SHARES.with(Cell::get), shares);
        assert_eq!(
            unsafe { blk.read_blocks_nb(0, &mut req, &mut buffer, &mut resp) },
            Err(Error::NotReady)
        );
        unsafe { blk.cancel_read_blocks(token, &req, &mut buffer, &mut resp) }.unwrap();
        assert_eq!(SHARES.with(Cell::get), 0);
        drop(blk);
        assert_eq!(DMA_LIVE.with(Cell::get), 0);
        assert_eq!(EARLY_RETIRE.with(Cell::get), 0);
        let state = state.lock().unwrap();
        assert_eq!(state.reset_requests, 1);
        assert_eq!(state.transport_drops, 1);
    }
}

#[test]
fn failed_reset_keeps_dma_then_retry_detaches_once() {
    let (mut blk, state, _config) = fixture(true);
    let mut req = BlkReq::default();
    let mut resp = BlkResp::default();
    let buffer = [0x5a; SECTOR_SIZE];
    let token = unsafe { blk.write_blocks_nb(0, &mut req, &buffer, &mut resp) }.unwrap();
    state.lock().unwrap().reset_delay = Some(usize::MAX);
    assert_eq!(blk.reset(), Err(Error::ResetTimeout));
    assert_eq!(state.lock().unwrap().reset_acks, 0);
    assert!(DMA_LIVE.with(Cell::get) > 0);
    assert!(SHARES.with(Cell::get) > 0);
    assert_eq!(
        unsafe { blk.cancel_write_blocks(token, &req, &buffer, &mut resp) },
        Err(Error::NotReady)
    );
    state.lock().unwrap().reset_delay = Some(0);
    blk.reset().unwrap();
    unsafe { blk.cancel_write_blocks(token, &req, &buffer, &mut resp) }.unwrap();
    assert_eq!(SHARES.with(Cell::get), 0);
    drop(blk);
    assert_eq!(DMA_LIVE.with(Cell::get), 0);
    assert_eq!(EARLY_RETIRE.with(Cell::get), 0);
}

#[test]
fn reset_cancels_flush_mappings() {
    for indirect in [false, true] {
        let (mut blk, state, _config) = fixture(indirect);
        let mut req = BlkReq::default();
        let mut resp = BlkResp::default();
        let token = unsafe { blk.flush_nb(&mut req, &mut resp) }
            .unwrap()
            .unwrap();
        state.lock().unwrap().reset_delay = Some(2);
        blk.reset().unwrap();
        unsafe { blk.cancel_flush(token, &req, &mut resp) }.unwrap();
        assert_eq!(SHARES.with(Cell::get), 0);
        drop(blk);
        assert_eq!(DMA_LIVE.with(Cell::get), 0);
        assert_eq!(EARLY_RETIRE.with(Cell::get), 0);
    }
}

#[test]
fn final_failed_drop_retains_transport_and_ring() {
    let (blk, state, _config) = fixture(false);
    let live = DMA_LIVE.with(Cell::get);
    state.lock().unwrap().reset_delay = Some(usize::MAX);
    drop(blk);
    assert_eq!(state.lock().unwrap().reset_acks, 0);
    assert_eq!(state.lock().unwrap().transport_drops, 0);
    assert_eq!(DMA_LIVE.with(Cell::get), live);
    // The hardware-failure fallback intentionally retains these few ring pages.
    // No fake device thread exists, so no background DMA survives this test.
}

#[test]
fn drop_of_initialized_empty_device_waits_for_ack() {
    let (blk, state, _config) = fixture(false);
    state.lock().unwrap().reset_delay = Some(4);
    drop(blk); // Covers upper-layer preparation failure after hardware init.
    assert_eq!(DMA_LIVE.with(Cell::get), 0);
    assert_eq!(EARLY_RETIRE.with(Cell::get), 0);
    let state = state.lock().unwrap();
    assert_eq!(state.reset_acks, 1);
    assert_eq!(state.transport_drops, 1);
}
