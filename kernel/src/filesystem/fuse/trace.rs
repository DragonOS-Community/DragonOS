use alloc::format;

use crate::define_event_trace;

pub const VIRTIOFS_QUEUE_HIPRIO: u8 = 0;
pub const VIRTIOFS_QUEUE_REQUEST: u8 = 1;

pub const VIRTIOFS_RETRY_QUEUE_FULL: u8 = 1;

define_event_trace!(
fuse_request_queue,
TP_system(fuse),
TP_PROTO(unique: u64, opcode: u32, len: u64, no_reply: u8),
TP_STRUCT__entry {
    unique: u64,
    opcode: u32,
    len: u64,
    no_reply: u8,
},
TP_fast_assign {
    unique: unique,
    opcode: opcode,
    len: len,
    no_reply: no_reply,
},
TP_ident(__entry),
TP_printk({
    let unique = __entry.unique;
    let opcode = __entry.opcode;
    let len = __entry.len;
    let no_reply = __entry.no_reply;
    format!(
        "unique={} opcode={} len={} no_reply={}",
        unique, opcode, len, no_reply
    )
})
);

define_event_trace!(
    virtiofs_bridge_wait_enter,
    TP_system(fuse),
    TP_PROTO(pending: u8, inflight: u8, can_pop: u8, queue_full_blocked: u8),
    TP_STRUCT__entry {
        pending: u8,
        inflight: u8,
        can_pop: u8,
        queue_full_blocked: u8,
    },
    TP_fast_assign {
        pending: pending,
        inflight: inflight,
        can_pop: can_pop,
        queue_full_blocked: queue_full_blocked,
    },
    TP_ident(__entry),
    TP_printk({
        let pending = __entry.pending;
        let inflight = __entry.inflight;
        let can_pop = __entry.can_pop;
        let queue_full_blocked = __entry.queue_full_blocked;
        format!(
            "pending={} inflight={} can_pop={} queue_full_blocked={}",
            pending, inflight, can_pop, queue_full_blocked
        )
    })
);

define_event_trace!(
    virtiofs_bridge_wait_exit,
    TP_system(fuse),
    TP_PROTO(reason: u8, events: u32),
    TP_STRUCT__entry {
        reason: u8,
        events: u32,
    },
    TP_fast_assign {
        reason: reason,
        events: events,
    },
    TP_ident(__entry),
    TP_printk({
        let reason = __entry.reason;
        let events = __entry.events;
        format!("reason={} events={}", reason, events)
    })
);

define_event_trace!(
    virtiofs_bridge_wake,
    TP_system(fuse),
    TP_PROTO(source: u8),
    TP_STRUCT__entry {
        source: u8,
    },
    TP_fast_assign {
        source: source,
    },
    TP_ident(__entry),
    TP_printk({
        let source = __entry.source;
        format!("source={}", source)
    })
);

define_event_trace!(
    fuse_request_dequeue,
    TP_system(fuse),
    TP_PROTO(unique: u64, opcode: u32, len: u64),
    TP_STRUCT__entry {
        unique: u64,
        opcode: u32,
        len: u64,
    },
    TP_fast_assign {
        unique: unique,
        opcode: opcode,
        len: len,
    },
    TP_ident(__entry),
    TP_printk({
        let unique = __entry.unique;
        let opcode = __entry.opcode;
        let len = __entry.len;
        format!("unique={} opcode={} len={}", unique, opcode, len)
    })
);

define_event_trace!(
    fuse_reply_complete,
    TP_system(fuse),
    TP_PROTO(unique: u64, opcode: u32, error: i32, payload_len: u64),
    TP_STRUCT__entry {
        unique: u64,
        opcode: u32,
        error: i32,
        payload_len: u64,
    },
    TP_fast_assign {
        unique: unique,
        opcode: opcode,
        error: error,
        payload_len: payload_len,
    },
    TP_ident(__entry),
    TP_printk({
        let unique = __entry.unique;
        let opcode = __entry.opcode;
        let error = __entry.error;
        let payload_len = __entry.payload_len;
        format!(
            "unique={} opcode={} error={} payload_len={}",
            unique, opcode, error, payload_len
        )
    })
);

define_event_trace!(
    virtiofs_submit,
    TP_system(fuse),
    TP_PROTO(unique: u64, opcode: u32, queue_kind: u8, queue_slot: u16, token: u16, req_len: u64),
    TP_STRUCT__entry {
        unique: u64,
        opcode: u32,
        queue_kind: u8,
        queue_slot: u16,
        token: u16,
        req_len: u64,
    },
    TP_fast_assign {
        unique: unique,
        opcode: opcode,
        queue_kind: queue_kind,
        queue_slot: queue_slot,
        token: token,
        req_len: req_len,
    },
    TP_ident(__entry),
    TP_printk({
        let unique = __entry.unique;
        let opcode = __entry.opcode;
        let queue_kind = __entry.queue_kind;
        let queue_slot = __entry.queue_slot;
        let token = __entry.token;
        let req_len = __entry.req_len;
        format!(
            "unique={} opcode={} queue_kind={} queue_slot={} token={} req_len={}",
            unique, opcode, queue_kind, queue_slot, token, req_len
        )
    })
);

define_event_trace!(
    virtiofs_queue_retry,
    TP_system(fuse),
    TP_PROTO(unique: u64, opcode: u32, queue_kind: u8, queue_slot: u16, reason: u8),
    TP_STRUCT__entry {
        unique: u64,
        opcode: u32,
        queue_kind: u8,
        queue_slot: u16,
        reason: u8,
    },
    TP_fast_assign {
        unique: unique,
        opcode: opcode,
        queue_kind: queue_kind,
        queue_slot: queue_slot,
        reason: reason,
    },
    TP_ident(__entry),
    TP_printk({
        let unique = __entry.unique;
        let opcode = __entry.opcode;
        let queue_kind = __entry.queue_kind;
        let queue_slot = __entry.queue_slot;
        let reason = __entry.reason;
        format!(
            "unique={} opcode={} queue_kind={} queue_slot={} reason={}",
            unique, opcode, queue_kind, queue_slot, reason
        )
    })
);

define_event_trace!(
    virtiofs_complete,
    TP_system(fuse),
    TP_PROTO(unique: u64, opcode: u32, queue_kind: u8, queue_slot: u16, token: u16, used_len: u64),
    TP_STRUCT__entry {
        unique: u64,
        opcode: u32,
        queue_kind: u8,
        queue_slot: u16,
        token: u16,
        used_len: u64,
    },
    TP_fast_assign {
        unique: unique,
        opcode: opcode,
        queue_kind: queue_kind,
        queue_slot: queue_slot,
        token: token,
        used_len: used_len,
    },
    TP_ident(__entry),
    TP_printk({
        let unique = __entry.unique;
        let opcode = __entry.opcode;
        let queue_kind = __entry.queue_kind;
        let queue_slot = __entry.queue_slot;
        let token = __entry.token;
        let used_len = __entry.used_len;
        format!(
            "unique={} opcode={} queue_kind={} queue_slot={} token={} used_len={}",
            unique, opcode, queue_kind, queue_slot, token, used_len
        )
    })
);
