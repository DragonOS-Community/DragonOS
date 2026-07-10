use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    string::String,
    sync::Arc,
    vec,
    vec::Vec,
};
use core::hint::spin_loop;

use linkme::distributed_slice;
use log::{debug, info, warn};
use system_error::SystemError;
use virtio_drivers::{
    queue::VirtQueue,
    transport::{DeviceStatus, Transport},
    Error as VirtioError, Result as VirtioResult, PAGE_SIZE,
};

use crate::{
    driver::virtio::{
        transport::VirtIOTransport,
        virtio_drivers_error_to_system_error,
        virtio_fs::{virtio_fs_find_instance, VirtioFsInstance},
        virtio_impl::HalImpl,
    },
    filesystem::vfs::{
        file::File, FileSystem, FileSystemMakerData, FsInfo, IndexNode, MountableFileSystem,
        SuperBlock, FSMAKER,
    },
    mm::{fault::PageFaultMessage, VirtRegion, VmFaultReason, VmFlags},
    process::{kthread::KernelThreadClosure, kthread::KernelThreadMechanism, ProcessManager},
    register_mountable_fs,
    time::{sleep::nanosleep, PosixTimeSpec},
};

use super::{
    conn::FuseConn,
    fs::{FuseFS, FuseMountData},
    protocol::{
        fuse_pack_struct, fuse_read_struct, FuseInHeader, FuseOutHeader, FuseReadIn, FUSE_DESTROY,
        FUSE_FORGET, FUSE_INTERRUPT, FUSE_READ, FUSE_READDIR, FUSE_READDIRPLUS,
    },
    stats, trace,
};

const VIRTIOFS_QUEUE_SIZE_MIN: usize = 8;
const VIRTIOFS_QUEUE_SIZE_MAX: usize = 1024;
const VIRTIOFS_RESET_WAIT_SPINS: usize = 100_000;
const FUSE_HEADER_OVERHEAD: usize = 4;
const FUSE_MAX_MAX_PAGES: usize = 256;
const VIRTIOFS_REQ_BUF_SIZE: usize = 256 * 1024;
const VIRTIOFS_RSP_BUF_SIZE: usize = 256 * 1024;
const VIRTIOFS_POLL_NS: i64 = 1_000_000;
const VIRTIOFS_PUMP_BUDGET: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueKind {
    Hiprio,
    Request(usize),
}

