use alloc::boxed::Box;
use core::hint::spin_loop;

use system_error::SystemError;
use virtio_drivers::{
    queue::VirtQueue, transport::Transport, Error as VirtioError, Result as VirtioResult,
};

use crate::driver::virtio::{
    transport::VirtIOTransport, virtio_drivers_error_to_system_error, virtio_impl::HalImpl,
};

const VIRTIOFS_QUEUE_SIZE_MIN: usize = 8;
const VIRTIOFS_QUEUE_SIZE_MAX: usize = 1024;
const VIRTIOFS_RESET_WAIT_SPINS: usize = 100_000;

pub(super) enum VirtioFsQueue {
    Q8(Box<VirtQueue<HalImpl, 8>>),
    Q16(Box<VirtQueue<HalImpl, 16>>),
    Q32(Box<VirtQueue<HalImpl, 32>>),
    Q64(Box<VirtQueue<HalImpl, 64>>),
    Q128(Box<VirtQueue<HalImpl, 128>>),
    Q256(Box<VirtQueue<HalImpl, 256>>),
    Q512(Box<VirtQueue<HalImpl, 512>>),
    Q1024(Box<VirtQueue<HalImpl, 1024>>),
}

impl VirtioFsQueue {
    pub(super) fn can_pop(&self) -> bool {
        match self {
            Self::Q8(q) => q.can_pop(),
            Self::Q16(q) => q.can_pop(),
            Self::Q32(q) => q.can_pop(),
            Self::Q64(q) => q.can_pop(),
            Self::Q128(q) => q.can_pop(),
            Self::Q256(q) => q.can_pop(),
            Self::Q512(q) => q.can_pop(),
            Self::Q1024(q) => q.can_pop(),
        }
    }

    pub(super) fn peek_used(&self) -> Option<u16> {
        match self {
            Self::Q8(q) => q.peek_used(),
            Self::Q16(q) => q.peek_used(),
            Self::Q32(q) => q.peek_used(),
            Self::Q64(q) => q.peek_used(),
            Self::Q128(q) => q.peek_used(),
            Self::Q256(q) => q.peek_used(),
            Self::Q512(q) => q.peek_used(),
            Self::Q1024(q) => q.peek_used(),
        }
    }

    pub(super) fn should_notify(&self) -> bool {
        match self {
            Self::Q8(q) => q.should_notify(),
            Self::Q16(q) => q.should_notify(),
            Self::Q32(q) => q.should_notify(),
            Self::Q64(q) => q.should_notify(),
            Self::Q128(q) => q.should_notify(),
            Self::Q256(q) => q.should_notify(),
            Self::Q512(q) => q.should_notify(),
            Self::Q1024(q) => q.should_notify(),
        }
    }

