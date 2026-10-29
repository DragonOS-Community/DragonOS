//! Driver for VirtIO input devices.

use super::common::Feature;
use crate::hal::Hal;
use crate::queue::VirtQueue;
use crate::transport::Transport;
use crate::volatile::{volread, volwrite, ReadOnly, WriteOnly};
use crate::Result;
use alloc::boxed::Box;
use core::ptr::NonNull;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

/// Virtual human interface devices such as keyboards, mice and tablets.
///
/// An instance of the virtio device represents one such input device.
/// Device behavior mirrors that of the evdev layer in Linux,
/// making pass-through implementations on top of evdev easy.
pub struct VirtIOInput<H: Hal, T: Transport> {
    transport: T,
    event_queue: VirtQueue<H, QUEUE_SIZE>,
    status_queue: VirtQueue<H, QUEUE_SIZE>,
    event_buf: Box<[InputEvent; 32]>,
    config: NonNull<Config>,
}

impl<H: Hal, T: Transport> VirtIOInput<H, T> {
    /// Create a new VirtIO-Input driver.
    pub fn new(mut transport: T) -> Result<Self> {
        let mut event_buf = Box::new([InputEvent::default(); QUEUE_SIZE]);

        let negotiated_features = transport.begin_init(SUPPORTED_FEATURES);

        let config = transport.config_space::<Config>()?;

        let mut event_queue = VirtQueue::new(
            &mut transport,
            QUEUE_EVENT,
            false,
            negotiated_features.contains(Feature::RING_EVENT_IDX),
        )?;
        let status_queue = VirtQueue::new(
            &mut transport,
            QUEUE_STATUS,
            false,
            negotiated_features.contains(Feature::RING_EVENT_IDX),
        )?;
        for (i, event) in event_buf.as_mut().iter_mut().enumerate() {
            // Safe because the buffer lasts as long as the queue.
            let token = unsafe { event_queue.add(&[], &mut [event.as_bytes_mut()])? };
            assert_eq!(token, i as u16);
        }
        if event_queue.should_notify() {
            transport.notify(QUEUE_EVENT);
        }

        transport.finish_init();

        Ok(VirtIOInput {
            transport,
            event_queue,
            status_queue,
            event_buf,
            config,
        })
    }

    /// Acknowledge interrupt and process events.
    pub fn ack_interrupt(&mut self) -> bool {
        self.transport.ack_interrupt()
    }

    /// Pop the pending event.
    pub fn pop_pending_event(&mut self) -> Option<InputEvent> {
        if let Some(token) = self.event_queue.peek_used() {
            let event = &mut self.event_buf[token as usize];
            // Safe because we are passing the same buffer as we passed to `VirtQueue::add` and it
            // is still valid.
            unsafe {
                self.event_queue
                    .pop_used(token, &[], &mut [event.as_bytes_mut()])
                    .ok()?;
            }
            let event_saved = *event;
            // requeue
            // Safe because buffer lasts as long as the queue.
            if let Ok(new_token) = unsafe { self.event_queue.add(&[], &mut [event.as_bytes_mut()]) }
            {
                // This only works because nothing happen between `pop_used` and `add` that affects
                // the list of free descriptors in the queue, so `add` reuses the descriptor which
                // was just freed by `pop_used`.
                assert_eq!(new_token, token);
                if self.event_queue.should_notify() {
                    self.transport.notify(QUEUE_EVENT);
                }
                return Some(event_saved);
            }
        }
        None
    }

    /// Query a specific piece of information by `select` and `subsel`, and write
    /// result to `out`, return the result size.
    pub fn query_config_select(
        &mut self,
        select: InputConfigSelect,
        subsel: u8,
        out: &mut [u8],
    ) -> u8 {
        let size;
        let data;
        // Safe because config points to a valid MMIO region for the config space.
        unsafe {
            volwrite!(self.config, select, select as u8);
            volwrite!(self.config, subsel, subsel);
            size = volread!(self.config, size);
            data = volread!(self.config, data);
        }
        out[..size as usize].copy_from_slice(&data[..size as usize]);
        size
    }
}

impl<H: Hal, T: Transport> Drop for VirtIOInput<H, T> {
    fn drop(&mut self) {
        // Clear any pointers pointing to DMA regions, so the device doesn't try to access them
        // after they have been freed.
        self.transport.queue_unset(QUEUE_EVENT);
        self.transport.queue_unset(QUEUE_STATUS);
    }
}

/// Select value used for [`VirtIOInput::query_config_select()`].
#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub enum InputConfigSelect {
    /// Returns the name of the device, in u.string. subsel is zero.
    IdName = 0x01,
    /// Returns the serial number of the device, in u.string. subsel is zero.
    IdSerial = 0x02,
    /// Returns ID information of the device, in u.ids. subsel is zero.
    IdDevids = 0x03,
    /// Returns input properties of the device, in u.bitmap. subsel is zero.
    /// Individual bits in the bitmap correspond to INPUT_PROP_* constants used
    /// by the underlying evdev implementation.
    PropBits = 0x10,
    /// subsel specifies the event type using EV_* constants in the underlying
    /// evdev implementation. If size is non-zero the event type is supported
    /// and a bitmap of supported event codes is returned in u.bitmap. Individual
    /// bits in the bitmap correspond to implementation-defined input event codes,
    /// for example keys or pointing device axes.
    EvBits = 0x11,
    /// subsel specifies the absolute axis using ABS_* constants in the underlying
    /// evdev implementation. Information about the axis will be returned in u.abs.
    AbsInfo = 0x12,
}

#[repr(C)]
struct Config {
    select: WriteOnly<u8>,
    subsel: WriteOnly<u8>,
    size: ReadOnly<u8>,
    _reversed: [ReadOnly<u8>; 5],
    data: ReadOnly<[u8; 128]>,
}

#[repr(C)]
#[derive(Debug)]
struct AbsInfo {
    min: u32,
    max: u32,
    fuzz: u32,
    flat: u32,
    res: u32,
}

#[repr(C)]
#[derive(Debug)]
struct DevIDs {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

/// Both queues use the same `virtio_input_event` struct. `type`, `code` and `value`
/// are filled according to the Linux input layer (evdev) interface.
#[repr(C)]
#[derive(AsBytes, Clone, Copy, Debug, Default, FromBytes, FromZeroes)]
pub struct InputEvent {
    /// Event type.
    pub event_type: u16,
    /// Event code.
    pub code: u16,
    /// Event value.
    pub value: u32,
}

const QUEUE_EVENT: u16 = 0;
const QUEUE_STATUS: u16 = 1;
const SUPPORTED_FEATURES: Feature = Feature::RING_EVENT_IDX;

// a parameter that can change
const QUEUE_SIZE: usize = 32;
