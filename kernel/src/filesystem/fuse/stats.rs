use crate::mm::page_cache_stats;
use alloc::{format, string::String};
use core::{
    fmt::Write,
    sync::atomic::{AtomicU64, AtomicU8, Ordering},
};

const BATCH_BUCKETS: usize = 5;
const READ_PAGE_BUCKETS: usize = 6;
const OPCODE_BUCKETS: usize = 64;
const OPCODE_OVERFLOW_BUCKET: usize = OPCODE_BUCKETS - 1;

/// Controls only optional hot-path detail. Lifecycle and direct-DMA conservation
/// counters below are always-on, so changing this mode cannot split an owner's
/// acquire/release accounting across observation epochs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FuseStatsMode {
    Off = 0,
    Light = 1,
    Detailed = 2,
}

impl FuseStatsMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Light => "light",
            Self::Detailed => "detailed",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ()> {
        match value.trim() {
            "off" => Ok(Self::Off),
            "light" => Ok(Self::Light),
            "detailed" => Ok(Self::Detailed),
            _ => Err(()),
        }
    }
}

static STATS_MODE: AtomicU8 = AtomicU8::new(FuseStatsMode::Off as u8);

#[derive(Debug, Default, Clone, Copy)]
pub struct FuseStatsSnapshot {
    pub init_epoch: u64,
    pub negotiated_max_read_bytes: u64,
    pub negotiated_max_pages: u64,
    pub negotiated_max_readahead_bytes: u64,
    pub negotiated_async_read: u64,
    pub effective_read_payload_limit_bytes: u64,
    pub request_queue_current: u64,
    pub dispatch_current: u64,
    pub processing_current: u64,
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
    pub reply_payload_copy_count_total: u64,
    pub reply_payload_transfer_count_total: u64,
    pub reply_payload_transfer_bytes_total: u64,
    pub dev_fuse_input_copy_count_total: u64,
    pub dev_fuse_input_copy_bytes_total: u64,
    pub virtiofs_compat_copy_count_total: u64,
    pub virtiofs_compat_copy_bytes_total: u64,
    pub readahead_batches_total: u64,
    pub readahead_requests_total: u64,
    pub readahead_window_pages_total: u64,
    pub readahead_window_pages_peak: u64,
    pub readahead_window_extensions_total: u64,
    pub readahead_window_extension_pages_total: u64,
    pub readahead_saturated_single_page_extensions_total: u64,
    pub readahead_reservation_conflicts_total: u64,
    pub readahead_short_reads_total: u64,
    pub background_inflight_current: u64,
    pub read_reservation_current: u64,
    pub background_inflight_peak: u64,
    pub background_max_blocked_total: u64,
    pub background_congestion_skipped_total: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct VirtioFsStatsSnapshot {
    pub device_queue_depth_max: u64,
    pub hiprio_vring_size_configured: u64,
    pub request_queue_count_configured: u64,
    pub request_vring_size_min_configured: u64,
    pub request_vring_size_max_configured: u64,
    pub sg_limit_pages_configured: u64,
    pub inflight_current: u64,
    pub inflight_peak: u64,
    pub hiprio_inflight_current: u64,
    pub hiprio_inflight_peak: u64,
    pub request_inflight_current: u64,
    pub request_inflight_peak: u64,
    pub queue_full_blocked_current: u64,
    pub reply_retained_current: u64,
    pub reply_retained_peak: u64,
    pub reply_retained_capacity_bytes_current: u64,
    pub reply_retained_capacity_bytes_peak: u64,
    pub reply_credit_blocked_total: u64,
    pub reply_credit_blocked_wake_total: u64,
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
    pub detach_failure_total: u64,
    pub dma_owner_quarantined_total: u64,
    pub bridge_request_clone_count: u64,
    pub bridge_request_clone_bytes: u64,
    pub response_buffer_alloc_count: u64,
    pub response_buffer_alloc_bytes: u64,
    pub response_buffer_waste_bytes: u64,
    pub bytes_submitted_total: u64,
    pub bytes_completed_total: u64,
    pub direct_read_requested_requests_total: u64,
    pub direct_read_requested_bytes_total: u64,
    pub direct_read_completed_requests_total: u64,
    pub direct_read_completed_bytes_total: u64,
    pub read_requested_requests_total: u64,
    pub read_requested_bytes_total: u64,
    pub read_requested_bytes_max: u64,
    pub read_requested_pages: [u64; READ_PAGE_BUCKETS],
    pub pump_batch: [u64; BATCH_BUCKETS],
    pub complete_batch: [u64; BATCH_BUCKETS],
    pub bridge_waits_total: u64,
    pub bridge_wait_exit_request_pending_total: u64,
    pub bridge_wait_exit_completion_total: u64,
    pub bridge_wait_exit_teardown_total: u64,
    pub bridge_wait_exit_disconnect_total: u64,
    pub bridge_wait_exit_spurious_total: u64,
    pub bridge_wake_request_total: u64,
    pub bridge_wake_completion_total: u64,
    pub bridge_wake_reply_released_total: u64,
    pub bridge_wake_teardown_total: u64,
    pub bridge_wake_disconnect_total: u64,
    pub bridge_irq_no_active_conn_total: u64,
    pub bridge_irq_stale_session_total: u64,
    pub bridge_irq_weak_upgrade_failed_total: u64,
    pub bridge_queue_full_blocked_total: u64,
    pub bridge_queue_full_retry_total: u64,
    pub bridge_queue_full_retry_after_completion_total: u64,
    pub bridge_queue_full_retry_success_total: u64,
    pub hiprio_queue_full_total: u64,
    pub request_queue_full_total: u64,
    pub dax_mapping_created_total: u64,
    pub dax_mapping_removed_total: u64,
    pub dax_pressure_reclaims_total: u64,
    pub dax_device_resets_total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtioFsBridgeWakeSource {
    Request,
    Completion,
    Teardown,
    Disconnect,
    ReplyReleased,
}

impl VirtioFsBridgeWakeSource {
    pub const fn bit(self) -> u32 {
        match self {
            Self::Request => 1 << 0,
            Self::Completion => 1 << 1,
            Self::Teardown => 1 << 2,
            Self::Disconnect => 1 << 3,
            Self::ReplyReleased => 1 << 4,
        }
    }

    pub const fn trace_id(self) -> u8 {
        match self {
            Self::Request => 1,
            Self::Completion => 2,
            Self::Teardown => 3,
            Self::Disconnect => 4,
            Self::ReplyReleased => 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtioFsBridgeWaitExit {
    RequestPending,
    Completion,
    Teardown,
    Disconnect,
    Spurious,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtioFsQueueKind {
    Hiprio,
    Request,
}

impl VirtioFsBridgeWaitExit {
    pub const fn trace_id(self) -> u8 {
        match self {
            Self::RequestPending => 1,
            Self::Completion => 2,
            Self::Teardown => 3,
            Self::Disconnect => 4,
            Self::Spurious => 5,
        }
    }
}

static REQUESTS_QUEUED_TOTAL: AtomicU64 = AtomicU64::new(0);
static INIT_EPOCH: AtomicU64 = AtomicU64::new(0);
static INIT_LIMITS_SEQ: AtomicU64 = AtomicU64::new(0);
static NEGOTIATED_MAX_READ_BYTES: AtomicU64 = AtomicU64::new(0);
static NEGOTIATED_MAX_PAGES: AtomicU64 = AtomicU64::new(0);
static NEGOTIATED_MAX_READAHEAD_BYTES: AtomicU64 = AtomicU64::new(0);
static NEGOTIATED_ASYNC_READ: AtomicU64 = AtomicU64::new(0);
static EFFECTIVE_READ_PAYLOAD_LIMIT_BYTES: AtomicU64 = AtomicU64::new(0);
static REQUESTS_DEQUEUED_TOTAL: AtomicU64 = AtomicU64::new(0);
static REQUEST_QUEUE_CURRENT: AtomicU64 = AtomicU64::new(0);
static DISPATCH_CURRENT: AtomicU64 = AtomicU64::new(0);
static PROCESSING_CURRENT: AtomicU64 = AtomicU64::new(0);
static REQUESTS_REPLIED_OK_TOTAL: AtomicU64 = AtomicU64::new(0);
static REQUESTS_REPLIED_ERR_TOTAL: AtomicU64 = AtomicU64::new(0);
static REQUESTS_ABORTED_TOTAL: AtomicU64 = AtomicU64::new(0);
static REQUESTS_DROPPED_UMOUNT_TOTAL: AtomicU64 = AtomicU64::new(0);
static NOREPLY_QUEUED_TOTAL: AtomicU64 = AtomicU64::new(0);
static READ_BUFFER_TOO_SMALL_TOTAL: AtomicU64 = AtomicU64::new(0);
static BYTES_REQUEST_TO_DEV_TOTAL: AtomicU64 = AtomicU64::new(0);
static BYTES_REPLY_PAYLOAD_CLONED_TOTAL: AtomicU64 = AtomicU64::new(0);
static REPLY_PAYLOAD_COPY_COUNT_TOTAL: AtomicU64 = AtomicU64::new(0);
static REPLY_PAYLOAD_TRANSFER_COUNT_TOTAL: AtomicU64 = AtomicU64::new(0);
static REPLY_PAYLOAD_TRANSFER_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
static DEV_FUSE_INPUT_COPY_COUNT_TOTAL: AtomicU64 = AtomicU64::new(0);
static DEV_FUSE_INPUT_COPY_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
static VIRTIOFS_COMPAT_COPY_COUNT_TOTAL: AtomicU64 = AtomicU64::new(0);
static VIRTIOFS_COMPAT_COPY_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
static READAHEAD_BATCHES_TOTAL: AtomicU64 = AtomicU64::new(0);
static READAHEAD_REQUESTS_TOTAL: AtomicU64 = AtomicU64::new(0);
static READAHEAD_WINDOW_PAGES_TOTAL: AtomicU64 = AtomicU64::new(0);
static READAHEAD_WINDOW_PAGES_PEAK: AtomicU64 = AtomicU64::new(0);
static READAHEAD_WINDOW_EXTENSIONS_TOTAL: AtomicU64 = AtomicU64::new(0);
static READAHEAD_WINDOW_EXTENSION_PAGES_TOTAL: AtomicU64 = AtomicU64::new(0);
static READAHEAD_SATURATED_SINGLE_PAGE_EXTENSIONS_TOTAL: AtomicU64 = AtomicU64::new(0);
static READAHEAD_RESERVATION_CONFLICTS_TOTAL: AtomicU64 = AtomicU64::new(0);
static READAHEAD_SHORT_READS_TOTAL: AtomicU64 = AtomicU64::new(0);
static BACKGROUND_INFLIGHT_CURRENT: AtomicU64 = AtomicU64::new(0);
static BACKGROUND_INFLIGHT_PEAK: AtomicU64 = AtomicU64::new(0);
static BACKGROUND_MAX_BLOCKED_TOTAL: AtomicU64 = AtomicU64::new(0);
static BACKGROUND_CONGESTION_SKIPPED_TOTAL: AtomicU64 = AtomicU64::new(0);

pub fn on_fuse_read_limits_negotiated(
    max_read: usize,
    max_pages: usize,
    max_readahead: usize,
    async_read: bool,
    effective_read_payload_limit: usize,
) {
    let sequence = loop {
        let current = INIT_LIMITS_SEQ.load(Ordering::Acquire);
        if current & 1 != 0 {
            core::hint::spin_loop();
            continue;
        }
        if INIT_LIMITS_SEQ
            .compare_exchange(
                current,
                current.wrapping_add(1),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            break current;
        }
    };
    NEGOTIATED_MAX_READ_BYTES.store(max_read as u64, Ordering::Relaxed);
    NEGOTIATED_MAX_PAGES.store(max_pages as u64, Ordering::Relaxed);
    NEGOTIATED_MAX_READAHEAD_BYTES.store(max_readahead as u64, Ordering::Relaxed);
    NEGOTIATED_ASYNC_READ.store(async_read as u64, Ordering::Relaxed);
    EFFECTIVE_READ_PAYLOAD_LIMIT_BYTES
        .store(effective_read_payload_limit as u64, Ordering::Relaxed);
    INIT_EPOCH.fetch_add(1, Ordering::Relaxed);
    INIT_LIMITS_SEQ.store(sequence.wrapping_add(2), Ordering::Release);
}

fn fuse_init_limits_snapshot() -> (u64, u64, u64, u64, u64, u64) {
    loop {
        let before = INIT_LIMITS_SEQ.load(Ordering::Acquire);
        if before & 1 != 0 {
            core::hint::spin_loop();
            continue;
        }
        let values = (
            INIT_EPOCH.load(Ordering::Relaxed),
            NEGOTIATED_MAX_READ_BYTES.load(Ordering::Relaxed),
            NEGOTIATED_MAX_PAGES.load(Ordering::Relaxed),
            NEGOTIATED_MAX_READAHEAD_BYTES.load(Ordering::Relaxed),
            NEGOTIATED_ASYNC_READ.load(Ordering::Relaxed),
            EFFECTIVE_READ_PAYLOAD_LIMIT_BYTES.load(Ordering::Relaxed),
        );
        if before == INIT_LIMITS_SEQ.load(Ordering::Acquire) {
            return values;
        }
    }
}

pub fn on_readahead_batch(window_pages: usize, requests: usize) {
    if !light_stats_enabled() {
        return;
    }
    inc(&READAHEAD_BATCHES_TOTAL);
    add(&READAHEAD_REQUESTS_TOTAL, requests as u64);
    add(&READAHEAD_WINDOW_PAGES_TOTAL, window_pages as u64);
    update_peak(&READAHEAD_WINDOW_PAGES_PEAK, window_pages as u64);
}

pub fn on_readahead_short_read() {
    if !light_stats_enabled() {
        return;
    }
    inc(&READAHEAD_SHORT_READS_TOTAL);
}

pub fn on_readahead_window_extension(extension_pages: usize, saturated: bool) {
    if !light_stats_enabled() || extension_pages == 0 {
        return;
    }
    inc(&READAHEAD_WINDOW_EXTENSIONS_TOTAL);
    add(
        &READAHEAD_WINDOW_EXTENSION_PAGES_TOTAL,
        extension_pages as u64,
    );
    if saturated && extension_pages == 1 {
        inc(&READAHEAD_SATURATED_SINGLE_PAGE_EXTENSIONS_TOTAL);
    }
}

pub fn on_readahead_reservation_conflict() {
    if light_stats_enabled() {
        inc(&READAHEAD_RESERVATION_CONFLICTS_TOTAL);
    }
}

pub fn on_background_acquired() {
    let current = owner_add(&BACKGROUND_INFLIGHT_CURRENT, 1);
    update_peak(&BACKGROUND_INFLIGHT_PEAK, current);
}

pub fn on_background_released() {
    owner_saturating_sub(&BACKGROUND_INFLIGHT_CURRENT, 1);
}

pub fn on_background_pressure(speculative: bool) {
    if speculative {
        inc(&BACKGROUND_CONGESTION_SKIPPED_TOTAL);
    } else {
        inc(&BACKGROUND_MAX_BLOCKED_TOTAL);
    }
}

static DEVICE_QUEUE_DEPTH_MAX: AtomicU64 = AtomicU64::new(0);
static HIPRIO_VRING_SIZE_CONFIGURED: AtomicU64 = AtomicU64::new(0);
static REQUEST_QUEUE_COUNT_CONFIGURED: AtomicU64 = AtomicU64::new(0);
static REQUEST_VRING_SIZE_MIN_CONFIGURED: AtomicU64 = AtomicU64::new(0);
static REQUEST_VRING_SIZE_MAX_CONFIGURED: AtomicU64 = AtomicU64::new(0);
static SG_LIMIT_PAGES_CONFIGURED: AtomicU64 = AtomicU64::new(0);
static INFLIGHT_CURRENT: AtomicU64 = AtomicU64::new(0);
static INFLIGHT_PEAK: AtomicU64 = AtomicU64::new(0);
static HIPRIO_INFLIGHT_CURRENT: AtomicU64 = AtomicU64::new(0);
static HIPRIO_INFLIGHT_PEAK: AtomicU64 = AtomicU64::new(0);
static REQUEST_INFLIGHT_CURRENT: AtomicU64 = AtomicU64::new(0);
static REQUEST_INFLIGHT_PEAK: AtomicU64 = AtomicU64::new(0);
static QUEUE_FULL_BLOCKED_CURRENT: AtomicU64 = AtomicU64::new(0);
static REPLY_RETAINED_CURRENT: AtomicU64 = AtomicU64::new(0);
static REPLY_RETAINED_PEAK: AtomicU64 = AtomicU64::new(0);
static REPLY_RETAINED_CAPACITY_BYTES_CURRENT: AtomicU64 = AtomicU64::new(0);
static REPLY_RETAINED_CAPACITY_BYTES_PEAK: AtomicU64 = AtomicU64::new(0);
static REPLY_CREDIT_BLOCKED_TOTAL: AtomicU64 = AtomicU64::new(0);
static REPLY_CREDIT_BLOCKED_WAKE_TOTAL: AtomicU64 = AtomicU64::new(0);

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
static DETACH_FAILURE_TOTAL: AtomicU64 = AtomicU64::new(0);
static DMA_OWNER_QUARANTINED_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_REQUEST_CLONE_COUNT: AtomicU64 = AtomicU64::new(0);
static BRIDGE_REQUEST_CLONE_BYTES: AtomicU64 = AtomicU64::new(0);
static RESPONSE_BUFFER_ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static RESPONSE_BUFFER_ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static RESPONSE_BUFFER_REUSE_COUNT: AtomicU64 = AtomicU64::new(0);
static RESPONSE_BUFFER_REUSE_BYTES: AtomicU64 = AtomicU64::new(0);
static RESPONSE_BUFFER_ZERO_COUNT: AtomicU64 = AtomicU64::new(0);
static RESPONSE_BUFFER_ZERO_BYTES: AtomicU64 = AtomicU64::new(0);
static RESPONSE_POOL_DROPPED_COUNT: AtomicU64 = AtomicU64::new(0);
static RESPONSE_BUFFER_WASTE_BYTES: AtomicU64 = AtomicU64::new(0);
static BYTES_SUBMITTED_TOTAL: AtomicU64 = AtomicU64::new(0);
static BYTES_COMPLETED_TOTAL: AtomicU64 = AtomicU64::new(0);
// These direct page-cache DMA counters are always-on. A requested event means
// the virtqueue accepted the direct destination descriptors; a completed event
// means the validated successful reply was handed to the pending completion.
static DIRECT_READ_REQUESTED_REQUESTS_TOTAL: AtomicU64 = AtomicU64::new(0);
static DIRECT_READ_REQUESTED_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
static DIRECT_READ_COMPLETED_REQUESTS_TOTAL: AtomicU64 = AtomicU64::new(0);
static DIRECT_READ_COMPLETED_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);

// Request-size diagnostics are optional because they add several RMWs to every
// submitted FUSE_READ. They intentionally count actual requested payload bytes,
// not page-cache reservation capacity.
static READ_REQUESTED_REQUESTS_TOTAL: AtomicU64 = AtomicU64::new(0);
static READ_REQUESTED_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
static READ_REQUESTED_BYTES_MAX: AtomicU64 = AtomicU64::new(0);
static READ_REQUESTED_PAGES: [AtomicU64; READ_PAGE_BUCKETS] =
    [const { AtomicU64::new(0) }; READ_PAGE_BUCKETS];

static OPCODE_REQUESTS_TOTAL: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_REQUEST_BYTES_TOTAL: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_REQUEST_BRIDGE_COPY_COUNT: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_REQUEST_BRIDGE_COPY_BYTES: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_RESPONSE_BUFFER_ALLOC_COUNT: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_RESPONSE_BUFFER_ALLOC_BYTES: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_RESPONSE_BUFFER_REUSE_COUNT: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_RESPONSE_BUFFER_REUSE_BYTES: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_RESPONSE_BUFFER_ZERO_COUNT: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_RESPONSE_BUFFER_ZERO_BYTES: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_RESPONSE_SUBMITTED_CAPACITY_COUNT: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_RESPONSE_SUBMITTED_CAPACITY_BYTES: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_RESPONSE_CAPACITY_FALLBACK_COUNT: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_RESPONSE_COMPLETED_CAPACITY_BYTES: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_RESPONSE_USED_BYTES: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_RESPONSE_UNUSED_TAIL_BYTES: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_RESPONSE_OVERRUN_COUNT: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_RESPONSE_OVERRUN_BYTES: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_REPLY_PAYLOAD_COPY_COUNT: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_REPLY_PAYLOAD_COPY_BYTES: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_REPLY_PAYLOAD_TRANSFER_COUNT: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];
static OPCODE_REPLY_PAYLOAD_TRANSFER_BYTES: [AtomicU64; OPCODE_BUCKETS] =
    [const { AtomicU64::new(0) }; OPCODE_BUCKETS];

static PUMP_BATCH: [AtomicU64; BATCH_BUCKETS] = [const { AtomicU64::new(0) }; BATCH_BUCKETS];
static COMPLETE_BATCH: [AtomicU64; BATCH_BUCKETS] = [const { AtomicU64::new(0) }; BATCH_BUCKETS];

static BRIDGE_WAITS_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_WAIT_EXIT_REQUEST_PENDING_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_WAIT_EXIT_COMPLETION_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_WAIT_EXIT_TEARDOWN_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_WAIT_EXIT_DISCONNECT_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_WAIT_EXIT_SPURIOUS_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_WAKE_REQUEST_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_WAKE_COMPLETION_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_WAKE_REPLY_RELEASED_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_WAKE_TEARDOWN_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_WAKE_DISCONNECT_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_IRQ_NO_ACTIVE_CONN_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_IRQ_STALE_SESSION_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_IRQ_WEAK_UPGRADE_FAILED_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_QUEUE_FULL_BLOCKED_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_QUEUE_FULL_RETRY_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_QUEUE_FULL_RETRY_AFTER_COMPLETION_TOTAL: AtomicU64 = AtomicU64::new(0);
static BRIDGE_QUEUE_FULL_RETRY_SUCCESS_TOTAL: AtomicU64 = AtomicU64::new(0);
static HIPRIO_QUEUE_FULL_TOTAL: AtomicU64 = AtomicU64::new(0);
static REQUEST_QUEUE_FULL_TOTAL: AtomicU64 = AtomicU64::new(0);
// DAX lifecycle events are always enabled. They occur only on mapping, pressure-reclaim,
// and reset transitions, so they do not add atomic RMWs to the per-byte I/O hot path.
static DAX_MAPPING_CREATED_TOTAL: AtomicU64 = AtomicU64::new(0);
static DAX_MAPPING_REMOVED_TOTAL: AtomicU64 = AtomicU64::new(0);
static DAX_PRESSURE_RECLAIMS_TOTAL: AtomicU64 = AtomicU64::new(0);
static DAX_DEVICE_RESETS_TOTAL: AtomicU64 = AtomicU64::new(0);

#[inline]
fn add(counter: &AtomicU64, value: u64) {
    counter.fetch_add(value, Ordering::Relaxed);
}

/// Publish acquisition of an always-on lifecycle owner. Unlike aggregate
/// counters, current-owner gauges participate in quiescence observations and
/// therefore need to synchronize with an Acquire snapshot.
#[inline]
fn owner_add(counter: &AtomicU64, value: u64) -> u64 {
    counter.fetch_add(value, Ordering::AcqRel) + value
}

/// Publish retirement of an always-on lifecycle owner. Keep the saturating
/// behavior so duplicate cleanup cannot underflow a gauge.
#[inline]
fn owner_saturating_sub(counter: &AtomicU64, value: u64) {
    let mut old = counter.load(Ordering::Relaxed);
    loop {
        let new = old.saturating_sub(value);
        match counter.compare_exchange_weak(old, new, Ordering::Release, Ordering::Relaxed) {
            Ok(_) => return,
            Err(v) => old = v,
        }
    }
}

#[inline]
fn inc(counter: &AtomicU64) {
    add(counter, 1);
}

#[inline]
fn opcode_bucket(opcode: u32) -> usize {
    core::cmp::min(opcode as usize, OPCODE_OVERFLOW_BUCKET)
}

#[inline]
fn detailed_stats_enabled() -> bool {
    stats_mode() == FuseStatsMode::Detailed
}

#[inline]
fn light_stats_enabled() -> bool {
    matches!(stats_mode(), FuseStatsMode::Light | FuseStatsMode::Detailed)
}

/// Sample once when an observable owner is acquired. Callers retain that
/// decision until retirement so a mode change cannot split acquire/release.
#[inline]
pub fn optional_read_stats_enabled() -> bool {
    light_stats_enabled()
}

pub fn stats_mode() -> FuseStatsMode {
    match STATS_MODE.load(Ordering::Acquire) {
        0 => FuseStatsMode::Off,
        1 => FuseStatsMode::Light,
        2 => FuseStatsMode::Detailed,
        _ => FuseStatsMode::Off,
    }
}

pub fn set_stats_mode(mode: FuseStatsMode) {
    STATS_MODE.store(mode as u8, Ordering::Release);
}

#[inline]
fn update_peak(peak: &AtomicU64, value: u64) {
    let mut old = peak.load(Ordering::Relaxed);
    while value > old {
        match peak.compare_exchange_weak(old, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(v) => old = v,
        }
    }
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

#[inline]
fn read_page_bucket(pages: usize) -> usize {
    debug_assert!(pages != 0);
    match pages {
        1 => 0,
        2..=4 => 1,
        5..=16 => 2,
        17..=32 => 3,
        33..=64 => 4,
        _ => 5,
    }
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

fn snapshot_read_pages() -> [u64; READ_PAGE_BUCKETS] {
    core::array::from_fn(|idx| READ_REQUESTED_PAGES[idx].load(Ordering::Relaxed))
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

pub fn on_fuse_queue_owner_acquired() {
    owner_add(&REQUEST_QUEUE_CURRENT, 1);
}

pub fn on_fuse_queue_owner_released() {
    owner_saturating_sub(&REQUEST_QUEUE_CURRENT, 1);
}

pub fn on_fuse_dispatch_owner_acquired() {
    owner_add(&DISPATCH_CURRENT, 1);
}

pub fn on_fuse_dispatch_owner_released() {
    owner_saturating_sub(&DISPATCH_CURRENT, 1);
}

pub fn on_fuse_processing_begin() {
    owner_add(&PROCESSING_CURRENT, 1);
}

pub fn on_fuse_processing_end() {
    owner_saturating_sub(&PROCESSING_CURRENT, 1);
}

#[inline]
pub fn on_fuse_read_buffer_too_small() {
    inc(&READ_BUFFER_TOO_SMALL_TOTAL);
}

#[inline]
pub fn on_fuse_reply_complete(_opcode: u32, error: i32, _payload_len: usize) {
    if error == 0 {
        inc(&REQUESTS_REPLIED_OK_TOTAL);
    } else {
        inc(&REQUESTS_REPLIED_ERR_TOTAL);
    }
}

#[inline]
pub fn on_fuse_reply_payload_copy(opcode: u32, payload_len: usize) {
    if payload_len == 0 {
        return;
    }
    inc(&REPLY_PAYLOAD_COPY_COUNT_TOTAL);
    add(&BYTES_REPLY_PAYLOAD_CLONED_TOTAL, payload_len as u64);
    if detailed_stats_enabled() {
        let bucket = opcode_bucket(opcode);
        inc(&OPCODE_REPLY_PAYLOAD_COPY_COUNT[bucket]);
        add(&OPCODE_REPLY_PAYLOAD_COPY_BYTES[bucket], payload_len as u64);
    }
}

#[inline]
pub fn on_virtiofs_reply_retained(capacity: usize) {
    let count = owner_add(&REPLY_RETAINED_CURRENT, 1);
    let capacity = capacity as u64;
    let bytes = owner_add(&REPLY_RETAINED_CAPACITY_BYTES_CURRENT, capacity);
    update_peak(&REPLY_RETAINED_PEAK, count);
    update_peak(&REPLY_RETAINED_CAPACITY_BYTES_PEAK, bytes);
}

#[inline]
pub fn on_virtiofs_reply_released(capacity: usize) {
    // Publish the resource detail before dropping the aggregate owner. An
    // observer that acquires a zero owner count must not retain stale capacity.
    owner_saturating_sub(&REPLY_RETAINED_CAPACITY_BYTES_CURRENT, capacity as u64);
    owner_saturating_sub(&REPLY_RETAINED_CURRENT, 1);
}

#[inline]
pub fn on_virtiofs_reply_capacity_reaccounted(old_capacity: usize, new_capacity: usize) {
    if new_capacity > old_capacity {
        let delta = (new_capacity - old_capacity) as u64;
        let bytes = owner_add(&REPLY_RETAINED_CAPACITY_BYTES_CURRENT, delta);
        update_peak(&REPLY_RETAINED_CAPACITY_BYTES_PEAK, bytes);
    } else {
        owner_saturating_sub(
            &REPLY_RETAINED_CAPACITY_BYTES_CURRENT,
            (old_capacity - new_capacity) as u64,
        );
    }
}

#[inline]
pub fn on_virtiofs_reply_credit_blocked() {
    inc(&REPLY_CREDIT_BLOCKED_TOTAL);
}

#[inline]
pub fn on_virtiofs_reply_credit_blocked_wake() {
    inc(&REPLY_CREDIT_BLOCKED_WAKE_TOTAL);
}

#[inline]
pub fn on_fuse_reply_payload_transfer(opcode: u32, payload_len: usize) {
    if payload_len == 0 {
        return;
    }
    inc(&REPLY_PAYLOAD_TRANSFER_COUNT_TOTAL);
    add(&REPLY_PAYLOAD_TRANSFER_BYTES_TOTAL, payload_len as u64);
    if detailed_stats_enabled() {
        let bucket = opcode_bucket(opcode);
        inc(&OPCODE_REPLY_PAYLOAD_TRANSFER_COUNT[bucket]);
        add(
            &OPCODE_REPLY_PAYLOAD_TRANSFER_BYTES[bucket],
            payload_len as u64,
        );
    }
}

#[inline]
pub fn on_dev_fuse_input_copy(payload_len: usize) {
    inc(&DEV_FUSE_INPUT_COPY_COUNT_TOTAL);
    add(&DEV_FUSE_INPUT_COPY_BYTES_TOTAL, payload_len as u64);
}

#[inline]
pub fn on_virtiofs_compat_copy(payload_len: usize) {
    inc(&VIRTIOFS_COMPAT_COPY_COUNT_TOTAL);
    add(&VIRTIOFS_COMPAT_COPY_BYTES_TOTAL, payload_len as u64);
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

pub fn on_virtiofs_queue_configured(
    device_queue_depth_max: usize,
    hiprio_vring_size: usize,
    request_queue_count: usize,
    request_vring_size_min: usize,
    request_vring_size_max: usize,
    sg_limit_pages: usize,
) {
    DEVICE_QUEUE_DEPTH_MAX.store(device_queue_depth_max as u64, Ordering::Relaxed);
    HIPRIO_VRING_SIZE_CONFIGURED.store(hiprio_vring_size as u64, Ordering::Relaxed);
    REQUEST_QUEUE_COUNT_CONFIGURED.store(request_queue_count as u64, Ordering::Relaxed);
    REQUEST_VRING_SIZE_MIN_CONFIGURED.store(request_vring_size_min as u64, Ordering::Relaxed);
    REQUEST_VRING_SIZE_MAX_CONFIGURED.store(request_vring_size_max as u64, Ordering::Relaxed);
    SG_LIMIT_PAGES_CONFIGURED.store(sg_limit_pages as u64, Ordering::Relaxed);
}

pub fn on_virtiofs_inflight_add(kind: VirtioFsQueueKind) {
    let total = owner_add(&INFLIGHT_CURRENT, 1);
    update_peak(&INFLIGHT_PEAK, total);
    match kind {
        VirtioFsQueueKind::Hiprio => {
            let current = owner_add(&HIPRIO_INFLIGHT_CURRENT, 1);
            update_peak(&HIPRIO_INFLIGHT_PEAK, current);
        }
        VirtioFsQueueKind::Request => {
            let current = owner_add(&REQUEST_INFLIGHT_CURRENT, 1);
            update_peak(&REQUEST_INFLIGHT_PEAK, current);
        }
    }
}

pub fn on_virtiofs_inflight_remove(kind: VirtioFsQueueKind, count: usize) {
    if count == 0 {
        return;
    }
    let count = count as u64;
    // Retire the classified owner before the aggregate. Together with the
    // Acquire snapshot order (aggregate, then class), observing total == 0
    // cannot leave a stale per-queue owner published by this retirement.
    match kind {
        VirtioFsQueueKind::Hiprio => {
            owner_saturating_sub(&HIPRIO_INFLIGHT_CURRENT, count);
        }
        VirtioFsQueueKind::Request => {
            owner_saturating_sub(&REQUEST_INFLIGHT_CURRENT, count);
        }
    }
    owner_saturating_sub(&INFLIGHT_CURRENT, count);
}

#[inline]
pub fn on_virtiofs_idle_sleep(ns: i64) {
    inc(&BRIDGE_IDLE_SLEEPS_TOTAL);
    if ns > 0 {
        add(&BRIDGE_POLL_SLEEP_NS_TOTAL, ns as u64);
    }
}

#[inline]
pub fn on_virtiofs_bridge_wait() {
    inc(&BRIDGE_WAITS_TOTAL);
}

#[inline]
pub fn on_virtiofs_bridge_wait_exit(reason: VirtioFsBridgeWaitExit) {
    match reason {
        VirtioFsBridgeWaitExit::RequestPending => inc(&BRIDGE_WAIT_EXIT_REQUEST_PENDING_TOTAL),
        VirtioFsBridgeWaitExit::Completion => inc(&BRIDGE_WAIT_EXIT_COMPLETION_TOTAL),
        VirtioFsBridgeWaitExit::Teardown => inc(&BRIDGE_WAIT_EXIT_TEARDOWN_TOTAL),
        VirtioFsBridgeWaitExit::Disconnect => inc(&BRIDGE_WAIT_EXIT_DISCONNECT_TOTAL),
        VirtioFsBridgeWaitExit::Spurious => inc(&BRIDGE_WAIT_EXIT_SPURIOUS_TOTAL),
    }
}

#[inline]
pub fn on_virtiofs_bridge_wake(source: VirtioFsBridgeWakeSource) {
    match source {
        VirtioFsBridgeWakeSource::Request => inc(&BRIDGE_WAKE_REQUEST_TOTAL),
        VirtioFsBridgeWakeSource::Completion => inc(&BRIDGE_WAKE_COMPLETION_TOTAL),
        VirtioFsBridgeWakeSource::Teardown => inc(&BRIDGE_WAKE_TEARDOWN_TOTAL),
        VirtioFsBridgeWakeSource::Disconnect => inc(&BRIDGE_WAKE_DISCONNECT_TOTAL),
        VirtioFsBridgeWakeSource::ReplyReleased => inc(&BRIDGE_WAKE_REPLY_RELEASED_TOTAL),
    }
}

#[inline]
pub fn on_virtiofs_irq_no_active_conn() {
    inc(&BRIDGE_IRQ_NO_ACTIVE_CONN_TOTAL);
}

#[inline]
pub fn on_virtiofs_irq_stale_session() {
    inc(&BRIDGE_IRQ_STALE_SESSION_TOTAL);
}

#[inline]
pub fn on_virtiofs_irq_weak_upgrade_failed() {
    inc(&BRIDGE_IRQ_WEAK_UPGRADE_FAILED_TOTAL);
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
pub fn on_virtiofs_response_buffer_alloc(opcode: u32, len: usize) {
    inc(&RESPONSE_BUFFER_ALLOC_COUNT);
    add(&RESPONSE_BUFFER_ALLOC_BYTES, len as u64);
    if detailed_stats_enabled() {
        let bucket = opcode_bucket(opcode);
        inc(&OPCODE_RESPONSE_BUFFER_ALLOC_COUNT[bucket]);
        add(&OPCODE_RESPONSE_BUFFER_ALLOC_BYTES[bucket], len as u64);
    }
}

#[inline]
pub fn on_virtiofs_response_buffer_reuse(opcode: u32, len: usize) {
    if !detailed_stats_enabled() {
        return;
    }
    inc(&RESPONSE_BUFFER_REUSE_COUNT);
    add(&RESPONSE_BUFFER_REUSE_BYTES, len as u64);
    let bucket = opcode_bucket(opcode);
    inc(&OPCODE_RESPONSE_BUFFER_REUSE_COUNT[bucket]);
    add(&OPCODE_RESPONSE_BUFFER_REUSE_BYTES[bucket], len as u64);
}

#[inline]
pub fn on_virtiofs_response_buffer_zero(opcode: u32, len: usize) {
    if !detailed_stats_enabled() {
        return;
    }
    inc(&RESPONSE_BUFFER_ZERO_COUNT);
    add(&RESPONSE_BUFFER_ZERO_BYTES, len as u64);
    let bucket = opcode_bucket(opcode);
    inc(&OPCODE_RESPONSE_BUFFER_ZERO_COUNT[bucket]);
    add(&OPCODE_RESPONSE_BUFFER_ZERO_BYTES[bucket], len as u64);
}

#[inline]
pub fn on_virtiofs_response_pool_drop() {
    if !detailed_stats_enabled() {
        return;
    }
    inc(&RESPONSE_POOL_DROPPED_COUNT);
}

#[inline]
pub fn on_virtiofs_response_buffer_waste(len: usize) {
    add(&RESPONSE_BUFFER_WASTE_BYTES, len as u64);
}

/// Record the response capacity made device-writable by a successful queue add.
///
/// This is deliberately separate from response-buffer preparation: queue-full and
/// submission-error paths can acquire and clear a buffer without submitting it.
#[inline]
pub fn on_virtiofs_response_submitted(opcode: u32, capacity: usize, fallback: bool) {
    if !detailed_stats_enabled() {
        return;
    }

    let bucket = opcode_bucket(opcode);
    inc(&OPCODE_RESPONSE_SUBMITTED_CAPACITY_COUNT[bucket]);
    add(
        &OPCODE_RESPONSE_SUBMITTED_CAPACITY_BYTES[bucket],
        capacity as u64,
    );
    if fallback {
        inc(&OPCODE_RESPONSE_CAPACITY_FALLBACK_COUNT[bucket]);
    }
}

/// Record the used-ring length against the submitted response capacity.
///
/// Valid completions contribute to the completed/used/unused-tail identity. An
/// overrun is kept in a disjoint event domain and records only the excess bytes,
/// avoiding unsigned subtraction and misleading completed-capacity accounting.
#[inline]
pub fn on_virtiofs_response_completed(opcode: u32, capacity: usize, used_len: usize) {
    if !detailed_stats_enabled() {
        return;
    }

    let bucket = opcode_bucket(opcode);
    if used_len > capacity {
        inc(&OPCODE_RESPONSE_OVERRUN_COUNT[bucket]);
        add(
            &OPCODE_RESPONSE_OVERRUN_BYTES[bucket],
            (used_len - capacity) as u64,
        );
        return;
    }

    add(
        &OPCODE_RESPONSE_COMPLETED_CAPACITY_BYTES[bucket],
        capacity as u64,
    );
    add(&OPCODE_RESPONSE_USED_BYTES[bucket], used_len as u64);
    add(
        &OPCODE_RESPONSE_UNUSED_TAIL_BYTES[bucket],
        (capacity - used_len) as u64,
    );
}

#[inline]
pub fn on_virtiofs_submitted(opcode: u32, req_len: usize) {
    inc(&BRIDGE_SUBMITTED_TOTAL);
    add(&BYTES_SUBMITTED_TOTAL, req_len as u64);
    if detailed_stats_enabled() {
        let bucket = opcode_bucket(opcode);
        inc(&OPCODE_REQUESTS_TOTAL[bucket]);
        add(&OPCODE_REQUEST_BYTES_TOTAL[bucket], req_len as u64);
    }
}

/// Record the payload size of a successfully submitted FUSE_READ.
///
/// `pages` is supplied by the caller using the active architecture page size;
/// keeping that conversion out of this module avoids coupling statistics to a
/// particular page-cache implementation.
#[inline]
pub fn on_virtiofs_read_requested(requested_bytes: usize, pages: usize) {
    if !light_stats_enabled() || requested_bytes == 0 || pages == 0 {
        return;
    }
    inc(&READ_REQUESTED_REQUESTS_TOTAL);
    add(&READ_REQUESTED_BYTES_TOTAL, requested_bytes as u64);
    update_peak(&READ_REQUESTED_BYTES_MAX, requested_bytes as u64);
    inc(&READ_REQUESTED_PAGES[read_page_bucket(pages)]);
}

/// Queue acceptance is the DMA ownership commit point.
#[inline]
pub fn on_virtiofs_direct_read_requested(requested_bytes: usize) {
    inc(&DIRECT_READ_REQUESTED_REQUESTS_TOTAL);
    add(&DIRECT_READ_REQUESTED_BYTES_TOTAL, requested_bytes as u64);
}

/// Count only a validated successful direct reply handed to pending completion.
#[inline]
pub fn on_virtiofs_direct_read_completed(payload_bytes: usize) {
    inc(&DIRECT_READ_COMPLETED_REQUESTS_TOTAL);
    add(&DIRECT_READ_COMPLETED_BYTES_TOTAL, payload_bytes as u64);
}

#[inline]
pub fn on_virtiofs_queue_full(kind: VirtioFsQueueKind) {
    inc(&VIRTQUEUE_FULL_TOTAL);
    match kind {
        VirtioFsQueueKind::Hiprio => inc(&HIPRIO_QUEUE_FULL_TOTAL),
        VirtioFsQueueKind::Request => inc(&REQUEST_QUEUE_FULL_TOTAL),
    }
}

#[inline]
pub fn on_virtiofs_queue_full_blocked() {
    inc(&BRIDGE_QUEUE_FULL_BLOCKED_TOTAL);
    owner_add(&QUEUE_FULL_BLOCKED_CURRENT, 1);
}

#[inline]
pub fn on_virtiofs_queue_full_unblocked() {
    owner_saturating_sub(&QUEUE_FULL_BLOCKED_CURRENT, 1);
}

#[inline]
pub fn on_virtiofs_queue_full_retry(after_completion: bool) {
    inc(&BRIDGE_QUEUE_FULL_RETRY_TOTAL);
    if after_completion {
        inc(&BRIDGE_QUEUE_FULL_RETRY_AFTER_COMPLETION_TOTAL);
    }
}

#[inline]
pub fn on_virtiofs_queue_full_retry_success() {
    inc(&BRIDGE_QUEUE_FULL_RETRY_SUCCESS_TOTAL);
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
pub fn on_virtiofs_detach_failure() {
    inc(&DETACH_FAILURE_TOTAL);
}

#[inline]
pub fn on_virtiofs_dma_owner_quarantined() {
    inc(&DMA_OWNER_QUARANTINED_TOTAL);
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

#[inline]
pub fn on_virtiofs_dax_mapping_created() {
    inc(&DAX_MAPPING_CREATED_TOTAL);
}

#[inline]
pub fn on_virtiofs_dax_mapping_removed() {
    inc(&DAX_MAPPING_REMOVED_TOTAL);
}

#[inline]
pub fn on_virtiofs_dax_pressure_reclaim() {
    inc(&DAX_PRESSURE_RECLAIMS_TOTAL);
}

#[inline]
pub fn on_virtiofs_dax_device_reset() {
    inc(&DAX_DEVICE_RESETS_TOTAL);
}

pub fn fuse_snapshot() -> FuseStatsSnapshot {
    let (init_epoch, max_read, max_pages, max_readahead, async_read, effective_read) =
        fuse_init_limits_snapshot();
    FuseStatsSnapshot {
        init_epoch,
        negotiated_max_read_bytes: max_read,
        negotiated_max_pages: max_pages,
        negotiated_max_readahead_bytes: max_readahead,
        negotiated_async_read: async_read,
        effective_read_payload_limit_bytes: effective_read,
        request_queue_current: REQUEST_QUEUE_CURRENT.load(Ordering::Acquire),
        dispatch_current: DISPATCH_CURRENT.load(Ordering::Acquire),
        processing_current: PROCESSING_CURRENT.load(Ordering::Acquire),
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
        reply_payload_copy_count_total: REPLY_PAYLOAD_COPY_COUNT_TOTAL.load(Ordering::Relaxed),
        reply_payload_transfer_count_total: REPLY_PAYLOAD_TRANSFER_COUNT_TOTAL
            .load(Ordering::Relaxed),
        reply_payload_transfer_bytes_total: REPLY_PAYLOAD_TRANSFER_BYTES_TOTAL
            .load(Ordering::Relaxed),
        dev_fuse_input_copy_count_total: DEV_FUSE_INPUT_COPY_COUNT_TOTAL.load(Ordering::Relaxed),
        dev_fuse_input_copy_bytes_total: DEV_FUSE_INPUT_COPY_BYTES_TOTAL.load(Ordering::Relaxed),
        virtiofs_compat_copy_count_total: VIRTIOFS_COMPAT_COPY_COUNT_TOTAL.load(Ordering::Relaxed),
        virtiofs_compat_copy_bytes_total: VIRTIOFS_COMPAT_COPY_BYTES_TOTAL.load(Ordering::Relaxed),
        readahead_batches_total: READAHEAD_BATCHES_TOTAL.load(Ordering::Relaxed),
        readahead_requests_total: READAHEAD_REQUESTS_TOTAL.load(Ordering::Relaxed),
        readahead_window_pages_total: READAHEAD_WINDOW_PAGES_TOTAL.load(Ordering::Relaxed),
        readahead_window_pages_peak: READAHEAD_WINDOW_PAGES_PEAK.load(Ordering::Relaxed),
        readahead_window_extensions_total: READAHEAD_WINDOW_EXTENSIONS_TOTAL
            .load(Ordering::Relaxed),
        readahead_window_extension_pages_total: READAHEAD_WINDOW_EXTENSION_PAGES_TOTAL
            .load(Ordering::Relaxed),
        readahead_saturated_single_page_extensions_total:
            READAHEAD_SATURATED_SINGLE_PAGE_EXTENSIONS_TOTAL.load(Ordering::Relaxed),
        readahead_reservation_conflicts_total: READAHEAD_RESERVATION_CONFLICTS_TOTAL
            .load(Ordering::Relaxed),
        readahead_short_reads_total: READAHEAD_SHORT_READS_TOTAL.load(Ordering::Relaxed),
        background_inflight_current: BACKGROUND_INFLIGHT_CURRENT.load(Ordering::Acquire),
        read_reservation_current: page_cache_stats::snapshot().read_dma_reservations,
        background_inflight_peak: BACKGROUND_INFLIGHT_PEAK.load(Ordering::Relaxed),
        background_max_blocked_total: BACKGROUND_MAX_BLOCKED_TOTAL.load(Ordering::Relaxed),
        background_congestion_skipped_total: BACKGROUND_CONGESTION_SKIPPED_TOTAL
            .load(Ordering::Relaxed),
    }
}

pub fn virtiofs_snapshot() -> VirtioFsStatsSnapshot {
    VirtioFsStatsSnapshot {
        device_queue_depth_max: DEVICE_QUEUE_DEPTH_MAX.load(Ordering::Relaxed),
        hiprio_vring_size_configured: HIPRIO_VRING_SIZE_CONFIGURED.load(Ordering::Relaxed),
        request_queue_count_configured: REQUEST_QUEUE_COUNT_CONFIGURED.load(Ordering::Relaxed),
        request_vring_size_min_configured: REQUEST_VRING_SIZE_MIN_CONFIGURED
            .load(Ordering::Relaxed),
        request_vring_size_max_configured: REQUEST_VRING_SIZE_MAX_CONFIGURED
            .load(Ordering::Relaxed),
        sg_limit_pages_configured: SG_LIMIT_PAGES_CONFIGURED.load(Ordering::Relaxed),
        inflight_current: INFLIGHT_CURRENT.load(Ordering::Acquire),
        inflight_peak: INFLIGHT_PEAK.load(Ordering::Relaxed),
        hiprio_inflight_current: HIPRIO_INFLIGHT_CURRENT.load(Ordering::Acquire),
        hiprio_inflight_peak: HIPRIO_INFLIGHT_PEAK.load(Ordering::Relaxed),
        request_inflight_current: REQUEST_INFLIGHT_CURRENT.load(Ordering::Acquire),
        request_inflight_peak: REQUEST_INFLIGHT_PEAK.load(Ordering::Relaxed),
        queue_full_blocked_current: QUEUE_FULL_BLOCKED_CURRENT.load(Ordering::Acquire),
        reply_retained_current: REPLY_RETAINED_CURRENT.load(Ordering::Acquire),
        reply_retained_peak: REPLY_RETAINED_PEAK.load(Ordering::Relaxed),
        reply_retained_capacity_bytes_current: REPLY_RETAINED_CAPACITY_BYTES_CURRENT
            .load(Ordering::Acquire),
        reply_retained_capacity_bytes_peak: REPLY_RETAINED_CAPACITY_BYTES_PEAK
            .load(Ordering::Relaxed),
        reply_credit_blocked_total: REPLY_CREDIT_BLOCKED_TOTAL.load(Ordering::Relaxed),
        reply_credit_blocked_wake_total: REPLY_CREDIT_BLOCKED_WAKE_TOTAL.load(Ordering::Relaxed),
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
        detach_failure_total: DETACH_FAILURE_TOTAL.load(Ordering::Relaxed),
        dma_owner_quarantined_total: DMA_OWNER_QUARANTINED_TOTAL.load(Ordering::Relaxed),
        bridge_request_clone_count: BRIDGE_REQUEST_CLONE_COUNT.load(Ordering::Relaxed),
        bridge_request_clone_bytes: BRIDGE_REQUEST_CLONE_BYTES.load(Ordering::Relaxed),
        response_buffer_alloc_count: RESPONSE_BUFFER_ALLOC_COUNT.load(Ordering::Relaxed),
        response_buffer_alloc_bytes: RESPONSE_BUFFER_ALLOC_BYTES.load(Ordering::Relaxed),
        response_buffer_waste_bytes: RESPONSE_BUFFER_WASTE_BYTES.load(Ordering::Relaxed),
        bytes_submitted_total: BYTES_SUBMITTED_TOTAL.load(Ordering::Relaxed),
        bytes_completed_total: BYTES_COMPLETED_TOTAL.load(Ordering::Relaxed),
        direct_read_requested_requests_total: DIRECT_READ_REQUESTED_REQUESTS_TOTAL
            .load(Ordering::Relaxed),
        direct_read_requested_bytes_total: DIRECT_READ_REQUESTED_BYTES_TOTAL
            .load(Ordering::Relaxed),
        direct_read_completed_requests_total: DIRECT_READ_COMPLETED_REQUESTS_TOTAL
            .load(Ordering::Relaxed),
        direct_read_completed_bytes_total: DIRECT_READ_COMPLETED_BYTES_TOTAL
            .load(Ordering::Relaxed),
        read_requested_requests_total: READ_REQUESTED_REQUESTS_TOTAL.load(Ordering::Relaxed),
        read_requested_bytes_total: READ_REQUESTED_BYTES_TOTAL.load(Ordering::Relaxed),
        read_requested_bytes_max: READ_REQUESTED_BYTES_MAX.load(Ordering::Relaxed),
        read_requested_pages: snapshot_read_pages(),
        pump_batch: snapshot_batch(&PUMP_BATCH),
        complete_batch: snapshot_batch(&COMPLETE_BATCH),
        bridge_waits_total: BRIDGE_WAITS_TOTAL.load(Ordering::Relaxed),
        bridge_wait_exit_request_pending_total: BRIDGE_WAIT_EXIT_REQUEST_PENDING_TOTAL
            .load(Ordering::Relaxed),
        bridge_wait_exit_completion_total: BRIDGE_WAIT_EXIT_COMPLETION_TOTAL
            .load(Ordering::Relaxed),
        bridge_wait_exit_teardown_total: BRIDGE_WAIT_EXIT_TEARDOWN_TOTAL.load(Ordering::Relaxed),
        bridge_wait_exit_disconnect_total: BRIDGE_WAIT_EXIT_DISCONNECT_TOTAL
            .load(Ordering::Relaxed),
        bridge_wait_exit_spurious_total: BRIDGE_WAIT_EXIT_SPURIOUS_TOTAL.load(Ordering::Relaxed),
        bridge_wake_request_total: BRIDGE_WAKE_REQUEST_TOTAL.load(Ordering::Relaxed),
        bridge_wake_completion_total: BRIDGE_WAKE_COMPLETION_TOTAL.load(Ordering::Relaxed),
        bridge_wake_reply_released_total: BRIDGE_WAKE_REPLY_RELEASED_TOTAL.load(Ordering::Relaxed),
        bridge_wake_teardown_total: BRIDGE_WAKE_TEARDOWN_TOTAL.load(Ordering::Relaxed),
        bridge_wake_disconnect_total: BRIDGE_WAKE_DISCONNECT_TOTAL.load(Ordering::Relaxed),
        bridge_irq_no_active_conn_total: BRIDGE_IRQ_NO_ACTIVE_CONN_TOTAL.load(Ordering::Relaxed),
        bridge_irq_stale_session_total: BRIDGE_IRQ_STALE_SESSION_TOTAL.load(Ordering::Relaxed),
        bridge_irq_weak_upgrade_failed_total: BRIDGE_IRQ_WEAK_UPGRADE_FAILED_TOTAL
            .load(Ordering::Relaxed),
        bridge_queue_full_blocked_total: BRIDGE_QUEUE_FULL_BLOCKED_TOTAL.load(Ordering::Relaxed),
        bridge_queue_full_retry_total: BRIDGE_QUEUE_FULL_RETRY_TOTAL.load(Ordering::Relaxed),
        bridge_queue_full_retry_after_completion_total:
            BRIDGE_QUEUE_FULL_RETRY_AFTER_COMPLETION_TOTAL.load(Ordering::Relaxed),
        bridge_queue_full_retry_success_total: BRIDGE_QUEUE_FULL_RETRY_SUCCESS_TOTAL
            .load(Ordering::Relaxed),
        hiprio_queue_full_total: HIPRIO_QUEUE_FULL_TOTAL.load(Ordering::Relaxed),
        request_queue_full_total: REQUEST_QUEUE_FULL_TOTAL.load(Ordering::Relaxed),
        dax_mapping_created_total: DAX_MAPPING_CREATED_TOTAL.load(Ordering::Relaxed),
        dax_mapping_removed_total: DAX_MAPPING_REMOVED_TOTAL.load(Ordering::Relaxed),
        dax_pressure_reclaims_total: DAX_PRESSURE_RECLAIMS_TOTAL.load(Ordering::Relaxed),
        dax_device_resets_total: DAX_DEVICE_RESETS_TOTAL.load(Ordering::Relaxed),
    }
}

pub fn format_snapshot() -> String {
    let fuse = fuse_snapshot();
    let virtiofs = virtiofs_snapshot();
    let mut output = format!(
        "[control]\n\
mode {}\n\
always_on aggregate_transport,quiescence_owner\n\
light direct_read_dma,read_size_buckets\n\
detailed opcode,copy,allocation\n\
\n\
[fuse]\n\
init_epoch {}\n\
negotiated_max_read_bytes {}\n\
negotiated_max_pages {}\n\
negotiated_max_readahead_bytes {}\n\
negotiated_async_read {}\n\
effective_read_payload_limit_bytes {}\n\
request_queue_current {}\n\
dispatch_current {}\n\
processing_current {}\n\
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
reply_payload_copy_count_total {}\n\
reply_payload_transfer_count_total {}\n\
reply_payload_transfer_bytes_total {}\n\
dev_fuse_input_copy_count_total {}\n\
dev_fuse_input_copy_bytes_total {}\n\
virtiofs_compat_copy_count_total {}\n\
virtiofs_compat_copy_bytes_total {}\n\
readahead_batches_total {}\n\
readahead_requests_total {}\n\
readahead_window_pages_total {}\n\
readahead_window_pages_peak {}\n\
readahead_window_extensions_total {}\n\
readahead_window_extension_pages_total {}\n\
readahead_saturated_single_page_extensions_total {}\n\
readahead_reservation_conflicts_total {}\n\
readahead_short_reads_total {}\n\
background_inflight_current {}\n\
read_reservation_current {}\n\
background_inflight_peak {}\n\
background_max_blocked_total {}\n\
background_congestion_skipped_total {}\n\
\n\
[virtiofs]\n\
device_queue_depth_max {}\n\
hiprio_vring_size_configured {}\n\
request_queue_count_configured {}\n\
request_vring_size_min_configured {}\n\
request_vring_size_max_configured {}\n\
sg_limit_pages_configured {}\n\
inflight_current {}\n\
inflight_peak {}\n\
hiprio_inflight_current {}\n\
hiprio_inflight_peak {}\n\
request_inflight_current {}\n\
request_inflight_peak {}\n\
queue_full_blocked_current {}\n\
reply_retained_current {}\n\
reply_retained_peak {}\n\
reply_retained_capacity_bytes_current {}\n\
reply_retained_capacity_bytes_peak {}\n\
reply_credit_blocked_total {}\n\
reply_credit_blocked_wake_total {}\n\
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
detach_failure_total {}\n\
dma_owner_quarantined_total {}\n\
bridge_request_clone_count {}\n\
bridge_request_clone_bytes {}\n\
response_buffer_alloc_count {}\n\
response_buffer_alloc_bytes {}\n\
response_buffer_waste_bytes {}\n\
bytes_submitted_total {}\n\
bytes_completed_total {}\n\
direct_read_requested_requests_total {}\n\
direct_read_requested_bytes_total {}\n\
direct_read_completed_requests_total {}\n\
direct_read_completed_bytes_total {}\n\
read_requested_requests_total {}\n\
read_requested_bytes_total {}\n\
read_requested_bytes_max {}\n\
read_requested_pages_1 {}\n\
read_requested_pages_2_4 {}\n\
read_requested_pages_5_16 {}\n\
read_requested_pages_17_32 {}\n\
read_requested_pages_33_64 {}\n\
read_requested_pages_65_plus {}\n\
pump_batch_0 {}\n\
pump_batch_1 {}\n\
pump_batch_2_4 {}\n\
pump_batch_5_16 {}\n\
pump_batch_gt_16 {}\n\
complete_batch_0 {}\n\
complete_batch_1 {}\n\
complete_batch_2_4 {}\n\
complete_batch_5_16 {}\n\
complete_batch_gt_16 {}\n\
bridge_waits_total {}\n\
bridge_wait_exit_request_pending_total {}\n\
bridge_wait_exit_completion_total {}\n\
bridge_wait_exit_teardown_total {}\n\
bridge_wait_exit_disconnect_total {}\n\
bridge_wait_exit_spurious_total {}\n\
bridge_wake_request_total {}\n\
bridge_wake_completion_total {}\n\
bridge_wake_reply_released_total {}\n\
bridge_wake_teardown_total {}\n\
bridge_wake_disconnect_total {}\n\
bridge_irq_no_active_conn_total {}\n\
bridge_irq_stale_session_total {}\n\
bridge_irq_weak_upgrade_failed_total {}\n\
bridge_queue_full_blocked_total {}\n\
bridge_queue_full_retry_total {}\n\
bridge_queue_full_retry_after_completion_total {}\n\
bridge_queue_full_retry_success_total {}\n\
hiprio_queue_full_total {}\n\
request_queue_full_total {}\n\
dax_mapping_created_total {}\n\
dax_mapping_removed_total {}\n\
dax_pressure_reclaims_total {}\n\
dax_device_resets_total {}\n",
        stats_mode().as_str(),
        fuse.init_epoch,
        fuse.negotiated_max_read_bytes,
        fuse.negotiated_max_pages,
        fuse.negotiated_max_readahead_bytes,
        fuse.negotiated_async_read,
        fuse.effective_read_payload_limit_bytes,
        fuse.request_queue_current,
        fuse.dispatch_current,
        fuse.processing_current,
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
        fuse.reply_payload_copy_count_total,
        fuse.reply_payload_transfer_count_total,
        fuse.reply_payload_transfer_bytes_total,
        fuse.dev_fuse_input_copy_count_total,
        fuse.dev_fuse_input_copy_bytes_total,
        fuse.virtiofs_compat_copy_count_total,
        fuse.virtiofs_compat_copy_bytes_total,
        fuse.readahead_batches_total,
        fuse.readahead_requests_total,
        fuse.readahead_window_pages_total,
        fuse.readahead_window_pages_peak,
        fuse.readahead_window_extensions_total,
        fuse.readahead_window_extension_pages_total,
        fuse.readahead_saturated_single_page_extensions_total,
        fuse.readahead_reservation_conflicts_total,
        fuse.readahead_short_reads_total,
        fuse.background_inflight_current,
        fuse.read_reservation_current,
        fuse.background_inflight_peak,
        fuse.background_max_blocked_total,
        fuse.background_congestion_skipped_total,
        virtiofs.device_queue_depth_max,
        virtiofs.hiprio_vring_size_configured,
        virtiofs.request_queue_count_configured,
        virtiofs.request_vring_size_min_configured,
        virtiofs.request_vring_size_max_configured,
        virtiofs.sg_limit_pages_configured,
        virtiofs.inflight_current,
        virtiofs.inflight_peak,
        virtiofs.hiprio_inflight_current,
        virtiofs.hiprio_inflight_peak,
        virtiofs.request_inflight_current,
        virtiofs.request_inflight_peak,
        virtiofs.queue_full_blocked_current,
        virtiofs.reply_retained_current,
        virtiofs.reply_retained_peak,
        virtiofs.reply_retained_capacity_bytes_current,
        virtiofs.reply_retained_capacity_bytes_peak,
        virtiofs.reply_credit_blocked_total,
        virtiofs.reply_credit_blocked_wake_total,
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
        virtiofs.detach_failure_total,
        virtiofs.dma_owner_quarantined_total,
        virtiofs.bridge_request_clone_count,
        virtiofs.bridge_request_clone_bytes,
        virtiofs.response_buffer_alloc_count,
        virtiofs.response_buffer_alloc_bytes,
        virtiofs.response_buffer_waste_bytes,
        virtiofs.bytes_submitted_total,
        virtiofs.bytes_completed_total,
        virtiofs.direct_read_requested_requests_total,
        virtiofs.direct_read_requested_bytes_total,
        virtiofs.direct_read_completed_requests_total,
        virtiofs.direct_read_completed_bytes_total,
        virtiofs.read_requested_requests_total,
        virtiofs.read_requested_bytes_total,
        virtiofs.read_requested_bytes_max,
        virtiofs.read_requested_pages[0],
        virtiofs.read_requested_pages[1],
        virtiofs.read_requested_pages[2],
        virtiofs.read_requested_pages[3],
        virtiofs.read_requested_pages[4],
        virtiofs.read_requested_pages[5],
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
        virtiofs.bridge_waits_total,
        virtiofs.bridge_wait_exit_request_pending_total,
        virtiofs.bridge_wait_exit_completion_total,
        virtiofs.bridge_wait_exit_teardown_total,
        virtiofs.bridge_wait_exit_disconnect_total,
        virtiofs.bridge_wait_exit_spurious_total,
        virtiofs.bridge_wake_request_total,
        virtiofs.bridge_wake_completion_total,
        virtiofs.bridge_wake_reply_released_total,
        virtiofs.bridge_wake_teardown_total,
        virtiofs.bridge_wake_disconnect_total,
        virtiofs.bridge_irq_no_active_conn_total,
        virtiofs.bridge_irq_stale_session_total,
        virtiofs.bridge_irq_weak_upgrade_failed_total,
        virtiofs.bridge_queue_full_blocked_total,
        virtiofs.bridge_queue_full_retry_total,
        virtiofs.bridge_queue_full_retry_after_completion_total,
        virtiofs.bridge_queue_full_retry_success_total,
        virtiofs.hiprio_queue_full_total,
        virtiofs.request_queue_full_total,
        virtiofs.dax_mapping_created_total,
        virtiofs.dax_mapping_removed_total,
        virtiofs.dax_pressure_reclaims_total,
        virtiofs.dax_device_resets_total,
    );

    writeln!(
        output,
        "response_buffer_reuse_count {}\nresponse_buffer_reuse_bytes {}\n\
response_buffer_zero_count {}\nresponse_buffer_zero_bytes {}\nresponse_pool_dropped_count {}",
        RESPONSE_BUFFER_REUSE_COUNT.load(Ordering::Relaxed),
        RESPONSE_BUFFER_REUSE_BYTES.load(Ordering::Relaxed),
        RESPONSE_BUFFER_ZERO_COUNT.load(Ordering::Relaxed),
        RESPONSE_BUFFER_ZERO_BYTES.load(Ordering::Relaxed),
        RESPONSE_POOL_DROPPED_COUNT.load(Ordering::Relaxed),
    )
    .expect("formatting fuse stats into String cannot fail");

    output.push_str("\n[virtiofs_opcode]\n");
    for opcode in 0..OPCODE_BUCKETS {
        writeln!(
            output,
            "opcode_{opcode}_requests_total {}\n\
opcode_{opcode}_request_bytes_total {}\n\
opcode_{opcode}_request_bridge_copy_count {}\n\
opcode_{opcode}_request_bridge_copy_bytes {}\n\
opcode_{opcode}_response_buffer_alloc_count {}\n\
opcode_{opcode}_response_buffer_alloc_bytes {}\n\
opcode_{opcode}_response_buffer_reuse_count {}\n\
opcode_{opcode}_response_buffer_reuse_bytes {}\n\
opcode_{opcode}_response_buffer_zero_count {}\n\
opcode_{opcode}_response_buffer_zero_bytes {}\n\
opcode_{opcode}_response_submitted_capacity_count {}\n\
opcode_{opcode}_response_submitted_capacity_bytes {}\n\
opcode_{opcode}_response_capacity_fallback_count {}\n\
opcode_{opcode}_response_completed_capacity_bytes {}\n\
opcode_{opcode}_response_used_bytes {}\n\
opcode_{opcode}_response_unused_tail_bytes {}\n\
opcode_{opcode}_response_overrun_count {}\n\
opcode_{opcode}_response_overrun_bytes {}\n\
opcode_{opcode}_reply_payload_copy_count {}\n\
opcode_{opcode}_reply_payload_copy_bytes {}\n\
opcode_{opcode}_reply_payload_transfer_count {}\n\
opcode_{opcode}_reply_payload_transfer_bytes {}",
            OPCODE_REQUESTS_TOTAL[opcode].load(Ordering::Relaxed),
            OPCODE_REQUEST_BYTES_TOTAL[opcode].load(Ordering::Relaxed),
            OPCODE_REQUEST_BRIDGE_COPY_COUNT[opcode].load(Ordering::Relaxed),
            OPCODE_REQUEST_BRIDGE_COPY_BYTES[opcode].load(Ordering::Relaxed),
            OPCODE_RESPONSE_BUFFER_ALLOC_COUNT[opcode].load(Ordering::Relaxed),
            OPCODE_RESPONSE_BUFFER_ALLOC_BYTES[opcode].load(Ordering::Relaxed),
            OPCODE_RESPONSE_BUFFER_REUSE_COUNT[opcode].load(Ordering::Relaxed),
            OPCODE_RESPONSE_BUFFER_REUSE_BYTES[opcode].load(Ordering::Relaxed),
            OPCODE_RESPONSE_BUFFER_ZERO_COUNT[opcode].load(Ordering::Relaxed),
            OPCODE_RESPONSE_BUFFER_ZERO_BYTES[opcode].load(Ordering::Relaxed),
            OPCODE_RESPONSE_SUBMITTED_CAPACITY_COUNT[opcode].load(Ordering::Relaxed),
            OPCODE_RESPONSE_SUBMITTED_CAPACITY_BYTES[opcode].load(Ordering::Relaxed),
            OPCODE_RESPONSE_CAPACITY_FALLBACK_COUNT[opcode].load(Ordering::Relaxed),
            OPCODE_RESPONSE_COMPLETED_CAPACITY_BYTES[opcode].load(Ordering::Relaxed),
            OPCODE_RESPONSE_USED_BYTES[opcode].load(Ordering::Relaxed),
            OPCODE_RESPONSE_UNUSED_TAIL_BYTES[opcode].load(Ordering::Relaxed),
            OPCODE_RESPONSE_OVERRUN_COUNT[opcode].load(Ordering::Relaxed),
            OPCODE_RESPONSE_OVERRUN_BYTES[opcode].load(Ordering::Relaxed),
            OPCODE_REPLY_PAYLOAD_COPY_COUNT[opcode].load(Ordering::Relaxed),
            OPCODE_REPLY_PAYLOAD_COPY_BYTES[opcode].load(Ordering::Relaxed),
            OPCODE_REPLY_PAYLOAD_TRANSFER_COUNT[opcode].load(Ordering::Relaxed),
            OPCODE_REPLY_PAYLOAD_TRANSFER_BYTES[opcode].load(Ordering::Relaxed),
        )
        .expect("formatting fuse opcode stats into String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_mode_parser_is_strict_but_accepts_debugfs_newline() {
        assert_eq!(FuseStatsMode::parse("off\n"), Ok(FuseStatsMode::Off));
        assert_eq!(FuseStatsMode::parse(" light "), Ok(FuseStatsMode::Light));
        assert_eq!(
            FuseStatsMode::parse("detailed"),
            Ok(FuseStatsMode::Detailed)
        );
        assert!(FuseStatsMode::parse("on").is_err());
    }

    #[test]
    fn read_page_buckets_match_reported_ranges() {
        assert_eq!(read_page_bucket(1), 0);
        assert_eq!(read_page_bucket(2), 1);
        assert_eq!(read_page_bucket(4), 1);
        assert_eq!(read_page_bucket(5), 2);
        assert_eq!(read_page_bucket(16), 2);
        assert_eq!(read_page_bucket(17), 3);
        assert_eq!(read_page_bucket(32), 3);
        assert_eq!(read_page_bucket(33), 4);
        assert_eq!(read_page_bucket(64), 4);
        assert_eq!(read_page_bucket(65), 5);
    }

    #[test]
    fn formatting_snapshot_does_not_enable_detailed_mode() {
        let previous = stats_mode();
        let report = format_snapshot();
        assert_eq!(stats_mode(), previous);
        assert!(report.contains(&alloc::format!("[control]\nmode {}\n", previous.as_str())));
    }
}
