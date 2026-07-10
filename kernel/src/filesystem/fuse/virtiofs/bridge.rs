use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    string::String,
    sync::Arc,
    vec,
    vec::Vec,
};

use log::{debug, info, warn};
use system_error::SystemError;
use virtio_drivers::{
    transport::{DeviceStatus, Transport},
    Error as VirtioError, PAGE_SIZE,
};

use crate::{
    driver::virtio::{
        transport::VirtIOTransport, virtio_drivers_error_to_system_error,
        virtio_fs::VirtioFsInstance,
    },
    process::{kthread::KernelThreadClosure, kthread::KernelThreadMechanism},
    time::{sleep::nanosleep, PosixTimeSpec},
};

use super::super::{
    conn::{FuseConn, FuseReplyCapacitySource, FuseReplyContract, FuseRequest},
    protocol::{
        fuse_pack_struct, fuse_read_struct, FuseOutHeader, FUSE_DESTROY, FUSE_FORGET,
        FUSE_INTERRUPT,
    },
    stats, trace,
};
use super::{
    queue::{create_queue, wait_transport_reset_complete, VirtioFsQueue},
    VIRTIOFS_MAX_REQUEST_SIZE, VIRTIOFS_RSP_BUF_SIZE,
};

const FUSE_HEADER_OVERHEAD: usize = 4;
const FUSE_MAX_MAX_PAGES: usize = 256;
const VIRTIOFS_RSP_POOL_MAX_BUFFERS: usize = 16;
const VIRTIOFS_RSP_POOL_MAX_CAPACITY_BYTES: usize =
    VIRTIOFS_RSP_BUF_SIZE * VIRTIOFS_RSP_POOL_MAX_BUFFERS;
const VIRTIOFS_POLL_NS: i64 = 1_000_000;
const VIRTIOFS_PUMP_BUDGET: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueKind {
    Hiprio,
    Request(usize),
}

#[derive(Debug)]
struct PendingReq {
    req: Arc<FuseRequest>,
    unique: u64,
    opcode: u32,
    noreply: bool,
    queue: QueueKind,
}

#[derive(Debug)]
struct InflightReq {
    pending: PendingReq,
    rsp: Option<Vec<u8>>,
}

#[derive(Debug, Default)]
struct QueueBlockState {
    blocked: bool,
    completion_seen: bool,
}

#[derive(Debug)]
struct ResponseBufferPool {
    buffers: Vec<Vec<u8>>,
    retained_capacity_bytes: usize,
}

impl ResponseBufferPool {
    fn new() -> Self {
        Self {
            buffers: Vec::with_capacity(VIRTIOFS_RSP_POOL_MAX_BUFFERS),
            retained_capacity_bytes: 0,
        }
    }

    fn acquire(&mut self, opcode: u32, len: usize) -> Vec<u8> {
        let best_fit = self
            .buffers
            .iter()
            .enumerate()
            .filter(|(_, buf)| buf.capacity() >= len)
            .min_by_key(|(_, buf)| buf.capacity())
            .map(|(index, _)| index);
        if let Some(index) = best_fit {
            let mut buf = self.buffers.swap_remove(index);
            self.retained_capacity_bytes = self
                .retained_capacity_bytes
                .checked_sub(buf.capacity())
                .expect("response pool capacity accounting underflow");
            let old_len = buf.len();
            if len > old_len {
                // `resize` zero-initializes the newly exposed tail. Clear only the old prefix so
                // every byte visible to the device is written exactly once on this reuse path.
                buf.resize(len, 0);
                buf[..old_len].fill(0);
            } else {
                buf.truncate(len);
                buf.fill(0);
            }
            stats::on_virtiofs_response_buffer_reuse(opcode, len);
            stats::on_virtiofs_response_buffer_zero(opcode, len);
            return buf;
        }

        let buf = vec![0u8; len];
        stats::on_virtiofs_response_buffer_alloc(opcode, len);
        stats::on_virtiofs_response_buffer_zero(opcode, len);
        buf
    }

    fn release(&mut self, buf: Vec<u8>, reusable: bool) {
        let capacity = buf.capacity();
        let next_capacity = self.retained_capacity_bytes.checked_add(capacity);
        let within_limits = reusable
            && buf.len() <= VIRTIOFS_RSP_BUF_SIZE
            && capacity <= VIRTIOFS_RSP_BUF_SIZE
            && self.buffers.len() < VIRTIOFS_RSP_POOL_MAX_BUFFERS
            && next_capacity.is_some_and(|total| total <= VIRTIOFS_RSP_POOL_MAX_CAPACITY_BYTES);

        if !within_limits {
            stats::on_virtiofs_response_pool_drop();
            return;
        }

        self.retained_capacity_bytes = next_capacity.expect("checked above");
        self.buffers.push(buf);
    }

    fn clear(&mut self) {
        self.buffers.clear();
        self.retained_capacity_bytes = 0;
    }
}

impl Drop for ResponseBufferPool {
    fn drop(&mut self) {
        self.clear();
    }
}

fn cleanup_created_queues(
    transport: &mut VirtIOTransport,
    hiprio_idx: u16,
    request_indices: &[u16],
) -> bool {
    transport.set_status(DeviceStatus::empty());
    if !wait_transport_reset_complete(transport) {
        return false;
    }

    transport.queue_unset(hiprio_idx);
    for idx in request_indices {
        transport.queue_unset(*idx);
    }
    true
}

fn put_transport_after_start_failure(
    instance: &Arc<VirtioFsInstance>,
    session_id: u64,
    mut transport: VirtIOTransport,
    hiprio_vq: VirtioFsQueue,
    request_vqs: Vec<VirtioFsQueue>,
    hiprio_idx: u16,
    request_indices: &[u16],
) {
    if cleanup_created_queues(&mut transport, hiprio_idx, request_indices) {
        drop(hiprio_vq);
        drop(request_vqs);
        transport.set_status(DeviceStatus::FAILED);
        instance.put_transport_after_session(transport);
    } else {
        warn!(
            "virtiofs bridge: device reset did not complete during start failure cleanup tag='{}' dev={:?}; keep transport unavailable",
            instance.tag(),
            instance.dev_id()
        );
        core::mem::forget(transport);
        core::mem::forget(hiprio_vq);
        core::mem::forget(request_vqs);
        if !instance.release_session_without_transport(session_id) {
            warn!(
                "virtiofs bridge: failed to release quarantined start session id={} tag='{}' dev={:?}",
                session_id,
                instance.tag(),
                instance.dev_id()
            );
        }
    }
}

fn reset_transport_after_start_failure(
    instance: &Arc<VirtioFsInstance>,
    session_id: u64,
    mut transport: VirtIOTransport,
) {
    transport.set_status(DeviceStatus::empty());
    if wait_transport_reset_complete(&transport) {
        transport.set_status(DeviceStatus::FAILED);
        instance.put_transport_after_session(transport);
    } else {
        warn!(
            "virtiofs bridge: device reset did not complete during start failure tag='{}' dev={:?}; keep transport unavailable",
            instance.tag(),
            instance.dev_id()
        );
        core::mem::forget(transport);
        if !instance.release_session_without_transport(session_id) {
            warn!(
                "virtiofs bridge: failed to release quarantined start session id={} tag='{}' dev={:?}",
                session_id,
                instance.tag(),
                instance.dev_id()
            );
        }
    }
}

