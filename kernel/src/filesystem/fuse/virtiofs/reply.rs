use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

use device_output_buffer::{BufferError, DeviceOutputBuffer, RetentionCredits, RetentionLimits};
use system_error::SystemError;

use crate::libs::spinlock::SpinLock;

use super::super::{conn::FuseConn, stats};
use super::VIRTIOFS_RSP_BUF_SIZE;

const FREE_MAX_BUFFERS: usize = 16;
const FREE_MAX_CAPACITY_BYTES: usize = VIRTIOFS_RSP_BUF_SIZE * FREE_MAX_BUFFERS;
const RETAINED_MAX_BUFFERS: usize = 16;
const RETAINED_MAX_CAPACITY_BYTES: usize = VIRTIOFS_RSP_BUF_SIZE * RETAINED_MAX_BUFFERS;

#[derive(Debug)]
struct ReplyReleaseGate {
    active: AtomicBool,
    credit_waiting: AtomicBool,
    conn: Weak<FuseConn>,
}

impl ReplyReleaseGate {
    fn new(conn: &Arc<FuseConn>) -> Self {
        Self {
            active: AtomicBool::new(true),
            credit_waiting: AtomicBool::new(false),
            conn: Arc::downgrade(conn),
        }
    }

    fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
    }

    fn mark_credit_waiting(&self) {
        self.credit_waiting.store(true, Ordering::Release);
    }

    fn clear_credit_waiting(&self) {
        self.credit_waiting.store(false, Ordering::Release);
    }

    fn signal(&self) {
        if self.active.load(Ordering::Acquire) && self.credit_waiting.load(Ordering::Acquire) {
            if let Some(conn) = self.conn.upgrade() {
                conn.wake_bridge(stats::VirtioFsBridgeWakeSource::ReplyReleased);
            }
        }
    }
}

#[derive(Debug)]
struct PoolInner {
    buffers: Vec<DeviceOutputBuffer>,
    free_capacity_bytes: usize,
    credits: RetentionCredits,
    accepting_returns: bool,
}

