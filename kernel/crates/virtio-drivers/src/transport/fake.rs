use super::{DeviceStatus, DeviceType, Transport};
use crate::{
    queue::{fake_read_write_queue, Descriptor},
    PhysAddr, Result,
};
use alloc::{sync::Arc, vec::Vec};
use core::{
    any::TypeId,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use std::{sync::Mutex, thread};

/// A fake implementation of [`Transport`] for unit tests.
#[derive(Debug)]
pub struct FakeTransport<C: 'static> {
    /// Device type reported to the driver.
    pub device_type: DeviceType,
    /// Maximum descriptor count reported for every queue.
    pub max_queue_size: u32,
    /// Feature bits offered by the simulated device.
    pub device_features: u64,
    /// Borrowed configuration backing, which must outlive the transport.
    pub config_space: NonNull<C>,
    /// Mutable device state shared with the simulated device thread.
    pub state: Arc<Mutex<State>>,
}

impl<C> Transport for FakeTransport<C> {
    fn device_type(&self) -> DeviceType {
        self.device_type
    }

    fn read_device_features(&mut self) -> u64 {
        self.device_features
    }

    fn write_driver_features(&mut self, driver_features: u64) {
        self.state.lock().unwrap().driver_features = driver_features;
    }

    fn max_queue_size(&mut self, _queue: u16) -> u32 {
        self.max_queue_size
    }

    fn notify(&mut self, queue: u16) {
        self.state.lock().unwrap().queues[queue as usize]
            .notified
            .store(true, Ordering::SeqCst);
    }

    fn get_status(&self) -> DeviceStatus {
        let mut state = self.state.lock().unwrap();
        if state.reset_requested {
            if state.reset_delay == Some(0) {
                state.status = DeviceStatus::empty();
                state.reset_requested = false;
                state.reset_acks += 1;
            } else if let Some(left) = state.reset_delay.as_mut() {
                *left -= 1;
            }
        }
        state.status
    }

    fn set_status(&mut self, status: DeviceStatus) {
        let mut state = self.state.lock().unwrap();
        if status.is_empty() {
            state.reset_zero_writes += 1;
        }
        if !status.is_empty() && state.reset_requested {
            state.initialization_before_reset_ack += 1;
        }
        if status.is_empty() && !state.status.is_empty() {
            state.reset_requests += 1;
            if state.reset_delay.is_some() {
                state.reset_requested = true;
                return;
            }
            state.reset_acks += 1;
        }
        state.status = status;
    }

    fn set_guest_page_size(&mut self, guest_page_size: u32) {
        self.state.lock().unwrap().guest_page_size = guest_page_size;
    }

    fn requires_legacy_layout(&self) -> bool {
        false
    }

    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: PhysAddr,
        driver_area: PhysAddr,
        device_area: PhysAddr,
    ) {
        let mut state = self.state.lock().unwrap();
        state.queue_sets += 1;
        if state.reset_requested {
            state.initialization_before_reset_ack += 1;
        }
        state.queues[queue as usize].size = size;
        state.queues[queue as usize].descriptors = descriptors;
        state.queues[queue as usize].driver_area = driver_area;
        state.queues[queue as usize].device_area = device_area;
    }

    fn queue_unset(&mut self, queue: u16) {
        let mut state = self.state.lock().unwrap();
        state.queues[queue as usize].size = 0;
        state.queues[queue as usize].descriptors = 0;
        state.queues[queue as usize].driver_area = 0;
        state.queues[queue as usize].device_area = 0;
    }

    fn queue_used(&mut self, queue: u16) -> bool {
        self.state.lock().unwrap().queues[queue as usize].descriptors != 0
    }

    fn ack_interrupt(&mut self) -> bool {
        let mut state = self.state.lock().unwrap();
        let pending = state.interrupt_pending;
        if pending {
            state.interrupt_pending = false;
        }
        pending
    }

    fn config_space<T: 'static>(&self) -> Result<NonNull<T>> {
        let mut state = self.state.lock().unwrap();
        state.config_reads += 1;
        if let Some(error) = state.config_error {
            return Err(error);
        }
        drop(state);
        if TypeId::of::<T>() == TypeId::of::<C>() {
            Ok(self.config_space.cast())
        } else {
            panic!("Unexpected config space type.");
        }
    }
}