struct VirtioFsBridgeContext {
    instance: Arc<VirtioFsInstance>,
    conn: Arc<FuseConn>,
    session_id: u64,
    irq_wake_enabled: bool,
    transport: Option<VirtIOTransport>,
    hiprio_vq: Option<VirtioFsQueue>,
    request_vqs: Vec<VirtioFsQueue>,
    hiprio_pending: VecDeque<PendingReq>,
    request_pending: Vec<VecDeque<PendingReq>>,
    hiprio_inflight: BTreeMap<u16, InflightReq>,
    request_inflight: Vec<BTreeMap<u16, InflightReq>>,
    response_pool: ResponseBufferPool,
    next_request_slot: usize,
    hiprio_blocked: QueueBlockState,
    request_blocked: Vec<QueueBlockState>,
}

impl VirtioFsBridgeContext {
    fn poll_pause() {
        let _ = nanosleep(PosixTimeSpec::new(0, VIRTIOFS_POLL_NS));
    }

    fn has_internal_pending(&self) -> bool {
        !self.hiprio_pending.is_empty() || self.request_pending.iter().any(|q| !q.is_empty())
    }

    fn has_inflight(&self) -> bool {
        !self.hiprio_inflight.is_empty() || self.request_inflight.iter().any(|m| !m.is_empty())
    }

    fn has_completion_available(&self) -> bool {
        self.hiprio_vq.as_ref().is_some_and(|queue| queue.can_pop())
            || self.request_vqs.iter().any(|queue| queue.can_pop())
    }

    fn has_queue_full_blocked(&self) -> bool {
        self.hiprio_blocked.blocked || self.request_blocked.iter().any(|state| state.blocked)
    }

    fn can_pump_high_priority(&self) -> bool {
        !self.hiprio_blocked.blocked
    }

    fn has_unblocked_request_slot(&self) -> bool {
        Self::has_unblocked_request_slot_in(&self.request_blocked, self.request_vqs.len())
    }

    fn has_unblocked_request_slot_in(
        request_blocked: &[QueueBlockState],
        request_queue_count: usize,
    ) -> bool {
        (0..request_queue_count).any(|slot| {
            request_blocked
                .get(slot)
                .is_some_and(|state| !state.blocked)
        })
    }

    fn block_state_mut(&mut self, kind: QueueKind) -> Result<&mut QueueBlockState, SystemError> {
        match kind {
            QueueKind::Hiprio => Ok(&mut self.hiprio_blocked),
            QueueKind::Request(slot) => self
                .request_blocked
                .get_mut(slot)
                .ok_or(SystemError::EINVAL),
        }
    }

    fn mark_queue_full_blocked(&mut self, kind: QueueKind) -> Result<(), SystemError> {
        let state = self.block_state_mut(kind)?;
        if !state.blocked {
            stats::on_virtiofs_queue_full_blocked();
        }
        state.blocked = true;
        state.completion_seen = false;
        Ok(())
    }

    fn mark_queue_completion_seen(&mut self, kind: QueueKind) {
        if let Ok(state) = self.block_state_mut(kind) {
            if state.blocked {
                state.completion_seen = true;
            }
        }
    }

    fn push_pending_back(&mut self, pending: PendingReq) -> Result<(), SystemError> {
        match pending.queue {
            QueueKind::Hiprio => self.hiprio_pending.push_back(pending),
            QueueKind::Request(slot) => self
                .request_pending
                .get_mut(slot)
                .ok_or(SystemError::EINVAL)?
                .push_back(pending),
        }
        Ok(())
    }

    fn push_pending_front(&mut self, pending: PendingReq) -> Result<(), SystemError> {
        match pending.queue {
            QueueKind::Hiprio => self.hiprio_pending.push_front(pending),
            QueueKind::Request(slot) => self
                .request_pending
                .get_mut(slot)
                .ok_or(SystemError::EINVAL)?
                .push_front(pending),
        }
        Ok(())
    }

    fn pop_pending_front(&mut self, kind: QueueKind) -> Result<Option<PendingReq>, SystemError> {
        Ok(match kind {
            QueueKind::Hiprio => self.hiprio_pending.pop_front(),
            QueueKind::Request(slot) => self
                .request_pending
                .get_mut(slot)
                .ok_or(SystemError::EINVAL)?
                .pop_front(),
        })
    }

    fn queue_can_retry(&self, kind: QueueKind) -> Result<bool, SystemError> {
        let state = match kind {
            QueueKind::Hiprio => &self.hiprio_blocked,
            QueueKind::Request(slot) => {
                self.request_blocked.get(slot).ok_or(SystemError::EINVAL)?
            }
        };
        Ok(!state.blocked || state.completion_seen)
    }

    fn request_inflight_contains_unique(&self, unique: u64) -> bool {
        self.request_inflight
            .iter()
            .any(|map| map.values().any(|req| req.pending.unique == unique))
    }

    fn hiprio_pending_submit_ready(&self, pending: &PendingReq) -> bool {
        if pending.opcode != FUSE_INTERRUPT {
            return true;
        }

        let target_unique = FuseConn::interrupt_target_unique(pending.unique);
        self.conn.has_processing_request(target_unique)
            && self.request_inflight_contains_unique(target_unique)
    }

    fn pop_ready_hiprio_pending(&mut self) -> Result<Option<PendingReq>, SystemError> {
        if !self.queue_can_retry(QueueKind::Hiprio)? {
            return Ok(None);
        }

        let mut index = 0usize;
        while index < self.hiprio_pending.len() {
            let (opcode, unique) = {
                let pending = self.hiprio_pending.get(index).ok_or(SystemError::EINVAL)?;
                (pending.opcode, pending.unique)
            };

            if opcode == FUSE_INTERRUPT {
                let target_unique = FuseConn::interrupt_target_unique(unique);
                if !self.conn.has_processing_request(target_unique) {
                    self.hiprio_pending.remove(index);
                    stats::on_fuse_requests_aborted(1);
                    continue;
                }
            }

            if self
                .hiprio_pending
                .get(index)
                .is_some_and(|pending| self.hiprio_pending_submit_ready(pending))
            {
                return self
                    .hiprio_pending
                    .remove(index)
                    .map(Some)
                    .ok_or(SystemError::EINVAL);
            }

            index += 1;
        }

        Ok(None)
    }

    fn destroy_barrier_ready(&self, unique: u64) -> bool {
        let request_pending_has_other = self.request_pending.iter().any(|queue| {
            queue
                .iter()
                .any(|pending| pending.opcode != FUSE_DESTROY || pending.unique != unique)
        });
        Self::destroy_barrier_ready_state(
            self.conn.has_pending_requests(),
            !self.hiprio_pending.is_empty(),
            !self.hiprio_inflight.is_empty(),
            self.request_inflight.iter().any(|map| !map.is_empty()),
            request_pending_has_other,
        )
    }

    fn destroy_barrier_ready_state(
        conn_has_pending: bool,
        hiprio_has_pending: bool,
        hiprio_has_inflight: bool,
        request_has_inflight: bool,
        request_pending_has_other: bool,
    ) -> bool {
        !(conn_has_pending
            || hiprio_has_pending
            || hiprio_has_inflight
            || request_has_inflight
            || request_pending_has_other)
    }

    fn pop_pending_for_submit(
        &mut self,
        kind: QueueKind,
    ) -> Result<Option<PendingReq>, SystemError> {
        if matches!(kind, QueueKind::Hiprio) {
            return self.pop_ready_hiprio_pending();
        }
        if !self.queue_can_retry(kind)? {
            return Ok(None);
        }
        if let QueueKind::Request(slot) = kind {
            let pending = self
                .request_pending
                .get(slot)
                .ok_or(SystemError::EINVAL)?
                .front();
            if pending.is_some_and(|pending| {
                pending.opcode == FUSE_DESTROY && !self.destroy_barrier_ready(pending.unique)
            }) {
                return Ok(None);
            }
        }
        self.pop_pending_front(kind)
    }

    fn queue_index(&self, kind: QueueKind) -> Result<u16, SystemError> {
        match kind {
            QueueKind::Hiprio => Ok(self.instance.hiprio_queue_index()),
            QueueKind::Request(slot) => self
                .instance
                .request_queue_index_by_slot(slot)
                .ok_or(SystemError::EINVAL),
        }
    }