    /// # Safety
    ///
    /// Every input and output buffer must be non-empty. If this call succeeds, the caller must keep
    /// every buffer valid at a fixed address and must not access its contents until `pop_used` with
    /// the returned token succeeds, or until the device has completed reset and `detach_unused`
    /// with the returned token succeeds.
    pub(super) unsafe fn add<'a, 'b>(
        &mut self,
        inputs: &'a [&'b [u8]],
        outputs: &'a mut [&'b mut [u8]],
    ) -> VirtioResult<u16> {
        match self {
            Self::Q8(q) => unsafe { q.add(inputs, outputs) },
            Self::Q16(q) => unsafe { q.add(inputs, outputs) },
            Self::Q32(q) => unsafe { q.add(inputs, outputs) },
            Self::Q64(q) => unsafe { q.add(inputs, outputs) },
            Self::Q128(q) => unsafe { q.add(inputs, outputs) },
            Self::Q256(q) => unsafe { q.add(inputs, outputs) },
            Self::Q512(q) => unsafe { q.add(inputs, outputs) },
            Self::Q1024(q) => unsafe { q.add(inputs, outputs) },
        }
    }

    /// # Safety
    ///
    /// `token`, `inputs`, and `outputs` must identify the same still-valid buffers originally
    /// passed to `add` when it returned `token`. The caller must retain them without accessing
    /// their contents until this operation succeeds.
    pub(super) unsafe fn pop_used<'a>(
        &mut self,
        token: u16,
        inputs: &'a [&'a [u8]],
        outputs: &'a mut [&'a mut [u8]],
    ) -> VirtioResult<u32> {
        match self {
            Self::Q8(q) => unsafe { q.pop_used(token, inputs, outputs) },
            Self::Q16(q) => unsafe { q.pop_used(token, inputs, outputs) },
            Self::Q32(q) => unsafe { q.pop_used(token, inputs, outputs) },
            Self::Q64(q) => unsafe { q.pop_used(token, inputs, outputs) },
            Self::Q128(q) => unsafe { q.pop_used(token, inputs, outputs) },
            Self::Q256(q) => unsafe { q.pop_used(token, inputs, outputs) },
            Self::Q512(q) => unsafe { q.pop_used(token, inputs, outputs) },
            Self::Q1024(q) => unsafe { q.pop_used(token, inputs, outputs) },
        }
    }

    /// # Safety
    ///
    /// The device must have completed reset and stopped accessing this queue. `token`, `inputs`,
    /// and `outputs` must identify the same still-valid buffers originally passed to `add` when it
    /// returned `token`; the caller must retain them until this operation succeeds.
    pub(super) unsafe fn detach_unused<'a>(
        &mut self,
        token: u16,
        inputs: &'a [&'a [u8]],
        outputs: &'a mut [&'a mut [u8]],
    ) -> VirtioResult<()> {
        match self {
            Self::Q8(q) => unsafe { q.detach_unused(token, inputs, outputs) },
            Self::Q16(q) => unsafe { q.detach_unused(token, inputs, outputs) },
            Self::Q32(q) => unsafe { q.detach_unused(token, inputs, outputs) },
            Self::Q64(q) => unsafe { q.detach_unused(token, inputs, outputs) },
            Self::Q128(q) => unsafe { q.detach_unused(token, inputs, outputs) },
            Self::Q256(q) => unsafe { q.detach_unused(token, inputs, outputs) },
            Self::Q512(q) => unsafe { q.detach_unused(token, inputs, outputs) },
            Self::Q1024(q) => unsafe { q.detach_unused(token, inputs, outputs) },
        }
    }
}

fn choose_queue_size(device_max: u32) -> Result<usize, SystemError> {
    let limit = core::cmp::min(device_max as usize, VIRTIOFS_QUEUE_SIZE_MAX);
    if limit < VIRTIOFS_QUEUE_SIZE_MIN {
        return Err(SystemError::EINVAL);
    }

    let mut size = VIRTIOFS_QUEUE_SIZE_MIN;
    while size <= limit / 2 {
        size *= 2;
    }
    Ok(size)
}

fn create_queue_with_size(
    transport: &mut VirtIOTransport,
    idx: u16,
    size: usize,
) -> Result<VirtioFsQueue, VirtioError> {
    macro_rules! new_queue {
        ($size:expr, $variant:ident) => {{
            let mut queue = VirtQueue::<HalImpl, $size>::new_boxed(transport, idx, false, false)?;
            queue.set_dev_notify(true);
            Ok(VirtioFsQueue::$variant(queue))
        }};
    }

    match size {
        8 => new_queue!(8, Q8),
        16 => new_queue!(16, Q16),
        32 => new_queue!(32, Q32),
        64 => new_queue!(64, Q64),
        128 => new_queue!(128, Q128),
        256 => new_queue!(256, Q256),
        512 => new_queue!(512, Q512),
        1024 => new_queue!(1024, Q1024),
        _ => Err(VirtioError::InvalidParam),
    }
}

pub(super) fn create_queue(
    transport: &mut VirtIOTransport,
    idx: u16,
) -> Result<(VirtioFsQueue, usize, usize), SystemError> {
    let device_max = transport.max_queue_size(idx) as usize;
    let size = choose_queue_size(device_max as u32)?;
    let queue = create_queue_with_size(transport, idx, size)
        .map_err(virtio_drivers_error_to_system_error)?;
    Ok((queue, device_max, size))
}

pub(super) fn wait_transport_reset_complete(transport: &VirtIOTransport) -> bool {
    for _ in 0..VIRTIOFS_RESET_WAIT_SPINS {
        if transport.get_status().is_empty() {
            return true;
        }
        spin_loop();
    }
    false
}

#[cfg(test)]
mod tests {
    use system_error::SystemError;

    use super::choose_queue_size;

    #[test]
    fn choose_queue_size_uses_supported_power_of_two() {
        assert_eq!(choose_queue_size(8).unwrap(), 8);
        assert_eq!(choose_queue_size(15).unwrap(), 8);
        assert_eq!(choose_queue_size(128).unwrap(), 128);
        assert_eq!(choose_queue_size(2048).unwrap(), 1024);
        assert_eq!(choose_queue_size(7), Err(SystemError::EINVAL));
    }
}