impl<C: 'static> Drop for FakeTransport<C> {
    fn drop(&mut self) {
        self.state.lock().unwrap().transport_drops += 1;
    }
}

/// Device state observed and updated by the driver and simulated device.
#[derive(Debug, Default)]
pub struct State {
    /// Most recently written device status.
    pub status: DeviceStatus,
    /// Optional number of reads before a requested reset is acknowledged.
    pub reset_delay: Option<usize>,
    /// A pending reset, independently of the queue's register addresses.
    pub reset_requested: bool,
    /// Number of reset writes and completed reset handshakes.
    pub reset_requests: usize,
    /// Number of reset acknowledgements.
    pub reset_acks: usize,
    /// Number of transport owners destroyed.
    pub transport_drops: usize,
    /// All writes of zero status, including redundant writes after reset.
    pub reset_zero_writes: usize,
    /// Status or queue initialization attempted while reset was still pending.
    pub initialization_before_reset_ack: usize,
    /// Number of queue publications.
    pub queue_sets: usize,
    /// Optional configuration error for constructor cleanup tests.
    pub config_error: Option<crate::Error>,
    /// Number of device configuration accesses.
    pub config_reads: usize,
    /// Feature bits selected by the driver.
    pub driver_features: u64,
    /// Guest page size written by the driver, in bytes.
    pub guest_page_size: u32,
    /// Pending interrupt indication consumed by interrupt acknowledgement.
    pub interrupt_pending: bool,
    /// Queue state indexed by the device's queue number.
    pub queues: Vec<QueueStatus>,
}

impl State {
    /// Simulates the device writing to the given queue.
    ///
    /// The fake device always uses descriptors in order.
    pub fn write_to_queue<const QUEUE_SIZE: usize>(&mut self, queue_index: u16, data: &[u8]) {
        let queue = &self.queues[queue_index as usize];
        assert_ne!(queue.descriptors, 0);
        fake_read_write_queue(
            queue.descriptors as *const [Descriptor; QUEUE_SIZE],
            queue.driver_area as *const u8,
            queue.device_area as *mut u8,
            |input| {
                assert_eq!(input, Vec::new());
                data.to_owned()
            },
        );
    }

    /// Simulates the device reading from the given queue.
    ///
    /// Data is read into the `data` buffer passed in. Returns the number of bytes actually read.
    ///
    /// The fake device always uses descriptors in order.
    pub fn read_from_queue<const QUEUE_SIZE: usize>(&mut self, queue_index: u16) -> Vec<u8> {
        let queue = &self.queues[queue_index as usize];
        assert_ne!(queue.descriptors, 0);

        let mut ret = None;

        // Read data from the queue but don't write any response.
        fake_read_write_queue(
            queue.descriptors as *const [Descriptor; QUEUE_SIZE],
            queue.driver_area as *const u8,
            queue.device_area as *mut u8,
            |input| {
                ret = Some(input);
                Vec::new()
            },
        );

        ret.unwrap()
    }

    /// Simulates the device reading data from the given queue and then writing a response back.
    ///
    /// The fake device always uses descriptors in order.
    pub fn read_write_queue<const QUEUE_SIZE: usize>(
        &mut self,
        queue_index: u16,
        handler: impl FnOnce(Vec<u8>) -> Vec<u8>,
    ) {
        let queue = &self.queues[queue_index as usize];
        assert_ne!(queue.descriptors, 0);
        fake_read_write_queue(
            queue.descriptors as *const [Descriptor; QUEUE_SIZE],
            queue.driver_area as *const u8,
            queue.device_area as *mut u8,
            handler,
        )
    }

    /// Waits until the given queue is notified.
    pub fn wait_until_queue_notified(state: &Mutex<Self>, queue_index: u16) {
        while !state.lock().unwrap().queues[usize::from(queue_index)]
            .notified
            .swap(false, Ordering::SeqCst)
        {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Driver-programmed addresses and notification state of one split queue.
#[derive(Debug, Default)]
pub struct QueueStatus {
    /// Configured descriptor count, or zero after the queue is unset.
    pub size: u32,
    /// Descriptor table address, or zero when the queue is unused.
    pub descriptors: PhysAddr,
    /// Driver-owned available ring address.
    pub driver_area: PhysAddr,
    /// Device-owned used ring address.
    pub device_area: PhysAddr,
    /// Pending driver notification consumed by the simulated device waiter.
    pub notified: AtomicBool,
}