    fn trace_queue_fields(kind: QueueKind) -> (u8, u16) {
        match kind {
            QueueKind::Hiprio => (trace::VIRTIOFS_QUEUE_HIPRIO, 0),
            QueueKind::Request(slot) => (trace::VIRTIOFS_QUEUE_REQUEST, slot as u16),
        }
    }

    fn take_inflight(&mut self, kind: QueueKind, token: u16) -> Result<InflightReq, SystemError> {
        match kind {
            QueueKind::Hiprio => self.hiprio_inflight.remove(&token).ok_or(SystemError::EIO),
            QueueKind::Request(slot) => self
                .request_inflight
                .get_mut(slot)
                .ok_or(SystemError::EINVAL)?
                .remove(&token)
                .ok_or(SystemError::EIO),
        }
    }

    fn put_back_inflight(
        &mut self,
        kind: QueueKind,
        token: u16,
        inflight: InflightReq,
    ) -> Result<(), SystemError> {
        let replaced = match kind {
            QueueKind::Hiprio => self.hiprio_inflight.insert(token, inflight),
            QueueKind::Request(slot) => self
                .request_inflight
                .get_mut(slot)
                .ok_or(SystemError::EINVAL)?
                .insert(token, inflight),
        };
        debug_assert!(replaced.is_none());
        Ok(())
    }

    fn complete_request_with_negative_errno(conn: &Arc<FuseConn>, unique: u64, errno: i32) {
        if unique == 0 {
            return;
        }
        if errno <= -512 || errno >= 0 {
            warn!(
                "virtiofs bridge: invalid internal completion errno={} unique={}",
                errno, unique
            );
            return;
        }
        let out_hdr = FuseOutHeader {
            len: core::mem::size_of::<FuseOutHeader>() as u32,
            error: errno,
            unique,
        };
        let payload = fuse_pack_struct(&out_hdr);
        match conn.write_reply(payload) {
            Ok(_) => {}
            Err(SystemError::ENOENT) => {
                debug!(
                    "virtiofs bridge: late internal completion ignored unique={} errno={}",
                    unique, errno
                );
            }
            Err(e) => {
                warn!(
                    "virtiofs bridge: internal completion failed unique={} errno={} err={:?}",
                    unique, errno, e
                );
            }
        }
    }

    fn complete_request_with_error(&self, unique: u64, err: SystemError) {
        Self::complete_request_with_negative_errno(&self.conn, unique, err.to_posix_errno());
    }

    fn terminate_pending_with_error(&self, pending: &PendingReq, err: SystemError) {
        if !pending.noreply {
            self.complete_request_with_error(pending.unique, err);
        }
        if pending.opcode == FUSE_DESTROY {
            self.conn.abort();
        }
    }

    fn reply_matches_expected_unique(data: &[u8], expected_unique: u64) -> bool {
        matches!(
            fuse_read_struct::<FuseOutHeader>(data),
            Ok(header) if header.unique == expected_unique
        )
    }

    fn choose_unblocked_request_slot(&mut self) -> Result<usize, SystemError> {
        Self::choose_unblocked_request_slot_in(
            &mut self.next_request_slot,
            &self.request_blocked,
            self.request_vqs.len(),
        )
    }

    fn choose_unblocked_request_slot_in(
        next_request_slot: &mut usize,
        request_blocked: &[QueueBlockState],
        request_queue_count: usize,
    ) -> Result<usize, SystemError> {
        if request_queue_count == 0 {
            return Err(SystemError::ENODEV);
        }

        for offset in 0..request_queue_count {
            let slot = (*next_request_slot + offset) % request_queue_count;
            let state = request_blocked.get(slot).ok_or(SystemError::EINVAL)?;
            if !state.blocked {
                *next_request_slot = (slot + 1) % request_queue_count;
                return Ok(slot);
            }
        }

        Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
    }

    fn pump_ordinary_requests(&mut self) -> Result<usize, SystemError> {
        let mut pumped = 0usize;
        for _ in 0..VIRTIOFS_PUMP_BUDGET {
            if !self.conn.has_pending_ordinary_requests() {
                break;
            }

            let slot = match self.choose_unblocked_request_slot() {
                Ok(slot) => slot,
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => break,
                Err(e) => return Err(e),
            };

            let req = match self
                .conn
                .dequeue_virtiofs_ordinary_request(VIRTIOFS_MAX_REQUEST_SIZE)
            {
                Ok(req) => req,
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => break,
                Err(e) => return Err(e),
            };
            let opcode = req.opcode();
            let unique = req.unique();
            let noreply = matches!(req.reply_contract(), FuseReplyContract::NoReply);
            debug_assert!(!matches!(opcode, FUSE_FORGET | FUSE_INTERRUPT));
            self.push_pending_back(PendingReq {
                req,
                unique,
                opcode,
                noreply,
                queue: QueueKind::Request(slot),
            })?;
            pumped += 1;
        }
        stats::on_virtiofs_pump_batch(pumped);
        Ok(pumped)
    }

    fn pump_high_priority_requests(&mut self) -> Result<usize, SystemError> {
        let mut pumped = 0usize;
        for _ in 0..VIRTIOFS_PUMP_BUDGET {
            let req = match self
                .conn
                .dequeue_virtiofs_high_priority_request(VIRTIOFS_MAX_REQUEST_SIZE)
            {
                Ok(req) => req,
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => break,
                Err(e) => return Err(e),
            };
            let opcode = req.opcode();
            let unique = req.unique();
            let noreply = matches!(req.reply_contract(), FuseReplyContract::NoReply);
            debug_assert!(matches!(opcode, FUSE_FORGET | FUSE_INTERRUPT));
            self.push_pending_back(PendingReq {
                req,
                unique,
                opcode,
                noreply,
                queue: QueueKind::Hiprio,
            })?;
            pumped += 1;
        }
        stats::on_virtiofs_pump_batch(pumped);
        Ok(pumped)
    }

    fn pump_available_requests(&mut self) -> Result<usize, SystemError> {
        let mut pumped = 0usize;
        if self.can_pump_high_priority() && self.conn.has_pending_high_priority_requests() {
            pumped += self.pump_high_priority_requests()?;
        }
        if self.has_unblocked_request_slot() && self.conn.has_pending_ordinary_requests() {
            pumped += self.pump_ordinary_requests()?;
        }
        Ok(pumped)
    }

