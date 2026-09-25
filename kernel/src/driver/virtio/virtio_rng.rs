//! VirtIO entropy source for security-sensitive kernel random requests.

use alloc::{boxed::Box, sync::Arc};
use system_error::SystemError;
use virtio_drivers::queue::VirtQueue;
use virtio_drivers::transport::Transport;

use crate::driver::virtio::transport::VirtIOTransport;
use crate::driver::virtio::virtio_drivers_error_to_system_error;
use crate::driver::virtio::virtio_impl::HalImpl;
use crate::libs::mutex::Mutex;
use crate::libs::rand::{register_secure_entropy_source, SecureEntropySource};
use crate::time::sleep::nanosleep;
use crate::time::{Instant, PosixTimeSpec};

const QUEUE_SIZE: usize = 8;
const REQUEST_SIZE: usize = 64;
const WAIT_TIMEOUT_US: i64 = 1_000_000;
const POLL_INTERVAL_NS: i64 = 1_000_000;

bitflags2::bitflags! {
    #[derive(Debug)]
    struct RngFeatures: u64 {
        const NONE = 0;
    }
}

struct RngDevice {
    transport: VirtIOTransport,
    queue: VirtQueue<HalImpl, QUEUE_SIZE>,
    // The device may still write after an interrupted or timed-out request.
    // Keep this allocation alive and reuse the outstanding descriptor later.
    buffer: Box<[u8; REQUEST_SIZE]>,
    pending: Option<u16>,
    pending_deadline_us: Option<i64>,
    next: usize,
    available: usize,
    failed: bool,
}

// SAFETY: VirtIO queue and transport are accessed only while holding the
// VirtioEntropy mutex. The pending DMA buffer has a stable heap address.
unsafe impl Send for RngDevice {}

impl RngDevice {
    fn refill(&mut self) -> Result<(), SystemError> {
        if self.failed {
            return Err(SystemError::EIO);
        }

        if self.pending.is_none() {
            // SAFETY: buffer has a stable heap address and is neither read nor
            // freed before the matching completion is popped. On timeout the
            // descriptor remains pending, so the DMA target stays allocated.
            let token = unsafe { self.queue.add(&[], &mut [&mut self.buffer[..]]) }
                .map_err(virtio_drivers_error_to_system_error)?;
            self.pending = Some(token);
            self.pending_deadline_us = Some(
                Instant::now()
                    .total_micros()
                    .saturating_add(WAIT_TIMEOUT_US),
            );
            if self.queue.should_notify() {
                self.transport.notify(0);
            }
        }

        let deadline = self
            .pending_deadline_us
            .expect("virtio-rng pending deadline");
        while !self.queue.can_pop() {
            if Instant::now().total_micros() >= deadline {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            nanosleep(PosixTimeSpec::new(0, POLL_INTERVAL_NS))?;
        }

        let token = self.pending.expect("virtio-rng completion without request");
        // SAFETY: the buffer and descriptor are exactly those submitted above;
        // a used-ring entry now exists, so the device no longer owns the buffer.
        let result = unsafe { self.queue.pop_used(token, &[], &mut [&mut self.buffer[..]]) };
        self.pending = None;
        self.pending_deadline_us = None;
        let filled = match result {
            Ok(filled) if (1..=REQUEST_SIZE as u32).contains(&filled) => filled as usize,
            _ => {
                self.failed = true;
                return Err(SystemError::EIO);
            }
        };
        self.next = 0;
        self.available = filled;
        Ok(())
    }
}

struct VirtioEntropy {
    device: Mutex<RngDevice>,
}

impl SecureEntropySource for VirtioEntropy {
    fn fill_bytes(&self, output: &mut [u8]) -> Result<(), SystemError> {
        let mut device = self.device.lock();
        let mut completed = 0;
        while completed < output.len() {
            if device.next == device.available {
                device.refill()?;
            }
            let count = (output.len() - completed).min(device.available - device.next);
            let start = device.next;
            output[completed..completed + count]
                .copy_from_slice(&device.buffer[start..start + count]);
            device.buffer[start..start + count].fill(0);
            device.next += count;
            completed += count;
        }
        Ok(())
    }
}

pub(super) fn virtio_rng(mut transport: VirtIOTransport) {
    transport.begin_init(RngFeatures::empty());
    let queue = match VirtQueue::<HalImpl, QUEUE_SIZE>::new(&mut transport, 0, false, false) {
        Ok(queue) => queue,
        Err(error) => {
            log::warn!("virtio-rng queue initialization failed: {error}");
            return;
        }
    };
    transport.finish_init();

    let source = Arc::new(VirtioEntropy {
        device: Mutex::new(RngDevice {
            transport,
            queue,
            buffer: Box::new([0; REQUEST_SIZE]),
            pending: None,
            pending_deadline_us: None,
            next: 0,
            available: 0,
            failed: false,
        }),
    });
    if let Err(error) = register_secure_entropy_source(source) {
        log::warn!("virtio-rng registration failed: {error:?}");
    }
}
