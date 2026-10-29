use super::*;
use crate::{
    hal::fake::FakeHal,
    transport::{
        fake::{FakeTransport, State},
        DeviceStatus, DeviceType,
    },
};
use core::cell::Cell;
use std::sync::{Arc, Mutex};

std::thread_local! {
    static DMA: Cell<(usize, usize, usize)> = const { Cell::new((0, 0, 0)) };
}

struct TrackedHal;
unsafe impl Hal for TrackedHal {
    fn dma_alloc(_: usize, _: BufferDirection) -> (usize, NonNull<u8>) {
        panic!("use fallible DMA allocation")
    }
    fn try_dma_alloc(pages: usize, direction: BufferDirection) -> Result<(usize, NonNull<u8>)> {
        DMA.with(|state| {
            let (fail, attempts, live) = state.get();
            state.set((fail, attempts + 1, live));
            if fail == attempts + 1 {
                return Err(Error::DmaError);
            }
            let allocation = FakeHal::dma_alloc(pages, direction);
            state.set((fail, attempts + 1, live + 1));
            Ok(allocation)
        })
    }
    unsafe fn dma_dealloc(paddr: usize, vaddr: NonNull<u8>, pages: usize) -> i32 {
        DMA.with(|state| {
            let (fail, attempts, live) = state.get();
            state.set((fail, attempts, live.checked_sub(1).unwrap()));
        });
        unsafe { FakeHal::dma_dealloc(paddr, vaddr, pages) }
    }
    unsafe fn mmio_phys_to_virt(paddr: usize, size: usize) -> NonNull<u8> {
        unsafe { FakeHal::mmio_phys_to_virt(paddr, size) }
    }
    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection) -> usize {
        unsafe { FakeHal::share(buffer, direction) }
    }
    unsafe fn unshare(paddr: usize, buffer: NonNull<[u8]>, direction: BufferDirection) {
        unsafe { FakeHal::unshare(paddr, buffer, direction) }
    }
}

/// FakeTransport with selectable ring layout, without pretending host virtual
/// addresses fit the legacy MMIO PFN register. Device memory accesses still use
/// the real published descriptor/available/used addresses.
struct TestTransport {
    inner: FakeTransport<()>,
    legacy: bool,
}
impl Transport for TestTransport {
    fn device_type(&self) -> DeviceType {
        self.inner.device_type()
    }
    fn read_device_features(&mut self) -> u64 {
        self.inner.read_device_features()
    }
    fn write_driver_features(&mut self, features: u64) {
        self.inner.write_driver_features(features);
    }
    fn max_queue_size(&mut self, queue: u16) -> u32 {
        self.inner.max_queue_size(queue)
    }
    fn notify(&mut self, queue: u16) {
        self.inner.notify(queue);
    }
    fn get_status(&self) -> DeviceStatus {
        self.inner.get_status()
    }
    fn set_status(&mut self, status: DeviceStatus) {
        self.inner.set_status(status);
    }
    fn set_guest_page_size(&mut self, size: u32) {
        self.inner.set_guest_page_size(size);
    }
    fn requires_legacy_layout(&self) -> bool {
        self.legacy
    }
    fn queue_set(&mut self, queue: u16, size: u32, desc: usize, avail: usize, used: usize) {
        if self.legacy {
            assert_eq!(avail, desc + size as usize * size_of::<Descriptor>());
            assert_eq!(
                used,
                desc + align_up(size as usize * 16 + (size as usize + 3) * 2)
            );
        }
        self.inner.queue_set(queue, size, desc, avail, used);
    }
    fn queue_unset(&mut self, queue: u16) {
        self.inner.queue_unset(queue);
    }
    fn queue_used(&mut self, queue: u16) -> bool {
        self.inner.queue_used(queue)
    }
    fn ack_interrupt(&mut self) -> bool {
        self.inner.ack_interrupt()
    }
    fn config_space<C: 'static>(&self) -> Result<NonNull<C>> {
        self.inner.config_space()
    }
}

fn fixture<const SIZE: usize>(legacy: bool, fail_dma: usize) -> TestTransport {
    cpu_storage::tests::reset(0);
    DMA.with(|state| {
        assert_eq!(state.get().2, 0);
        state.set((fail_dma, 0, 0));
    });
    TestTransport {
        inner: FakeTransport {
            device_type: DeviceType::FileSystem,
            max_queue_size: SIZE as u32,
            device_features: 0,
            config_space: NonNull::dangling(),
            state: Arc::new(Mutex::new(State {
                queues: vec![Default::default()],
                ..Default::default()
            })),
        },
        legacy,
    }
}

fn create<const SIZE: usize>(
    transport: &mut TestTransport,
    boxed: bool,
    indirect: bool,
    event: bool,
) -> Result<Box<VirtQueue<TrackedHal, SIZE>>> {
    if boxed {
        VirtQueue::new_boxed(transport, 0, indirect, event)
    } else {
        VirtQueue::new(transport, 0, indirect, event).map(Box::new)
    }
}