    fn submit_one_pending(&mut self, kind: QueueKind) -> Result<bool, SystemError> {
        if !self.conn.is_connected() {
            return Ok(false);
        }
        let queue_idx = self.queue_index(kind)?;
        if self.transport.is_none() {
            return Err(SystemError::EIO);
        }
        match kind {
            QueueKind::Hiprio => {
                if self.hiprio_vq.is_none() {
                    return Err(SystemError::EIO);
                }
            }
            QueueKind::Request(slot) => {
                if slot >= self.request_vqs.len()
                    || slot >= self.request_inflight.len()
                    || slot >= self.request_blocked.len()
                {
                    return Err(SystemError::EINVAL);
                }
            }
        }

        let pending = match self.pop_pending_for_submit(kind)? {
            Some(p) => p,
            None => return Ok(false),
        };
        let retry_after_completion = {
            let state = self.block_state_mut(kind)?;
            state.blocked.then_some(state.completion_seen)
        };
        if let Some(after_completion) = retry_after_completion {
            stats::on_virtiofs_queue_full_retry(after_completion);
        }
        let (queue_kind, queue_slot) = Self::trace_queue_fields(kind);
        let trace_unique = pending.unique;
        let trace_opcode = pending.opcode;
        let req_len = pending.req.bytes().len();
        let (mut rsp, fallback) = match pending.req.reply_contract() {
            FuseReplyContract::NoReply => (None, false),
            FuseReplyContract::Reply { capacity } => {
                let capacity = match capacity {
                    Some(capacity) => capacity,
                    None => {
                        self.terminate_pending_with_error(&pending, SystemError::EIO);
                        return Ok(true);
                    }
                };
                (
                    Some(self.response_pool.acquire(pending.opcode, capacity.bytes)),
                    matches!(capacity.source, FuseReplyCapacitySource::ExplicitFallback),
                )
            }
        };

        let (token, should_notify) = match kind {
            QueueKind::Hiprio => {
                let queue = self.hiprio_vq.as_mut().ok_or(SystemError::EIO)?;
                let token = if let Some(rsp_buf) = rsp.as_mut() {
                    let inputs = [pending.req.bytes()];
                    let mut outputs = [rsp_buf.as_mut_slice()];
                    // SAFETY: The request contains a non-empty FuseInHeader, and the reply
                    // contract makes `rsp_buf` at least a non-empty FuseOutHeader. On success,
                    // only their owners and independent metadata are moved/read before notification;
                    // neither buffer's contents are accessed again until `pop_used` succeeds or
                    // reset completes and `detach_unused` succeeds. On error no descriptor was
                    // accepted.
                    unsafe { queue.add(&inputs, &mut outputs) }
                } else {
                    let inputs = [pending.req.bytes()];
                    let mut outputs: [&mut [u8]; 0] = [];
                    // SAFETY: The request contains a non-empty FuseInHeader; the output slice
                    // list is empty for this no-reply request. On success, only its owner and
                    // independent metadata are moved/read before notification; the request contents
                    // are not accessed again until `pop_used` succeeds or reset completes and
                    // `detach_unused` succeeds. On error no descriptor was accepted.
                    unsafe { queue.add(&inputs, &mut outputs) }
                };
                (token, queue.should_notify())
            }
            QueueKind::Request(slot) => {
                let queue = self.request_vqs.get_mut(slot).ok_or(SystemError::EINVAL)?;
                let token = if let Some(rsp_buf) = rsp.as_mut() {
                    let inputs = [pending.req.bytes()];
                    let mut outputs = [rsp_buf.as_mut_slice()];
                    // SAFETY: The request contains a non-empty FuseInHeader, and the reply
                    // contract makes `rsp_buf` at least a non-empty FuseOutHeader. On success,
                    // only their owners and independent metadata are moved/read before notification;
                    // neither buffer's contents are accessed again until `pop_used` succeeds or
                    // reset completes and `detach_unused` succeeds. On error no descriptor was
                    // accepted.
                    unsafe { queue.add(&inputs, &mut outputs) }
                } else {
                    let inputs = [pending.req.bytes()];
                    let mut outputs: [&mut [u8]; 0] = [];
                    // SAFETY: The request contains a non-empty FuseInHeader; the output slice
                    // list is empty for this no-reply request. On success, only its owner and
                    // independent metadata are moved/read before notification; the request contents
                    // are not accessed again until `pop_used` succeeds or reset completes and
                    // `detach_unused` succeeds. On error no descriptor was accepted.
                    unsafe { queue.add(&inputs, &mut outputs) }
                };
                (token, queue.should_notify())
            }
        };

        let token = match token {
            Ok(token) => token,
            Err(VirtioError::QueueFull) => {
                if let Some(rsp_buf) = rsp.take() {
                    self.response_pool.release(rsp_buf, true);
                }
                stats::on_virtiofs_queue_full(kind.stats_kind());
                self.mark_queue_full_blocked(kind)?;
                let (queue_kind, queue_slot) = Self::trace_queue_fields(kind);
                trace::trace_virtiofs_queue_retry(
                    pending.unique,
                    pending.opcode,
                    queue_kind,
                    queue_slot,
                    trace::VIRTIOFS_RETRY_QUEUE_FULL,
                );
                self.push_pending_front(pending)?;
                return Ok(false);
            }
            Err(VirtioError::NotReady) => {
                if let Some(rsp_buf) = rsp.take() {
                    self.response_pool.release(rsp_buf, true);
                }
                stats::on_virtiofs_not_ready();
                stats::on_virtiofs_submit_error();
                let se = virtio_drivers_error_to_system_error(VirtioError::NotReady);
                warn!(
                    "virtiofs bridge: queue not ready opcode={} unique={} queue={:?} err={:?}",
                    pending.opcode, pending.unique, kind, se
                );
                self.terminate_pending_with_error(&pending, se);
                return Ok(true);
            }
            Err(e) => {
                if let Some(rsp_buf) = rsp.take() {
                    self.response_pool.release(rsp_buf, true);
                }
                stats::on_virtiofs_submit_error();
                let se = virtio_drivers_error_to_system_error(e);
                warn!(
                    "virtiofs bridge: submit failed opcode={} unique={} queue={:?} err={:?}",
                    pending.opcode, pending.unique, kind, se
                );
                self.terminate_pending_with_error(&pending, se);
                return Ok(true);
            }
        };

        if let Some(rsp_buf) = rsp.as_ref() {
            stats::on_virtiofs_response_submitted(trace_opcode, rsp_buf.len(), fallback);
        }
        let inflight = InflightReq { pending, rsp };
        let replaced = match kind {
            QueueKind::Hiprio => self.hiprio_inflight.insert(token, inflight),
            QueueKind::Request(slot) => self.request_inflight[slot].insert(token, inflight),
        };
        debug_assert!(replaced.is_none());
        stats::on_virtiofs_inflight_add(kind.stats_kind());
        stats::on_virtiofs_submitted(trace_opcode, req_len);

        let retry_succeeded = {
            let state = self
                .block_state_mut(kind)
                .expect("queue kind was validated before virtqueue submission");
            if state.blocked {
                state.blocked = false;
                state.completion_seen = false;
                stats::on_virtiofs_queue_full_unblocked();
                true
            } else {
                false
            }
        };
        if retry_succeeded {
            stats::on_virtiofs_queue_full_retry_success();
        }

        if should_notify {
            self.transport
                .as_mut()
                .expect("transport was validated before virtqueue submission")
                .notify(queue_idx);
        }

        trace::trace_virtiofs_submit(
            trace_unique,
            trace_opcode,
            queue_kind,
            queue_slot,
            token,
            req_len as u64,
        );
        Ok(true)
    }

    fn submit_pending(&mut self) -> Result<bool, SystemError> {
        let mut progressed = false;
        while self.submit_one_pending(QueueKind::Hiprio)? {
            progressed = true;
        }
        if !self.conn.is_connected() {
            return Ok(progressed);
        }

        for slot in 0..self.request_vqs.len() {
            while self.submit_one_pending(QueueKind::Request(slot))? {
                progressed = true;
            }
            if !self.conn.is_connected() {
                break;
            }
        }
        Ok(progressed)
    }