#[derive(Debug)]
struct PendingReq {
    req: Vec<u8>,
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

enum VirtioFsQueue {
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
    fn can_pop(&self) -> bool {
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

    fn peek_used(&self) -> Option<u16> {
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

    fn should_notify(&self) -> bool {
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

    unsafe fn add<'a, 'b>(
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

    unsafe fn pop_used<'a>(
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

    unsafe fn detach_unused<'a>(
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

fn create_queue(
    transport: &mut VirtIOTransport,
    idx: u16,
) -> Result<(VirtioFsQueue, usize, usize), SystemError> {
    let device_max = transport.max_queue_size(idx) as usize;
    let size = choose_queue_size(device_max as u32)?;
    let queue = create_queue_with_size(transport, idx, size)
        .map_err(virtio_drivers_error_to_system_error)?;
    Ok((queue, device_max, size))
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

fn wait_transport_reset_complete(transport: &VirtIOTransport) -> bool {
    for _ in 0..VIRTIOFS_RESET_WAIT_SPINS {
        if transport.get_status().is_empty() {
            return true;
        }
        spin_loop();
    }
    false
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
    next_request_slot: usize,
    req_buf: Vec<u8>,
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

    fn response_buffer_size(pending: &PendingReq) -> usize {
        let default_size = VIRTIOFS_RSP_BUF_SIZE;
        let read_like = matches!(pending.opcode, FUSE_READ | FUSE_READDIR | FUSE_READDIRPLUS);
        if !read_like || pending.req.len() < core::mem::size_of::<FuseInHeader>() {
            return default_size;
        }

        let payload = &pending.req[core::mem::size_of::<FuseInHeader>()..];
        let Ok(read_in) = fuse_read_struct::<FuseReadIn>(payload) else {
            return default_size;
        };
        let wanted = core::mem::size_of::<FuseOutHeader>().saturating_add(read_in.size as usize);
        core::cmp::max(core::mem::size_of::<FuseOutHeader>(), wanted).min(default_size)
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

            let len = match self.conn.read_ordinary_request(true, &mut self.req_buf) {
                Ok(len) => len,
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => break,
                Err(e) => return Err(e),
            };
            let req = self.req_buf[..len].to_vec();
            stats::on_virtiofs_request_cloned(req.len());
            let in_hdr: FuseInHeader = fuse_read_struct(&req)?;
            let noreply = matches!(in_hdr.opcode, FUSE_FORGET | FUSE_DESTROY);
            debug_assert!(!matches!(in_hdr.opcode, FUSE_FORGET | FUSE_INTERRUPT));
            self.push_pending_back(PendingReq {
                req,
                unique: in_hdr.unique,
                opcode: in_hdr.opcode,
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
            let len = match self.conn.read_high_priority_request(&mut self.req_buf) {
                Ok(len) => len,
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => break,
                Err(e) => return Err(e),
            };
            let req = self.req_buf[..len].to_vec();
            stats::on_virtiofs_request_cloned(req.len());
            let in_hdr: FuseInHeader = fuse_read_struct(&req)?;
            debug_assert!(matches!(in_hdr.opcode, FUSE_FORGET | FUSE_INTERRUPT));
            self.push_pending_back(PendingReq {
                req,
                unique: in_hdr.unique,
                opcode: in_hdr.opcode,
                noreply: in_hdr.opcode == FUSE_FORGET,
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
        let queue_idx = self.queue_index(kind)?;
        let mut rsp = if pending.noreply {
            None
        } else {
            let rsp_size = Self::response_buffer_size(&pending);
            stats::on_virtiofs_response_buffer_alloc(rsp_size);
            Some(vec![0u8; rsp_size])
        };

        let (token, should_notify) = match kind {
            QueueKind::Hiprio => {
                let queue = self.hiprio_vq.as_mut().ok_or(SystemError::EIO)?;
                let token = if let Some(rsp_buf) = rsp.as_mut() {
                    let inputs = [pending.req.as_slice()];
                    let mut outputs = [rsp_buf.as_mut_slice()];
                    unsafe { queue.add(&inputs, &mut outputs) }
                } else {
                    let inputs = [pending.req.as_slice()];
                    let mut outputs: [&mut [u8]; 0] = [];
                    unsafe { queue.add(&inputs, &mut outputs) }
                };
                (token, queue.should_notify())
            }
            QueueKind::Request(slot) => {
                let queue = self.request_vqs.get_mut(slot).ok_or(SystemError::EINVAL)?;
                let token = if let Some(rsp_buf) = rsp.as_mut() {
                    let inputs = [pending.req.as_slice()];
                    let mut outputs = [rsp_buf.as_mut_slice()];
                    unsafe { queue.add(&inputs, &mut outputs) }
                } else {
                    let inputs = [pending.req.as_slice()];
                    let mut outputs: [&mut [u8]; 0] = [];
                    unsafe { queue.add(&inputs, &mut outputs) }
                };
                (token, queue.should_notify())
            }
        };

        let token = match token {
            Ok(token) => token,
            Err(VirtioError::QueueFull) => {
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
                stats::on_virtiofs_not_ready();
                stats::on_virtiofs_submit_error();
                let se = virtio_drivers_error_to_system_error(VirtioError::NotReady);
                warn!(
                    "virtiofs bridge: queue not ready opcode={} unique={} queue={:?} err={:?}",
                    pending.opcode, pending.unique, kind, se
                );
                if !pending.noreply {
                    self.complete_request_with_error(pending.unique, se);
                }
                return Ok(true);
            }
            Err(e) => {
                stats::on_virtiofs_submit_error();
                let se = virtio_drivers_error_to_system_error(e);
                warn!(
                    "virtiofs bridge: submit failed opcode={} unique={} queue={:?} err={:?}",
                    pending.opcode, pending.unique, kind, se
                );
                if !pending.noreply {
                    self.complete_request_with_error(pending.unique, se);
                }
                return Ok(true);
            }
        };

        let (queue_kind, queue_slot) = Self::trace_queue_fields(kind);
        let trace_unique = pending.unique;
        let trace_opcode = pending.opcode;
        let req_len = pending.req.len();
        let inflight = InflightReq { pending, rsp };
        let replaced = match kind {
            QueueKind::Hiprio => self.hiprio_inflight.insert(token, inflight),
            QueueKind::Request(slot) => self
                .request_inflight
                .get_mut(slot)
                .ok_or(SystemError::EINVAL)?
                .insert(token, inflight),
        };
        debug_assert!(replaced.is_none());
        stats::on_virtiofs_inflight_add(kind.stats_kind());
        stats::on_virtiofs_submitted(req_len);

        let retry_succeeded = {
            let state = self.block_state_mut(kind)?;
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
                .ok_or(SystemError::EIO)?
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

        for slot in 0..self.request_vqs.len() {
            while self.submit_one_pending(QueueKind::Request(slot))? {
                progressed = true;
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
                let inputs = [inflight.pending.req.as_slice()];
                if let Some(rsp_buf) = inflight.rsp.as_mut() {
                    let mut outputs = [rsp_buf.as_mut_slice()];
                    unsafe { queue.pop_used(token, &inputs, &mut outputs) }
                        .map_err(virtio_drivers_error_to_system_error)
                } else {
                    let mut outputs: [&mut [u8]; 0] = [];
                    unsafe { queue.pop_used(token, &inputs, &mut outputs) }
                        .map_err(virtio_drivers_error_to_system_error)
                }
            }
            QueueKind::Request(slot) => {
                let queue = self.request_vqs.get_mut(slot).ok_or(SystemError::EINVAL)?;
                let inputs = [inflight.pending.req.as_slice()];
                if let Some(rsp_buf) = inflight.rsp.as_mut() {
                    let mut outputs = [rsp_buf.as_mut_slice()];
                    unsafe { queue.pop_used(token, &inputs, &mut outputs) }
                        .map_err(virtio_drivers_error_to_system_error)
                } else {
                    let mut outputs: [&mut [u8]; 0] = [];
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

        let rsp_buf = inflight.rsp.as_ref().ok_or(SystemError::EIO)?;
        if used_len > rsp_buf.len() {
            stats::on_virtiofs_completed(used_len, false);
            self.complete_request_with_error(inflight.pending.unique, SystemError::EIO);
            return Ok(true);
        }
        if rsp_buf.len() > used_len {
            stats::on_virtiofs_response_buffer_waste(rsp_buf.len() - used_len);
        }
        stats::on_virtiofs_completed(used_len, false);

        match self.conn.write_reply(&rsp_buf[..used_len]) {
            Ok(_) | Err(SystemError::ENOENT) => {}
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
                self.complete_request_with_error(unique, completion_err);
            }
        }
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
        if !conn.is_connected() && !self.has_inflight() {
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
        if disconnect_event && !self.has_inflight() {
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
            let inputs = [req.pending.req.as_slice()];
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

            progressed |= self.submit_pending()?;

            if let Some(transport) = self.transport.as_mut() {
                if transport.ack_interrupt() {
                    stats::on_virtiofs_ack_interrupt();
                }
            }
            let completed = self.drain_completions()?;
            if completed != 0 {
                progressed = true;
                progressed |= self.submit_pending()?;
            }
            stats::on_virtiofs_loop_iteration(progressed);

            if !self.conn.is_connected() && !self.has_inflight() {
                break;
            }

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

fn start_bridge(instance: Arc<VirtioFsInstance>, conn: Arc<FuseConn>) -> Result<u64, SystemError> {
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
        next_request_slot: 0,
        req_buf: vec![0u8; VIRTIOFS_REQ_BUF_SIZE],
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
        let mut ctx = unsafe { Box::from_raw(raw) };
        let safe_to_drop = ctx.finish();
        if !safe_to_drop {
            Box::leak(ctx);
        }
        return Err(SystemError::ENOMEM);
    }

    Ok(session_id)
}

#[derive(Debug)]
struct VirtioFsMountData {
    rootmode: u32,
    user_id: u32,
    group_id: u32,
    allow_other: bool,
    default_permissions: bool,
    dax_mode: VirtioFsDaxMode,
    conn: Arc<FuseConn>,
    instance: Arc<VirtioFsInstance>,
}

impl FileSystemMakerData for VirtioFsMountData {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

#[derive(Debug)]
struct VirtioFsFs {
    inner: Arc<dyn FileSystem>,
    instance: Arc<VirtioFsInstance>,
    session_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VirtioFsDaxMode {
    Never,
    Always,
    Inode,
}

impl VirtioFsFs {
    fn parse_opt_u32_decimal(v: &str) -> Result<u32, SystemError> {
        v.parse::<u32>().map_err(|_| SystemError::EINVAL)
    }

    fn parse_opt_u32_octal(v: &str) -> Result<u32, SystemError> {
        u32::from_str_radix(v, 8).map_err(|_| SystemError::EINVAL)
    }

    fn parse_opt_bool_switch(v: &str) -> bool {
        v.is_empty() || v != "0"
    }

    fn parse_dax_mode(v: &str) -> Result<VirtioFsDaxMode, SystemError> {
        if v.is_empty() {
            return Ok(VirtioFsDaxMode::Always);
        }

        match v {
            "always" => Ok(VirtioFsDaxMode::Always),
            "never" => Ok(VirtioFsDaxMode::Never),
            "inode" => Ok(VirtioFsDaxMode::Inode),
            _ => Err(SystemError::EINVAL),
        }
    }

    fn parse_mount_options(
        raw: Option<&str>,
    ) -> Result<(u32, u32, u32, bool, bool, VirtioFsDaxMode), SystemError> {
        let pcb = ProcessManager::current_pcb();
        let cred = pcb.cred();

        let mut rootmode: Option<u32> = None;
        let mut user_id: Option<u32> = None;
        let mut group_id: Option<u32> = None;
        let mut default_permissions = true;
        let mut allow_other = true;
        let mut dax_mode = VirtioFsDaxMode::Never;

        for part in raw.unwrap_or("").split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (k, v) = match part.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => (part, ""),
            };

            match k {
                "rootmode" => rootmode = Some(Self::parse_opt_u32_octal(v)?),
                "user_id" => user_id = Some(Self::parse_opt_u32_decimal(v)?),
                "group_id" => group_id = Some(Self::parse_opt_u32_decimal(v)?),
                "default_permissions" => default_permissions = Self::parse_opt_bool_switch(v),
                "allow_other" => allow_other = Self::parse_opt_bool_switch(v),
                "dax" => dax_mode = Self::parse_dax_mode(v)?,
                _ => return Err(SystemError::EINVAL),
            }
        }

        if dax_mode != VirtioFsDaxMode::Never {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }

        Ok((
            rootmode.unwrap_or(0o040755),
            user_id.unwrap_or(cred.fsuid.data() as u32),
            group_id.unwrap_or(cred.fsgid.data() as u32),
            default_permissions,
            allow_other,
            dax_mode,
        ))
    }
}

impl FileSystem for VirtioFsFs {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.inner.root_inode()
    }

    fn info(&self) -> FsInfo {
        self.inner.info()
    }

    fn support_readahead(&self) -> bool {
        self.inner.support_readahead()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn name(&self) -> &str {
        "virtiofs"
    }

    fn super_block(&self) -> SuperBlock {
        self.inner.super_block()
    }

    fn statfs(&self, inode: &Arc<dyn IndexNode>) -> Result<SuperBlock, SystemError> {
        self.inner.statfs(inode)
    }

    fn permission_policy(&self) -> crate::filesystem::vfs::FsPermissionPolicy {
        self.inner.permission_policy()
    }

    unsafe fn fault(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        self.inner.fault(pfm)
    }

    unsafe fn page_mkwrite(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        self.inner.page_mkwrite(pfm)
    }

    fn mprotect(&self, old_vm_flags: VmFlags, new_vm_flags: VmFlags) -> Result<(), SystemError> {
        self.inner.mprotect(old_vm_flags, new_vm_flags)
    }

    fn vma_close(&self, file: &Arc<File>, region: VirtRegion, vm_flags: VmFlags) {
        self.inner.vma_close(file, region, vm_flags)
    }

    unsafe fn map_pages(
        &self,
        pfm: &mut PageFaultMessage,
        start_pgoff: usize,
        end_pgoff: usize,
    ) -> VmFaultReason {
        self.inner.map_pages(pfm, start_pgoff, end_pgoff)
    }

    fn on_umount(&self) {
        self.inner.on_umount();
        self.instance.wait_session_released(self.session_id);
    }
}

impl MountableFileSystem for VirtioFsFs {
    fn make_mount_data(
        raw_data: Option<&str>,
        source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        if source.is_empty() {
            return Err(SystemError::EINVAL);
        }

        let (rootmode, user_id, group_id, default_permissions, allow_other, dax_mode) =
            Self::parse_mount_options(raw_data)?;
        let instance = virtio_fs_find_instance(source).ok_or(SystemError::ENODEV)?;
        let conn = FuseConn::new_for_virtiofs(core::cmp::min(
            VIRTIOFS_REQ_BUF_SIZE,
            VIRTIOFS_RSP_BUF_SIZE,
        ));

        Ok(Some(Arc::new(VirtioFsMountData {
            rootmode,
            user_id,
            group_id,
            allow_other,
            default_permissions,
            dax_mode,
            conn,
            instance,
        })))
    }

    fn make_fs(
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let md = data
            .and_then(|d| d.as_any().downcast_ref::<VirtioFsMountData>())
            .ok_or(SystemError::EINVAL)?;

        let fuse_mount_data = FuseMountData {
            rootmode: md.rootmode,
            user_id: md.user_id,
            group_id: md.group_id,
            max_read: VIRTIOFS_RSP_BUF_SIZE
                .saturating_sub(core::mem::size_of::<FuseOutHeader>())
                .min(u32::MAX as usize) as u32,
            allow_other: md.allow_other,
            default_permissions: md.default_permissions,
            conn: md.conn.clone(),
        };

        if md.dax_mode != VirtioFsDaxMode::Never {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }

        let inner = <FuseFS as MountableFileSystem>::make_fs(Some(
            &fuse_mount_data as &dyn FileSystemMakerData,
        ))?;

        let session_id = match start_bridge(md.instance.clone(), md.conn.clone()) {
            Ok(id) => id,
            Err(e) => {
                inner.on_umount();
                return Err(e);
            }
        };

        Ok(Arc::new(VirtioFsFs {
            inner,
            instance: md.instance.clone(),
            session_id,
        }))
    }
}

register_mountable_fs!(VirtioFsFs, VIRTIOFSMAKER, "virtiofs");

#[cfg(test)]
mod tests {
    use alloc::vec;

    use system_error::SystemError;

    use super::{choose_queue_size, QueueBlockState, VirtioFsBridgeContext};

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
    fn choose_queue_size_uses_supported_power_of_two() {
        assert_eq!(choose_queue_size(8).unwrap(), 8);
        assert_eq!(choose_queue_size(15).unwrap(), 8);
        assert_eq!(choose_queue_size(128).unwrap(), 128);
        assert_eq!(choose_queue_size(2048).unwrap(), 1024);
        assert_eq!(choose_queue_size(7), Err(SystemError::EINVAL));
    }
}