fn failure_matrix<const SIZE: usize>() {
    for legacy in [false, true] {
        for boxed in [false, true] {
            for indirect in [false, true] {
                let mut transport = fixture::<SIZE>(legacy, 0);
                let queue = create::<SIZE>(&mut transport, boxed, indirect, true).unwrap();
                assert!(queue.dev_notify_enabled);
                let attempts = cpu_storage::tests::snapshot().1;
                assert!(cpu_storage::tests::snapshot().3 <= PAGE_SIZE);
                drop(queue);
                assert_eq!(cpu_storage::tests::snapshot().2, 0);
                assert_eq!(DMA.with(|state| state.get().2), 0);
                for fail in 1..=attempts {
                    let mut transport = fixture::<SIZE>(legacy, 0);
                    cpu_storage::tests::reset(fail);
                    assert!(matches!(
                        create::<SIZE>(&mut transport, boxed, indirect, true),
                        Err(Error::MemoryError)
                    ));
                    assert_eq!(
                        transport.inner.state.lock().unwrap().queues[0].descriptors,
                        0
                    );
                    assert_eq!(cpu_storage::tests::snapshot().2, 0);
                    assert_eq!(DMA.with(|state| state.get().2), 0);
                }
                for fail in 1..=if legacy { 1 } else { 3 } {
                    let mut transport = fixture::<SIZE>(legacy, fail);
                    assert!(matches!(
                        create::<SIZE>(&mut transport, boxed, indirect, false),
                        Err(Error::DmaError)
                    ));
                    assert_eq!(
                        transport.inner.state.lock().unwrap().queues[0].descriptors,
                        0
                    );
                    assert_eq!(cpu_storage::tests::snapshot().2, 0);
                    assert_eq!(DMA.with(|state| state.get().2), 0);
                }
            }
        }
    }
}

#[test]
fn all_large_queue_allocations_fail_transactionally_before_publication() {
    failure_matrix::<512>();
    failure_matrix::<1024>();
}

fn complete_across_pages<const SIZE: usize>() {
    for legacy in [false, true] {
        for boxed in [false, true] {
            for indirect in [false, true] {
                for event in [false, true] {
                    let mut transport = fixture::<SIZE>(legacy, 0);
                    let mut queue = create::<SIZE>(&mut transport, boxed, indirect, event).unwrap();
                    let payload = vec![31u8; SIZE * 2];
                    // One request per slot, including slots on every page.
                    for index in 0..SIZE {
                        let first = &payload[index * 2..index * 2 + 1];
                        let second = &payload[index * 2 + 1..index * 2 + 2];
                        let inputs = if indirect {
                            vec![first, second]
                        } else {
                            vec![first]
                        };
                        assert_eq!(
                            unsafe { queue.add(&inputs, &mut []) }.unwrap(),
                            index as u16
                        );
                    }
                    assert_eq!(queue.num_used as usize, SIZE);
                    assert!(queue.dev_notify_enabled);
                    for index in 0..SIZE {
                        if index == 1 {
                            queue.set_dev_notify(false);
                        }
                        if index == 2 {
                            queue.set_dev_notify(true);
                        }
                        let expected_event =
                            unsafe { (*queue.avail.as_ptr()).used_event.load(Ordering::Acquire) };
                        let produced = transport
                            .inner
                            .state
                            .lock()
                            .unwrap()
                            .read_from_queue::<SIZE>(0);
                        assert_eq!(produced, vec![31u8; if indirect { 2 } else { 1 }]);
                        let first = &payload[index * 2..index * 2 + 1];
                        let second = &payload[index * 2 + 1..index * 2 + 2];
                        let inputs = if indirect {
                            vec![first, second]
                        } else {
                            vec![first]
                        };
                        unsafe { queue.pop_used(index as u16, &inputs, &mut []) }.unwrap();
                        if event {
                            assert_eq!(
                                unsafe {
                                    (*queue.avail.as_ptr()).used_event.load(Ordering::Acquire)
                                },
                                if index == 1 {
                                    expected_event
                                } else {
                                    index as u16 + 1
                                }
                            );
                        }
                    }
                    assert_eq!(queue.num_used, 0);
                    assert!(queue.indirect_lists.iter().all(Option::is_none));
                    // Reset-style detachment also restores CPU owners at a page boundary.
                    let inputs = [&payload[..1]];
                    let token = unsafe { queue.add(&inputs, &mut []) }.unwrap();
                    transport.queue_unset(0);
                    unsafe { queue.detach_unused(token, &inputs, &mut []) }.unwrap();
                    drop(queue);
                    assert_eq!(cpu_storage::tests::snapshot().2, 0);
                    assert_eq!(DMA.with(|state| state.get().2), 0);
                }
            }
        }
    }
}

#[test]
fn large_queues_complete_across_pages_and_keep_notification_state() {
    assert!(size_of::<VirtQueue<TrackedHal, 1024>>() <= PAGE_SIZE);
    assert_eq!(
        size_of::<VirtQueue<TrackedHal, 1024>>(),
        size_of::<VirtQueue<TrackedHal, 8>>()
    );
    complete_across_pages::<512>();
    complete_across_pages::<1024>();
}