    fn pop_one_used(&mut self, kind: QueueKind) -> Result<bool, SystemError> {
        let token = match kind {
            QueueKind::Hiprio => {
                let queue = self.hiprio_vq.as_mut().ok_or(SystemError::EIO)?;
                if !queue.can_pop() {
                    return Ok(false);
                }
                queue.peek_used().ok_or(SystemError::EIO)?
            }
            QueueKind::Request(slot) => {
                let queue = self.request_vqs.get_mut(slot).ok_or(SystemError::EINVAL)?;
                if !queue.can_pop() {
                    return Ok(false);
                }
                queue.peek_used().ok_or(SystemError::EIO)?
            }
        };

        let mut inflight = self.take_inflight(kind, token)?;

        let used_len_res = match kind {
            QueueKind::Hiprio => {
                let queue = self.hiprio_vq.as_mut().ok_or(SystemError::EIO)?;
                let inputs = [inflight.pending.req.bytes()];
                if let Some(rsp_buf) = inflight.rsp.as_mut() {
                    let mut outputs = [rsp_buf.as_mut_slice()];
                    // SAFETY: `inflight` is the token-indexed owner of the exact request and
                    // response buffers submitted by `add`. Both remain live during this call;
                    // an error puts the same owner back, while success retires device access.
                    unsafe { queue.pop_used(token, &inputs, &mut outputs) }
                        .map_err(virtio_drivers_error_to_system_error)
                } else {
                    let mut outputs: [&mut [u8]; 0] = [];
                    // SAFETY: `inflight` is the token-indexed owner of the exact request buffer
                    // submitted by `add` and keeps it live during this call. This no-reply request
                    // has no output buffer; an error restores the owner, while success retires
                    // device access.
                    unsafe { queue.pop_used(token, &inputs, &mut outputs) }
                        .map_err(virtio_drivers_error_to_system_error)
                }
            }
            QueueKind::Request(slot) => {
                let queue = self.request_vqs.get_mut(slot).ok_or(SystemError::EINVAL)?;
                let inputs = [inflight.pending.req.bytes()];
                if let Some(rsp_buf) = inflight.rsp.as_mut() {
                    let mut outputs = [rsp_buf.as_mut_slice()];
                    // SAFETY: `inflight` is the token-indexed owner of the exact request and
                    // response buffers submitted by `add`. Both remain live during this call;
                    // an error puts the same owner back, while success retires device access.
                    unsafe { queue.pop_used(token, &inputs, &mut outputs) }
                        .map_err(virtio_drivers_error_to_system_error)
                } else {
                    let mut outputs: [&mut [u8]; 0] = [];
                    // SAFETY: `inflight` is the token-indexed owner of the exact request buffer
                    // submitted by `add` and keeps it live during this call. This no-reply request
                    // has no output buffer; an error restores the owner, while success retires
                    // device access.
                    unsafe { queue.pop_used(token, &inputs, &mut outputs) }
                        .map_err(virtio_drivers_error_to_system_error)
                }
            }
        };

        let used_len = match used_len_res {
            Ok(v) => v as usize,
            Err(e) => {
                stats::on_virtiofs_pop_used_error();
                let unique = inflight.pending.unique;
                self.put_back_inflight(kind, token, inflight)?;
                warn!(
                    "virtiofs bridge: pop_used failed unique={} token={} queue={:?} err={:?}",
                    unique, token, kind, e
                );
                return Err(e);
            }
        };
        stats::on_virtiofs_inflight_remove(kind.stats_kind(), 1);

        let (queue_kind, queue_slot) = Self::trace_queue_fields(kind);
        trace::trace_virtiofs_complete(
            inflight.pending.unique,
            inflight.pending.opcode,
            queue_kind,
            queue_slot,
            token,
            used_len as u64,
        );

        if inflight.pending.noreply {
            stats::on_virtiofs_completed(used_len, true);
            return Ok(true);
        }

        let rsp_buf = inflight
            .rsp
            .take()
            .expect("reply-bearing inflight request must own a response buffer");
        stats::on_virtiofs_response_completed(inflight.pending.opcode, rsp_buf.len(), used_len);
        if used_len > rsp_buf.len() {
            stats::on_virtiofs_completed(used_len, false);
            self.terminate_pending_with_error(&inflight.pending, SystemError::EIO);
            self.response_pool.release(rsp_buf, false);
            return Ok(true);
        }
        if rsp_buf.len() > used_len {
            stats::on_virtiofs_response_buffer_waste(rsp_buf.len() - used_len);
        }
        stats::on_virtiofs_completed(used_len, false);

        if inflight.pending.opcode == FUSE_DESTROY && used_len == 0 {
            let reusable = match self
                .conn
                .complete_destroy_without_reply(inflight.pending.unique)
            {
                Ok(()) => true,
                Err(e) => {
                    warn!(
                        "virtiofs bridge: zero-length DESTROY completion failed unique={} err={:?}",
                        inflight.pending.unique, e
                    );
                    self.terminate_pending_with_error(&inflight.pending, SystemError::EIO);
                    false
                }
            };
            self.response_pool.release(rsp_buf, reusable);
            return Ok(true);
        }

        let expected_unique = inflight.pending.unique;
        if !Self::reply_matches_expected_unique(&rsp_buf[..used_len], expected_unique) {
            warn!(
                "virtiofs bridge: reply unique mismatch or short header expected={} opcode={} used_len={}",
                expected_unique, inflight.pending.opcode, used_len
            );
            self.terminate_pending_with_error(&inflight.pending, SystemError::EIO);
            self.response_pool.release(rsp_buf, false);
            return Ok(true);
        }

        let reusable = match self.conn.write_reply(&rsp_buf[..used_len]) {
            Ok(_) => true,
            Err(SystemError::ENOENT) if inflight.pending.opcode != FUSE_DESTROY => true,
            Err(e) => {
                // Linux virtio-fs always ends a completed request from the used ring.
                // Keep that behavior here: fail this unique instead of exiting bridge loop.
                let unique = inflight.pending.unique;
                let completion_err = if e == SystemError::EINVAL {
                    SystemError::EIO
                } else {
                    e.clone()
                };
                warn!(
                    "virtiofs bridge: write_reply failed unique={} opcode={} err={:?}, complete with {:?}",
                    unique, inflight.pending.opcode, e, completion_err
                );
                self.terminate_pending_with_error(&inflight.pending, completion_err);
                false
            }
        };
        self.response_pool.release(rsp_buf, reusable);
        Ok(true)
    }

    fn drain_completions(&mut self) -> Result<usize, SystemError> {
        let mut completed = 0usize;
        let mut hiprio_completed = 0usize;
        while self.pop_one_used(QueueKind::Hiprio)? {
            hiprio_completed += 1;
            completed += 1;
        }
        if hiprio_completed != 0 {
            self.mark_queue_completion_seen(QueueKind::Hiprio);
        }

        for slot in 0..self.request_vqs.len() {
            let mut queue_completed = 0usize;
            while self.pop_one_used(QueueKind::Request(slot))? {
                queue_completed += 1;
                completed += 1;
            }
            if queue_completed != 0 {
                self.mark_queue_completion_seen(QueueKind::Request(slot));
            }
        }

        stats::on_virtiofs_complete_batch(completed);
        Ok(completed)
    }

    fn bridge_wait_exit_reason(
        &self,
        conn: &FuseConn,
        events: u32,
    ) -> Option<stats::VirtioFsBridgeWaitExit> {
        if !conn.is_connected() {
            return Some(stats::VirtioFsBridgeWaitExit::Disconnect);
        }

        let teardown_event = events & stats::VirtioFsBridgeWakeSource::Teardown.bit() != 0;
        if teardown_event {
            return Some(stats::VirtioFsBridgeWaitExit::Teardown);
        }

        let completion_event = events & stats::VirtioFsBridgeWakeSource::Completion.bit() != 0;
        if completion_event || self.has_completion_available() {
            return Some(stats::VirtioFsBridgeWaitExit::Completion);
        }

        if self.can_pump_high_priority() && conn.has_pending_high_priority_requests() {
            return Some(stats::VirtioFsBridgeWaitExit::RequestPending);
        }
        if self.has_unblocked_request_slot() && conn.has_pending_ordinary_requests() {
            return Some(stats::VirtioFsBridgeWaitExit::RequestPending);
        }

        let disconnect_event = events & stats::VirtioFsBridgeWakeSource::Disconnect.bit() != 0;
        if disconnect_event {
            return Some(stats::VirtioFsBridgeWaitExit::Disconnect);
        }

        let known_events = stats::VirtioFsBridgeWakeSource::Request.bit()
            | stats::VirtioFsBridgeWakeSource::Completion.bit()
            | stats::VirtioFsBridgeWakeSource::Teardown.bit()
            | stats::VirtioFsBridgeWakeSource::Disconnect.bit();
        if events & !known_events != 0 {
            return Some(stats::VirtioFsBridgeWaitExit::Spurious);
        }

        None
    }

