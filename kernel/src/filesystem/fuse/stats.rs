use alloc::{format, string::String};
use core::sync::atomic::{AtomicU64, Ordering};

const BATCH_BUCKETS: usize = 5;

#[derive(Debug, Default, Clone, Copy)]
pub struct FuseStatsSnapshot {
    pub requests_queued_total: u64,
    pub requests_dequeued_total: u64,
    pub requests_replied_ok_total: u64,
    pub requests_replied_err_total: u64,
    pub requests_aborted_total: u64,
    pub requests_dropped_umount_total: u64,
    pub noreply_queued_total: u64,
    pub read_buffer_too_small_total: u64,
    pub bytes_request_to_dev_total: u64,
    pub bytes_reply_payload_cloned_total: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct VirtioFsStatsSnapshot {
    pub bridge_loop_iterations_total: u64,
    pub bridge_progress_iterations_total: u64,
    pub bridge_idle_sleeps_total: u64,
    pub bridge_poll_sleep_ns_total: u64,
    pub bridge_ack_interrupt_total: u64,
    pub bridge_pumped_requests_total: u64,
    pub bridge_submitted_total: u64,
    pub bridge_completed_total: u64,
    pub bridge_noreply_completed_total: u64,
    pub bridge_fail_unfinished_total: u64,
    pub virtqueue_full_total: u64,
    pub virtqueue_not_ready_total: u64,
    pub submit_error_total: u64,
    pub pop_used_error_total: u64,
    pub bridge_request_clone_count: u64,
    pub bridge_request_clone_bytes: u64,
    pub response_buffer_alloc_count: u64,
    pub response_buffer_alloc_bytes: u64,
    pub response_buffer_waste_bytes: u64,
    pub bytes_submitted_total: u64,
    pub bytes_completed_total: u64,
    pub pump_batch: [u64; BATCH_BUCKETS],
    pub complete_batch: [u64; BATCH_BUCKETS],
}

static REQUESTS_QUEUED_TOTAL: AtomicU64 = AtomicU64::new(0);
static REQUESTS_DEQUEUED_TOTAL: AtomicU64 = AtomicU64::new(0);
static REQUESTS_REPLIED_OK_TOTAL: AtomicU64 = AtomicU64::new(0);
static REQUESTS_REPLIED_ERR_TOTAL: AtomicU64 = AtomicU64::new(0);
static REQUESTS_ABORTED_TOTAL: AtomicU64 = AtomicU64::new(0);
static REQUESTS_DROPPED_UMOUNT_TOTAL: AtomicU64 = AtomicU64::new(0);
static NOREPLY_QUEUED_TOTAL: AtomicU64 = AtomicU64::new(0);
static READ_BUFFER_TOO_SMALL_TOTAL: AtomicU64 = AtomicU64::new(0);
static BYTES_REQUEST_TO_DEV_TOTAL: AtomicU64 = AtomicU64::new(0);
static BYTES_REPLY_PAYLOAD_CLONED_TOTAL: AtomicU64 = AtomicU64::new(0);

static BRIDGE_LOOP_ITERATIONS_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_PROGRESS_ITERATIONS_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_IDLE_SLEEPS_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_POLL_SLEEP_NS_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_ACK_INTERRUPT_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_PUMPED_REQUESTS_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_SUBMITTED_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_COMPLETED_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_NOREPLY_COMPLETED_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_FAIL_UNFINISHED_TOTAL: AtomicU64 = AtomicU64::new(0);
static VIRTQUEUE_FULL_TOTAL: AtomicU64 = AtomicU64::new(0);
static VIRTQUEUE_NOT_READY_TOTAL: AtomicU64 = AtomicU64::new(0);
static SUBMIT_ERROR_TOTAL: AtomicU64 = AtomicU64::new(0);
static POP_USED_ERROR_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_REQUEST_CLONE_COUNT: AtomicU64 = AtomicU64::new(0);
static BRIDGE_REQUEST_CLONE_BYTES: AtomicU64 = AtomicU64::new(0);
static RESPONSE_BUFFER_ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static RESPONSE_BUFFER_ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static RESPONSE_BUFFER_WASTE_BYTES: AtomicU64 = AtomicU64::new(0);
static BYTES_SUBMITTED_TOTAL: AtomicU64 = AtomicU64::new(0);
static BYTES_COMPLETED_TOTAL: AtomicU64 = AtomicU64::new(0);

static PUMP_BATCH: [AtomicU64; BATCH_BUCKETS] = [const { AtomicU64::new(0) }; BATCH_BUCKETS];
static COMPLETE_BATCH: [AtomicU64; BATCH_BUCKETS] = [const { AtomicU64::new(0) }; BATCH_BUCKETS];

#[inline]
fn add(counter: &AtomicU64, value: u64) {
    counter.fetch_add(value, Ordering::Relaxed);
}

#[inline]
fn inc(counter: &AtomicU64) {
    add(counter, 1);
}

#[inline]
fn batch_bucket(count: usize) -> usize {
    match count {
        0 => 0,
        1 => 1,
        2..=4 => 2,
        5..=16 => 3,
        _ => 4,
    }
}

#[inline]
fn record_batch(buckets: &[AtomicU64; BATCH_BUCKETS], count: usize) {
    inc(&buckets[batch_bucket(count)]);
}

fn snapshot_batch(buckets: &[AtomicU64; BATCH_BUCKETS]) -> [u64; BATCH_BUCKETS] {
    [
        buckets[0].load(Ordering::Relaxed),
        buckets[1].load(Ordering::Relaxed),
        buckets[2].load(Ordering::Relaxed),
        buckets[3].load(Ordering::Relaxed),
        buckets[4].load(Ordering::Relaxed),
    ]
}

#[inline]
pub fn on_fuse_request_queued(_len: usize, no_reply: bool) {
    inc(&REQUESTS_QUEUED_TOTAL);
    if no_reply {
        inc(&NOREPLY_QUEUED_TOTAL);
    }
}

#[inline]
pub fn on_fuse_request_dequeued(len: usize) {
    inc(&REQUESTS_DEQUEUED_TOTAL);
    add(&BYTES_REQUEST_TO_DEV_TOTAL, len as u64);
}

#[inline]
pub fn on_fuse_read_buffer_too_small() {
    inc(&READ_BUFFER_TOO_SMALL_TOTAL);
}

#[inline]
pub fn on_fuse_reply_complete(error: i32, payload_len: usize) {
    if error == 0 {
        inc(&REQUESTS_REPLIED_OK_TOTAL);
        add(&BYTES_REPLY_PAYLOAD_CLONED_TOTAL, payload_len as u64);
    } else {
        inc(&REQUESTS_REPLIED_ERR_TOTAL);
    }
}

#[inline]
pub fn on_fuse_requests_aborted(count: usize) {
    add(&REQUESTS_ABORTED_TOTAL, count as u64);
}

#[inline]
pub fn on_fuse_requests_dropped_umount(count: usize) {
    add(&REQUESTS_DROPPED_UMOUNT_TOTAL, count as u64);
}

#[inline]
pub fn on_virtiofs_loop_iteration(progressed: bool) {
    inc(&BRIDGE_LOOP_ITERATIONS_TOTAL);
    if progressed {
        inc(&BRIDGE_PROGRESS_ITERATIONS_TOTAL);
    }
}

#[inline]
pub fn on_virtiofs_idle_sleep(ns: i64) {
    inc(&BRIDGE_IDLE_SLEEPS_TOTAL);
    if ns > 0 {
        add(&BRIDGE_POLL_SLEEP_NS_TOTAL, ns as u64);
    }
}

#[inline]
pub fn on_virtiofs_ack_interrupt() {
    inc(&BRIDGE_ACK_INTERRUPT_TOTAL);
}

#[inline]
pub fn on_virtiofs_pump_batch(count: usize) {
    add(&BRIDGE_PUMPED_REQUESTS_TOTAL, count as u64);
    record_batch(&PUMP_BATCH, count);
}

#[inline]
pub fn on_virtiofs_complete_batch(count: usize) {
    record_batch(&COMPLETE_BATCH, count);
}

#[inline]
pub fn on_virtiofs_request_cloned(len: usize) {
    inc(&BRIDGE_REQUEST_CLONE_COUNT);
    add(&BRIDGE_REQUEST_CLONE_BYTES, len as u64);
}

#[inline]
pub fn on_virtiofs_response_buffer_alloc(len: usize) {
    inc(&RESPONSE_BUFFER_ALLOC_COUNT);
    add(&RESPONSE_BUFFER_ALLOC_BYTES, len as u64);
}

#[inline]
pub fn on_virtiofs_response_buffer_waste(len: usize) {
    add(&RESPONSE_BUFFER_WASTE_BYTES, len as u64);
}

#[inline]
pub fn on_virtiofs_submitted(req_len: usize) {
    inc(&BRIDGE_SUBMITTED_TOTAL);
    add(&BYTES_SUBMITTED_TOTAL, req_len as u64);
}

#[inline]
pub fn on_virtiofs_queue_full() {
    inc(&VIRTQUEUE_FULL_TOTAL);
}

#[inline]
pub fn on_virtiofs_not_ready() {
    inc(&VIRTQUEUE_NOT_READY_TOTAL);
}

#[inline]
pub fn on_virtiofs_submit_error() {
    inc(&SUBMIT_ERROR_TOTAL);
}

#[inline]
pub fn on_virtiofs_pop_used_error() {
    inc(&POP_USED_ERROR_TOTAL);
}

#[inline]
pub fn on_virtiofs_completed(used_len: usize, noreply: bool) {
    inc(&BRIDGE_COMPLETED_TOTAL);
    if noreply {
        inc(&BRIDGE_NOREPLY_COMPLETED_TOTAL);
    }
    add(&BYTES_COMPLETED_TOTAL, used_len as u64);
}

#[inline]
pub fn on_virtiofs_fail_unfinished(count: usize) {
    add(&BRIDGE_FAIL_UNFINISHED_TOTAL, count as u64);
}

pub fn fuse_snapshot() -> FuseStatsSnapshot {
    FuseStatsSnapshot {
        requests_queued_total: REQUESTS_QUEUED_TOTAL.load(Ordering::Relaxed),
        requests_dequeued_total: REQUESTS_DEQUEUED_TOTAL.load(Ordering::Relaxed),
        requests_replied_ok_total: REQUESTS_REPLIED_OK_TOTAL.load(Ordering::Relaxed),
        requests_replied_err_total: REQUESTS_REPLIED_ERR_TOTAL.load(Ordering::Relaxed),
        requests_aborted_total: REQUESTS_ABORTED_TOTAL.load(Ordering::Relaxed),
        requests_dropped_umount_total: REQUESTS_DROPPED_UMOUNT_TOTAL.load(Ordering::Relaxed),
        noreply_queued_total: NOREPLY_QUEUED_TOTAL.load(Ordering::Relaxed),
        read_buffer_too_small_total: READ_BUFFER_TOO_SMALL_TOTAL.load(Ordering::Relaxed),
        bytes_request_to_dev_total: BYTES_REQUEST_TO_DEV_TOTAL.load(Ordering::Relaxed),
        bytes_reply_payload_cloned_total: BYTES_REPLY_PAYLOAD_CLONED_TOTAL.load(Ordering::Relaxed),
    }
}

pub fn virtiofs_snapshot() -> VirtioFsStatsSnapshot {
    VirtioFsStatsSnapshot {
        bridge_loop_iterations_total: BRIDGE_LOOP_ITERATIONS_TOTAL.load(Ordering::Relaxed),
        bridge_progress_iterations_total: BRIDGE_PROGRESS_ITERATIONS_TOTAL.load(Ordering::Relaxed),
        bridge_idle_sleeps_total: BRIDGE_IDLE_SLEEPS_TOTAL.load(Ordering::Relaxed),
        bridge_poll_sleep_ns_total: BRIDGE_POLL_SLEEP_NS_TOTAL.load(Ordering::Relaxed),
        bridge_ack_interrupt_total: BRIDGE_ACK_INTERRUPT_TOTAL.load(Ordering::Relaxed),
        bridge_pumped_requests_total: BRIDGE_PUMPED_REQUESTS_TOTAL.load(Ordering::Relaxed),
        bridge_submitted_total: BRIDGE_SUBMITTED_TOTAL.load(Ordering::Relaxed),
        bridge_completed_total: BRIDGE_COMPLETED_TOTAL.load(Ordering::Relaxed),
        bridge_noreply_completed_total: BRIDGE_NOREPLY_COMPLETED_TOTAL.load(Ordering::Relaxed),
        bridge_fail_unfinished_total: BRIDGE_FAIL_UNFINISHED_TOTAL.load(Ordering::Relaxed),
        virtqueue_full_total: VIRTQUEUE_FULL_TOTAL.load(Ordering::Relaxed),
        virtqueue_not_ready_total: VIRTQUEUE_NOT_READY_TOTAL.load(Ordering::Relaxed),
        submit_error_total: SUBMIT_ERROR_TOTAL.load(Ordering::Relaxed),
        pop_used_error_total: POP_USED_ERROR_TOTAL.load(Ordering::Relaxed),
        bridge_request_clone_count: BRIDGE_REQUEST_CLONE_COUNT.load(Ordering::Relaxed),
        bridge_request_clone_bytes: BRIDGE_REQUEST_CLONE_BYTES.load(Ordering::Relaxed),
        response_buffer_alloc_count: RESPONSE_BUFFER_ALLOC_COUNT.load(Ordering::Relaxed),
        response_buffer_alloc_bytes: RESPONSE_BUFFER_ALLOC_BYTES.load(Ordering::Relaxed),
        response_buffer_waste_bytes: RESPONSE_BUFFER_WASTE_BYTES.load(Ordering::Relaxed),
        bytes_submitted_total: BYTES_SUBMITTED_TOTAL.load(Ordering::Relaxed),
        bytes_completed_total: BYTES_COMPLETED_TOTAL.load(Ordering::Relaxed),
        pump_batch: snapshot_batch(&PUMP_BATCH),
        complete_batch: snapshot_batch(&COMPLETE_BATCH),
    }
}

pub fn format_snapshot() -> String {
    let fuse = fuse_snapshot();
    let virtiofs = virtiofs_snapshot();
    format!(
        "[fuse]\n\
requests_queued_total {}\n\
requests_dequeued_total {}\n\
requests_replied_ok_total {}\n\
requests_replied_err_total {}\n\
requests_aborted_total {}\n\
requests_dropped_umount_total {}\n\
noreply_queued_total {}\n\
read_buffer_too_small_total {}\n\
bytes_request_to_dev_total {}\n\
bytes_reply_payload_cloned_total {}\n\
\n\
[virtiofs]\n\
bridge_loop_iterations_total {}\n\
bridge_progress_iterations_total {}\n\
bridge_idle_sleeps_total {}\n\
bridge_poll_sleep_ns_total {}\n\
bridge_ack_interrupt_total {}\n\
bridge_pumped_requests_total {}\n\
bridge_submitted_total {}\n\
bridge_completed_total {}\n\
bridge_noreply_completed_total {}\n\
bridge_fail_unfinished_total {}\n\
virtqueue_full_total {}\n\
virtqueue_not_ready_total {}\n\
submit_error_total {}\n\
pop_used_error_total {}\n\
bridge_request_clone_count {}\n\
bridge_request_clone_bytes {}\n\
response_buffer_alloc_count {}\n\
response_buffer_alloc_bytes {}\n\
response_buffer_waste_bytes {}\n\
bytes_submitted_total {}\n\
bytes_completed_total {}\n\
pump_batch_0 {}\n\
pump_batch_1 {}\n\
pump_batch_2_4 {}\n\
pump_batch_5_16 {}\n\
pump_batch_gt_16 {}\n\
complete_batch_0 {}\n\
complete_batch_1 {}\n\
complete_batch_2_4 {}\n\
complete_batch_5_16 {}\n\
complete_batch_gt_16 {}\n",
        fuse.requests_queued_total,
        fuse.requests_dequeued_total,
        fuse.requests_replied_ok_total,
        fuse.requests_replied_err_total,
        fuse.requests_aborted_total,
        fuse.requests_dropped_umount_total,
        fuse.noreply_queued_total,
        fuse.read_buffer_too_small_total,
        fuse.bytes_request_to_dev_total,
        fuse.bytes_reply_payload_cloned_total,
        virtiofs.bridge_loop_iterations_total,
        virtiofs.bridge_progress_iterations_total,
        virtiofs.bridge_idle_sleeps_total,
        virtiofs.bridge_poll_sleep_ns_total,
        virtiofs.bridge_ack_interrupt_total,
        virtiofs.bridge_pumped_requests_total,
        virtiofs.bridge_submitted_total,
        virtiofs.bridge_completed_total,
        virtiofs.bridge_noreply_completed_total,
        virtiofs.bridge_fail_unfinished_total,
        virtiofs.virtqueue_full_total,
        virtiofs.virtqueue_not_ready_total,
        virtiofs.submit_error_total,
        virtiofs.pop_used_error_total,
        virtiofs.bridge_request_clone_count,
        virtiofs.bridge_request_clone_bytes,
        virtiofs.response_buffer_alloc_count,
        virtiofs.response_buffer_alloc_bytes,
        virtiofs.response_buffer_waste_bytes,
        virtiofs.bytes_submitted_total,
        virtiofs.bytes_completed_total,
        virtiofs.pump_batch[0],
        virtiofs.pump_batch[1],
        virtiofs.pump_batch[2],
        virtiofs.pump_batch[3],
        virtiofs.pump_batch[4],
        virtiofs.complete_batch[0],
        virtiofs.complete_batch[1],
        virtiofs.complete_batch[2],
        virtiofs.complete_batch[3],
        virtiofs.complete_batch[4],
    )
}