impl PoolInner {
    fn new() -> Self {
        Self {
            buffers: Vec::with_capacity(FREE_MAX_BUFFERS),
            free_capacity_bytes: 0,
            credits: RetentionCredits::new(RetentionLimits {
                max_count: RETAINED_MAX_BUFFERS,
                max_capacity_bytes: RETAINED_MAX_CAPACITY_BYTES,
            }),
            accepting_returns: true,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ResponseBufferPool {
    inner: Arc<SpinLock<PoolInner>>,
    gate: Arc<ReplyReleaseGate>,
}

impl ResponseBufferPool {
    pub(crate) fn new(conn: &Arc<FuseConn>) -> Self {
        Self {
            inner: Arc::new(SpinLock::new(PoolInner::new())),
            gate: Arc::new(ReplyReleaseGate::new(conn)),
        }
    }

    pub(crate) fn acquire<F>(
        &self,
        opcode: u32,
        len: usize,
        virt_to_phys: F,
    ) -> Result<DeviceOutputBuffer, SystemError>
    where
        F: FnMut(usize) -> Option<usize> + Copy,
    {
        let candidate = {
            let mut inner = self.inner.lock();
            let best = inner
                .buffers
                .iter()
                .enumerate()
                .filter(|(_, buf)| buf.allocation_len() >= len)
                .min_by_key(|(_, buf)| buf.allocation_len())
                .map(|(index, _)| index);
            best.map(|index| {
                let buf = inner.buffers.swap_remove(index);
                inner.free_capacity_bytes -= buf.allocation_len();
                buf
            })
        };

        if let Some(mut buf) = candidate {
            if buf.prepare(len, virt_to_phys).is_err() {
                stats::on_virtiofs_response_pool_drop();
                return Err(SystemError::EIO);
            }
            stats::on_virtiofs_response_buffer_reuse(opcode, len);
            return Ok(buf);
        }

        let buf = DeviceOutputBuffer::new(len, virt_to_phys).map_err(|_| SystemError::EIO)?;
        stats::on_virtiofs_response_buffer_alloc(opcode, len);
        stats::on_virtiofs_response_buffer_zero(opcode, len);
        Ok(buf)
    }

    pub(crate) fn release(&self, mut buf: DeviceOutputBuffer, reusable: bool) -> bool {
        if !reusable || buf.recycle().is_err() {
            stats::on_virtiofs_response_pool_drop();
            return false;
        }
        let capacity = buf.allocation_len();
        let mut owner = Some(buf);
        let retained = {
            let mut inner = self.inner.lock();
            let next = inner.free_capacity_bytes.checked_add(capacity);
            if inner.accepting_returns
                && capacity <= VIRTIOFS_RSP_BUF_SIZE
                && inner.buffers.len() < FREE_MAX_BUFFERS
                && next.is_some_and(|v| v <= FREE_MAX_CAPACITY_BYTES)
            {
                inner.free_capacity_bytes = next.expect("checked above");
                inner.buffers.push(owner.take().expect("owner is present"));
                true
            } else {
                false
            }
        };
        drop(owner);
        if !retained {
            stats::on_virtiofs_response_pool_drop();
        }
        retained
    }

    pub(crate) fn reserve(&self, capacity: usize) -> Option<RetentionReservation> {
        let reserved = {
            let mut inner = self.inner.lock();
            let reserved = inner.credits.try_reserve(capacity);
            if !reserved {
                // Publish the waiter while holding the same lock used by release. A release is
                // therefore either visible to this reservation attempt or observes the waiter;
                // there is no mark-after-release lost-wakeup window.
                self.gate.mark_credit_waiting();
            }
            reserved
        };
        reserved.then(|| {
            stats::on_virtiofs_reply_retained(capacity);
            RetentionReservation {
                inner: self.inner.clone(),
                gate: Arc::downgrade(&self.gate),
                capacity,
                active: true,
            }
        })
    }

    pub(crate) fn clear_credit_waiting(&self) {
        self.gate.clear_credit_waiting();
    }

    pub(crate) fn close(&self) {
        self.gate.deactivate();
        let buffers = {
            let mut inner = self.inner.lock();
            inner.accepting_returns = false;
            inner.free_capacity_bytes = 0;
            core::mem::take(&mut inner.buffers)
        };
        drop(buffers);
    }
}

impl Drop for ResponseBufferPool {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Debug)]
pub(crate) struct RetentionReservation {
    inner: Arc<SpinLock<PoolInner>>,
    gate: Weak<ReplyReleaseGate>,
    capacity: usize,
    active: bool,
}

impl RetentionReservation {
    fn reaccount_capacity(&mut self, capacity: usize) -> bool {
        let old_capacity = self.capacity;
        if old_capacity == capacity {
            return true;
        }
        let resized = self.inner.lock().credits.resize(old_capacity, capacity);
        if !resized {
            return false;
        }
        self.capacity = capacity;
        stats::on_virtiofs_reply_capacity_reaccounted(old_capacity, capacity);
        if capacity < old_capacity {
            if let Some(gate) = self.gate.upgrade() {
                gate.signal();
            }
        }
        true
    }
}

impl Drop for RetentionReservation {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let released = self.inner.lock().credits.release(self.capacity);
        debug_assert!(released, "retention credit accounting mismatch");
        self.active = false;
        if released {
            stats::on_virtiofs_reply_released(self.capacity);
        }
        if let Some(gate) = self.gate.upgrade() {
            gate.signal();
        }
    }
}

#[derive(Debug)]
pub(crate) enum VirtioFsReplyStorage {
    Device(DeviceReplyLease),
    CompatBytes {
        bytes: Vec<u8>,
        _reservation: RetentionReservation,
    },
}

#[derive(Debug)]
pub(crate) struct DeviceReplyLease {
    buffer: Option<DeviceOutputBuffer>,
    reservation: Option<RetentionReservation>,
    pool: Arc<SpinLock<PoolInner>>,
    range: Range<usize>,
}

impl VirtioFsReplyStorage {
    pub(crate) fn is_device(&self) -> bool {
        matches!(self, Self::Device(_))
    }