    fn wait_for_event(&mut self) {
        if !self.irq_wake_enabled {
            stats::on_virtiofs_idle_sleep(VIRTIOFS_POLL_NS);
            Self::poll_pause();
            return;
        }

        let conn = self.conn.clone();
        trace::trace_virtiofs_bridge_wait_enter(
            self.has_internal_pending() as u8,
            self.has_inflight() as u8,
            self.has_completion_available() as u8,
            self.has_queue_full_blocked() as u8,
        );
        stats::on_virtiofs_bridge_wait();
        let reason = conn.wait_bridge_until(|events| self.bridge_wait_exit_reason(&conn, events));
        let events = conn.take_bridge_wake_events();
        stats::on_virtiofs_bridge_wait_exit(reason);
        trace::trace_virtiofs_bridge_wait_exit(reason.trace_id(), events);
    }

    fn drain_pending_reply_uniques(&mut self, need_reply: &mut Vec<u64>) {
        while let Some(req) = self.hiprio_pending.pop_front() {
            if !req.noreply {
                need_reply.push(req.unique);
            }
        }

        for pending_q in &mut self.request_pending {
            while let Some(req) = pending_q.pop_front() {
                if !req.noreply {
                    need_reply.push(req.unique);
                }
            }
        }
    }

    fn collect_inflight_reply_uniques(&self, need_reply: &mut Vec<u64>) {
        for (_, req) in self.hiprio_inflight.iter() {
            if !req.pending.noreply {
                need_reply.push(req.pending.unique);
            }
        }

        for inflight_map in &self.request_inflight {
            for (_, req) in inflight_map.iter() {
                if !req.pending.noreply {
                    need_reply.push(req.pending.unique);
                }
            }
        }
    }

    fn complete_unfinished(&self, err: SystemError, need_reply: Vec<u64>) {
        let failed = need_reply.len();
        let errno = err.to_posix_errno();
        for unique in need_reply {
            Self::complete_request_with_negative_errno(&self.conn, unique, errno);
        }
        stats::on_virtiofs_fail_unfinished(failed);
    }

    fn fail_unfinished_preserving_inflight_dma(&mut self, err: SystemError) {
        let mut need_reply = Vec::new();
        self.drain_pending_reply_uniques(&mut need_reply);
        self.collect_inflight_reply_uniques(&mut need_reply);
        self.complete_unfinished(err, need_reply);
    }

    fn fail_all_unfinished(&mut self, err: SystemError) {
        let mut need_reply = Vec::new();
        self.drain_pending_reply_uniques(&mut need_reply);
        self.collect_inflight_reply_uniques(&mut need_reply);

        let hiprio_inflight_count = self.hiprio_inflight.len();
        self.hiprio_inflight.clear();
        stats::on_virtiofs_inflight_remove(stats::VirtioFsQueueKind::Hiprio, hiprio_inflight_count);

        let mut request_inflight_count = 0usize;
        for inflight_map in &mut self.request_inflight {
            request_inflight_count += inflight_map.len();
            inflight_map.clear();
        }
        stats::on_virtiofs_inflight_remove(
            stats::VirtioFsQueueKind::Request,
            request_inflight_count,
        );
        self.complete_unfinished(err, need_reply);
    }

    fn clear_queue_full_blocked_stats(&mut self) {
        if self.hiprio_blocked.blocked {
            self.hiprio_blocked.blocked = false;
            self.hiprio_blocked.completion_seen = false;
            stats::on_virtiofs_queue_full_unblocked();
        }

        for state in &mut self.request_blocked {
            if state.blocked {
                state.blocked = false;
                state.completion_seen = false;
                stats::on_virtiofs_queue_full_unblocked();
            }
        }
    }

    fn detach_inflight_descriptors_for_queue(
        queue: &mut VirtioFsQueue,
        inflight: &mut BTreeMap<u16, InflightReq>,
        kind: QueueKind,
    ) {
        for (token, req) in inflight.iter_mut() {
            let inputs = [req.pending.req.bytes()];
            let result = if let Some(rsp_buf) = req.rsp.as_mut() {
                let mut outputs = [rsp_buf.as_mut_slice()];
                // SAFETY: The caller invokes this only after the device reset completed, so the
                // device can no longer access the queue. The buffers come from the same InflightReq
                // that was inserted immediately after `VirtQueue::add` returned this token.
                unsafe { queue.detach_unused(*token, &inputs, &mut outputs) }
            } else {
                let mut outputs: [&mut [u8]; 0] = [];
                // SAFETY: Same as above; this request had no device-writable response buffer.
                unsafe { queue.detach_unused(*token, &inputs, &mut outputs) }
            };

            if let Err(e) = result {
                warn!(
                    "virtiofs bridge: failed to detach inflight descriptor token={} queue={:?} unique={} err={:?}",
                    token, kind, req.pending.unique, e
                );
            }
        }
    }

    fn detach_inflight_descriptors_after_reset(&mut self) {
        if let Some(queue) = self.hiprio_vq.as_mut() {
            Self::detach_inflight_descriptors_for_queue(
                queue,
                &mut self.hiprio_inflight,
                QueueKind::Hiprio,
            );
        }

        for slot in 0..self.request_vqs.len() {
            if let Some(inflight) = self.request_inflight.get_mut(slot) {
                Self::detach_inflight_descriptors_for_queue(
                    &mut self.request_vqs[slot],
                    inflight,
                    QueueKind::Request(slot),
                );
            }
        }
    }

    fn reset_device_and_unset_queues(&mut self) -> bool {
        let reset_complete = if let Some(transport) = self.transport.as_mut() {
            transport.set_status(DeviceStatus::empty());
            wait_transport_reset_complete(transport)
        } else {
            return true;
        };

        if !reset_complete {
            warn!(
                "virtiofs bridge: device reset did not complete before queue cleanup tag='{}' dev={:?}; keep queues and transport unavailable",
                self.instance.tag(),
                self.instance.dev_id()
            );
            return false;
        } else {
            self.detach_inflight_descriptors_after_reset();
        }

        if let Some(transport) = self.transport.as_mut() {
            if self.hiprio_vq.take().is_some() {
                transport.queue_unset(self.instance.hiprio_queue_index());
            }
            for slot in 0..self.request_vqs.len() {
                if let Some(idx) = self.instance.request_queue_index_by_slot(slot) {
                    transport.queue_unset(idx);
                }
            }
            self.request_vqs.clear();
        }

        true
    }

    fn run_loop(&mut self) -> Result<(), SystemError> {
        loop {
            if !self.conn.is_connected() {
                break;
            }
            let mut progressed = false;

            let pump_result = self.pump_available_requests();
            match pump_result {
                Ok(v) => progressed |= v != 0,
                Err(SystemError::ENOTCONN) => {}
                Err(e) => {
                    warn!("virtiofs bridge: read_request failed: {:?}", e);
                    if !self.conn.is_connected() {
                        break;
                    }
                }
            }

            if !self.conn.is_connected() {
                break;
            }
            progressed |= self.submit_pending()?;
            if !self.conn.is_connected() {
                break;
            }

            if let Some(transport) = self.transport.as_mut() {
                if transport.ack_interrupt() {
                    stats::on_virtiofs_ack_interrupt();
                }
            }
            let completed = self.drain_completions()?;
            if !self.conn.is_connected() {
                break;
            }
            if completed != 0 {
                progressed = true;
                progressed |= self.submit_pending()?;
            }
            stats::on_virtiofs_loop_iteration(progressed);

            if !self.conn.is_mounted()
                && !self.conn.has_pending_requests()
                && !self.has_internal_pending()
                && !self.has_inflight()
            {
                break;
            }

            if !progressed {
                self.wait_for_event();
            }
        }

        Ok(())
    }