    pub(crate) fn from_completed(
        buffer: DeviceOutputBuffer,
        reservation: RetentionReservation,
        range: Range<usize>,
    ) -> Result<Self, BufferError> {
        let initialized_len = match buffer.initialized_prefix() {
            Ok(bytes) => bytes.len(),
            Err(err) => return Err(err),
        };
        if range.start > range.end || range.end > initialized_len {
            return Err(BufferError::InvalidLength);
        }
        let pool = reservation.inner.clone();
        Ok(Self::Device(DeviceReplyLease {
            buffer: Some(buffer),
            reservation: Some(reservation),
            pool,
            range,
        }))
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            Self::Device(lease) => &lease
                .buffer
                .as_ref()
                .expect("device reply owns its buffer")
                .initialized_prefix()
                .expect("device reply was created from Completed buffer")[lease.range.clone()],
            Self::CompatBytes { bytes, .. } => bytes,
        }
    }

    pub(crate) fn into_compat_bytes(self, bytes: Vec<u8>) -> Result<Self, SystemError> {
        match self {
            Self::Device(mut lease) => {
                let mut reservation = lease
                    .reservation
                    .take()
                    .expect("device reply owns its retention reservation");
                if !reservation.reaccount_capacity(bytes.capacity()) {
                    return Err(SystemError::EIO);
                }
                if let Some(mut buf) = lease.buffer.take() {
                    if buf.recycle().is_ok() {
                        return_buffer_to_pool(&lease.pool, buf);
                    } else {
                        stats::on_virtiofs_response_pool_drop();
                    }
                }
                Ok(Self::CompatBytes {
                    bytes,
                    _reservation: reservation,
                })
            }
            Self::CompatBytes { .. } => Err(SystemError::EINVAL),
        }
    }
}

fn return_buffer_to_pool(pool: &Arc<SpinLock<PoolInner>>, buf: DeviceOutputBuffer) {
    let capacity = buf.allocation_len();
    let mut owner = Some(buf);
    {
        let mut inner = pool.lock();
        let next = inner.free_capacity_bytes.checked_add(capacity);
        if inner.accepting_returns
            && capacity <= VIRTIOFS_RSP_BUF_SIZE
            && inner.buffers.len() < FREE_MAX_BUFFERS
            && next.is_some_and(|v| v <= FREE_MAX_CAPACITY_BYTES)
        {
            inner.free_capacity_bytes = next.expect("checked above");
            inner.buffers.push(owner.take().expect("owner present"));
        }
    }
    let retained = owner.is_none();
    drop(owner);
    if !retained {
        stats::on_virtiofs_response_pool_drop();
    }
}

impl Drop for DeviceReplyLease {
    fn drop(&mut self) {
        if let Some(mut buf) = self.buffer.take() {
            if buf.recycle().is_ok() {
                return_buffer_to_pool(&self.pool, buf);
            } else {
                stats::on_virtiofs_response_pool_drop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use device_output_buffer::DeviceOutputBuffer;

    use super::{ResponseBufferPool, VirtioFsReplyStorage};
    use crate::filesystem::fuse::conn::FuseConn;

    fn direct(vaddr: usize) -> Option<usize> {
        Some(vaddr)
    }

    #[test]
    fn compat_expansion_uses_precharged_retention_capacity() {
        let conn = FuseConn::new_for_virtiofs(256 * 1024, 256 * 1024);
        let pool = ResponseBufferPool::new(&conn);
        let reservation = pool.reserve(80).unwrap();
        let mut buffer = DeviceOutputBuffer::new(64, direct).unwrap();
        unsafe {
            buffer.submission_dma_slice(direct).unwrap();
        }
        buffer.mark_submitted();
        unsafe {
            buffer.complete_after_pop(64).unwrap();
        }
        let storage = VirtioFsReplyStorage::from_completed(buffer, reservation, 0..64).unwrap();

        let storage = storage.into_compat_bytes(vec![0; 80]).unwrap();
        assert_eq!(storage.as_slice().len(), 80);
    }
}