    fn finish(&mut self) -> bool {
        if self.irq_wake_enabled {
            self.instance.disable_irq_wake();
        }
        self.clear_queue_full_blocked_stats();
        self.instance.clear_bridge_wake(self.session_id);
        if !self.reset_device_and_unset_queues() {
            // Idle pooled buffers are not visible to the device. Release them before the
            // reset-timeout quarantine keeps the transport, queues and inflight DMA buffers alive.
            self.response_pool.clear();
            self.fail_unfinished_preserving_inflight_dma(SystemError::ENOTCONN);
            if !self
                .instance
                .release_session_without_transport(self.session_id)
            {
                warn!(
                    "virtiofs bridge: failed to release quarantined session id={} tag='{}' dev={:?}",
                    self.session_id,
                    self.instance.tag(),
                    self.instance.dev_id()
                );
            }
            return false;
        }
        self.fail_all_unfinished(SystemError::ENOTCONN);

        if let Some(transport) = self.transport.take() {
            self.instance.put_transport_after_session(transport);
        }

        true
    }
}

impl QueueKind {
    fn stats_kind(self) -> stats::VirtioFsQueueKind {
        match self {
            QueueKind::Hiprio => stats::VirtioFsQueueKind::Hiprio,
            QueueKind::Request(_) => stats::VirtioFsQueueKind::Request,
        }
    }
}

fn virtiofs_bridge_thread_entry(arg: usize) -> i32 {
    // SAFETY: `arg` is produced exactly once by `Box::into_raw` in `start_bridge` and the
    // successful thread creation transfers its sole ownership to this entry point.
    let mut ctx = unsafe { Box::from_raw(arg as *mut VirtioFsBridgeContext) };
    let result = ctx.run_loop();
    if let Err(e) = &result {
        warn!("virtiofs bridge thread exit with error: {:?}", e);
    }
    let safe_to_drop = ctx.finish();
    if !safe_to_drop {
        Box::leak(ctx);
    }
    result.map(|_| 0).unwrap_or_else(|e| e.to_posix_errno())
}

pub(super) fn start_bridge(
    instance: Arc<VirtioFsInstance>,
    conn: Arc<FuseConn>,
) -> Result<u64, SystemError> {
    let (mut transport, session_id) = instance.take_transport_for_session()?;
    if instance.request_queue_count() == 0 {
        warn!(
            "virtiofs bridge: no request queues: tag='{}' dev={:?}",
            instance.tag(),
            instance.dev_id(),
        );
        instance.put_transport_after_session(transport);
        return Err(SystemError::EINVAL);
    }

    debug!(
        "virtiofs bridge: start tag='{}' dev={:?} request_queues={}",
        instance.tag(),
        instance.dev_id(),
        instance.num_request_queues()
    );

    transport.set_status(DeviceStatus::empty());
    transport.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER);
    let _device_features = transport.read_device_features();
    transport.write_driver_features(0);
    transport
        .set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER | DeviceStatus::FEATURES_OK);
    let status = transport.get_status();
    if !status.contains(DeviceStatus::FEATURES_OK) {
        warn!(
            "virtiofs bridge: device rejected features tag='{}' dev={:?} status={:?}",
            instance.tag(),
            instance.dev_id(),
            status
        );
        reset_transport_after_start_failure(&instance, session_id, transport);
        return Err(SystemError::ENODEV);
    }
    transport.set_guest_page_size(PAGE_SIZE as u32);

    let (hiprio_vq, hiprio_device_max, hiprio_size) =
        match create_queue(&mut transport, instance.hiprio_queue_index()) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "virtiofs bridge: failed to create hiprio queue tag='{}' dev={:?}: {:?}",
                    instance.tag(),
                    instance.dev_id(),
                    e
                );
                reset_transport_after_start_failure(&instance, session_id, transport);
                return Err(e);
            }
        };

    let mut request_vqs = Vec::with_capacity(instance.request_queue_count());
    let mut request_indices = Vec::with_capacity(instance.request_queue_count());
    let mut request_size_min = usize::MAX;
    let mut request_size_max = 0usize;
    let mut device_queue_depth_max = hiprio_device_max;

    for slot in 0..instance.request_queue_count() {
        let idx = match instance.request_queue_index_by_slot(slot) {
            Some(idx) => idx,
            None => {
                put_transport_after_start_failure(
                    &instance,
                    session_id,
                    transport,
                    hiprio_vq,
                    request_vqs,
                    instance.hiprio_queue_index(),
                    &request_indices,
                );
                return Err(SystemError::EINVAL);
            }
        };

        let (vq, device_max, queue_size) = match create_queue(&mut transport, idx) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "virtiofs bridge: failed to create request queue slot={} idx={} tag='{}' dev={:?}: {:?}",
                    slot,
                    idx,
                    instance.tag(),
                    instance.dev_id(),
                    e
                );
                put_transport_after_start_failure(
                    &instance,
                    session_id,
                    transport,
                    hiprio_vq,
                    request_vqs,
                    instance.hiprio_queue_index(),
                    &request_indices,
                );
                return Err(e);
            }
        };

        request_indices.push(idx);
        request_size_min = core::cmp::min(request_size_min, queue_size);
        request_size_max = core::cmp::max(request_size_max, queue_size);
        device_queue_depth_max = core::cmp::max(device_queue_depth_max, device_max);
        request_vqs.push(vq);
    }

    let Some(sg_limit_pages) = request_size_min.checked_sub(FUSE_HEADER_OVERHEAD) else {
        put_transport_after_start_failure(
            &instance,
            session_id,
            transport,
            hiprio_vq,
            request_vqs,
            instance.hiprio_queue_index(),
            &request_indices,
        );
        return Err(SystemError::EINVAL);
    };
    if sg_limit_pages == 0 {
        put_transport_after_start_failure(
            &instance,
            session_id,
            transport,
            hiprio_vq,
            request_vqs,
            instance.hiprio_queue_index(),
            &request_indices,
        );
        return Err(SystemError::EINVAL);
    }
    let max_pages_limit = core::cmp::min(sg_limit_pages, FUSE_MAX_MAX_PAGES);
    if let Err(e) = conn.set_max_pages_limit(max_pages_limit) {
        put_transport_after_start_failure(
            &instance,
            session_id,
            transport,
            hiprio_vq,
            request_vqs,
            instance.hiprio_queue_index(),
            &request_indices,
        );
        return Err(e);
    }

    stats::on_virtiofs_queue_configured(
        device_queue_depth_max,
        hiprio_size,
        request_vqs.len(),
        request_size_min,
        request_size_max,
        max_pages_limit,
    );

    info!(
        "virtiofs bridge: queue config tag='{}' dev={:?} hiprio_vring={} request_queues={} request_vring_min={} request_vring_max={} sg_limit_pages={}",
        instance.tag(),
        instance.dev_id(),
        hiprio_size,
        request_vqs.len(),
        request_size_min,
        request_size_max,
        max_pages_limit
    );

    transport.finish_init();

    instance.install_bridge_wake(session_id, &conn);
    let irq_wake_enabled = instance.enable_irq_wake();
    let request_queue_count = request_vqs.len();
    let ctx = Box::new(VirtioFsBridgeContext {
        instance,
        conn,
        session_id,
        irq_wake_enabled,
        transport: Some(transport),
        hiprio_vq: Some(hiprio_vq),
        request_pending: core::iter::repeat_with(VecDeque::new)
            .take(request_vqs.len())
            .collect(),
        request_inflight: core::iter::repeat_with(BTreeMap::new)
            .take(request_vqs.len())
            .collect(),
        request_vqs,
        hiprio_pending: VecDeque::new(),
        hiprio_inflight: BTreeMap::new(),
        response_pool: ResponseBufferPool::new(),
        next_request_slot: 0,
        hiprio_blocked: QueueBlockState::default(),
        request_blocked: core::iter::repeat_with(QueueBlockState::default)
            .take(request_queue_count)
            .collect(),
    });

    let raw = Box::into_raw(ctx);
    if KernelThreadMechanism::create_and_run(
        KernelThreadClosure::StaticUsizeClosure((
            &(virtiofs_bridge_thread_entry as fn(usize) -> i32),
            raw as usize,
        )),
        String::from("virtiofs-bridge"),
    )
    .is_none()
    {
        // SAFETY: thread creation returned `None`, so ownership of the pointer produced by
        // `Box::into_raw` was not transferred to a bridge thread and is recovered exactly once.
        let mut ctx = unsafe { Box::from_raw(raw) };
        let safe_to_drop = ctx.finish();
        if !safe_to_drop {
            Box::leak(ctx);
        }
        return Err(SystemError::ENOMEM);
    }

    Ok(session_id)
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use system_error::SystemError;

    use super::{
        QueueBlockState, ResponseBufferPool, VirtioFsBridgeContext, VIRTIOFS_RSP_BUF_SIZE,
        VIRTIOFS_RSP_POOL_MAX_BUFFERS,
    };
    use crate::filesystem::fuse::protocol::FUSE_LOOKUP;

    fn block_state(blocked: bool, completion_seen: bool) -> QueueBlockState {
        QueueBlockState {
            blocked,
            completion_seen,
        }
    }

    #[test]
    fn choose_unblocked_request_slot_skips_blocked_slots() {
        let states = vec![
            block_state(true, false),
            block_state(false, false),
            block_state(false, false),
        ];
        let mut next = 0usize;

        assert!(VirtioFsBridgeContext::has_unblocked_request_slot_in(
            &states,
            states.len()
        ));
        assert_eq!(
            VirtioFsBridgeContext::choose_unblocked_request_slot_in(
                &mut next,
                &states,
                states.len()
            )
            .unwrap(),
            1
        );
        assert_eq!(next, 2);
        assert_eq!(
            VirtioFsBridgeContext::choose_unblocked_request_slot_in(
                &mut next,
                &states,
                states.len()
            )
            .unwrap(),
            2
        );
        assert_eq!(next, 0);
        assert_eq!(
            VirtioFsBridgeContext::choose_unblocked_request_slot_in(
                &mut next,
                &states,
                states.len()
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn completion_seen_does_not_accept_new_ordinary_requests() {
        let states = vec![block_state(true, true), block_state(false, false)];
        let mut next = 0usize;

        assert_eq!(
            VirtioFsBridgeContext::choose_unblocked_request_slot_in(
                &mut next,
                &states,
                states.len()
            )
            .unwrap(),
            1
        );
        assert_eq!(next, 0);
    }

    #[test]
    fn all_blocked_request_slots_are_not_pumpable() {
        let states = vec![block_state(true, false), block_state(true, true)];
        let mut next = 0usize;

        assert!(!VirtioFsBridgeContext::has_unblocked_request_slot_in(
            &states,
            states.len()
        ));
        assert!(matches!(
            VirtioFsBridgeContext::choose_unblocked_request_slot_in(
                &mut next,
                &states,
                states.len()
            ),
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
        ));
        assert_eq!(next, 0);
    }

    #[test]
    fn destroy_barrier_waits_for_every_pre_destroy_queue_domain() {
        assert!(VirtioFsBridgeContext::destroy_barrier_ready_state(
            false, false, false, false, false
        ));
        for blocked_domain in 0..5 {
            let mut state = [false; 5];
            state[blocked_domain] = true;
            assert!(!VirtioFsBridgeContext::destroy_barrier_ready_state(
                state[0], state[1], state[2], state[3], state[4]
            ));
        }
    }

    #[test]
    fn reply_header_must_match_inflight_unique() {
        let header = super::FuseOutHeader {
            len: core::mem::size_of::<super::FuseOutHeader>() as u32,
            error: 0,
            unique: 42,
        };
        assert!(VirtioFsBridgeContext::reply_matches_expected_unique(
            super::fuse_pack_struct(&header),
            42
        ));
        assert!(!VirtioFsBridgeContext::reply_matches_expected_unique(
            super::fuse_pack_struct(&header),
            43
        ));
        assert!(!VirtioFsBridgeContext::reply_matches_expected_unique(
            &[],
            42
        ));
    }

    #[test]
    fn response_pool_reuses_exact_length_and_zeroes_before_submit() {
        let mut pool = ResponseBufferPool::new();
        let mut buf = pool.acquire(FUSE_LOOKUP, 128);
        buf.fill(0xaa);
        let ptr = buf.as_ptr();
        pool.release(buf, true);

        assert_eq!(pool.buffers.len(), 1);
        let reused = pool.acquire(FUSE_LOOKUP, 128);
        assert_eq!(reused.as_ptr(), ptr);
        assert!(reused.iter().all(|byte| *byte == 0));
        assert!(pool.buffers.is_empty());
        assert_eq!(pool.retained_capacity_bytes, 0);
    }

    #[test]
    fn response_pool_best_fit_reuses_larger_capacity_without_exposing_tail() {
        let mut pool = ResponseBufferPool::new();
        let mut large = Vec::with_capacity(256);
        large.resize(192, 0xaa);
        let ptr = large.as_ptr();
        pool.release(large, true);

        let reused = pool.acquire(FUSE_LOOKUP, 64);
        assert_eq!(reused.as_ptr(), ptr);
        assert_eq!(reused.len(), 64);
        assert!(reused.iter().all(|byte| *byte == 0));
        assert_eq!(reused.capacity(), 256);
        assert_eq!(pool.retained_capacity_bytes, 0);
    }

    #[test]
    fn response_pool_zeroes_old_prefix_when_reused_length_grows() {
        let mut pool = ResponseBufferPool::new();
        let mut buf = Vec::with_capacity(256);
        buf.resize(64, 0xaa);
        let ptr = buf.as_ptr();
        pool.release(buf, true);

        let reused = pool.acquire(FUSE_LOOKUP, 128);
        assert_eq!(reused.as_ptr(), ptr);
        assert_eq!(reused.len(), 128);
        assert!(reused.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn response_pool_rejects_excess_capacity_and_nonreusable_buffers() {
        let mut pool = ResponseBufferPool::new();
        let mut oversized_capacity = Vec::with_capacity(VIRTIOFS_RSP_BUF_SIZE + 1);
        oversized_capacity.resize(16, 0);
        pool.release(oversized_capacity, true);
        pool.release(vec![0u8; 16], false);

        assert!(pool.buffers.is_empty());
        assert_eq!(pool.retained_capacity_bytes, 0);
    }

    #[test]
    fn response_pool_enforces_buffer_count_and_checked_capacity() {
        let mut pool = ResponseBufferPool::new();
        for _ in 0..=VIRTIOFS_RSP_POOL_MAX_BUFFERS {
            pool.release(vec![0u8; 1], true);
        }
        assert_eq!(pool.buffers.len(), VIRTIOFS_RSP_POOL_MAX_BUFFERS);

        pool.clear();
        pool.retained_capacity_bytes = usize::MAX;
        pool.release(vec![0u8; 1], true);
        assert!(pool.buffers.is_empty());
        pool.retained_capacity_bytes = 0;
    }
}
