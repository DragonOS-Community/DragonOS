//! Allocation-free handoff from arbitrary final-file context to a sleepable
//! perf-event release worker.

use alloc::{boxed::Box, sync::Arc};
use core::{
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use crate::{
    libs::wait_queue::WaitQueue,
    process::kthread::{KernelThreadClosure, KernelThreadMechanism},
    time::{sleep::nanosleep, PosixTimeSpec},
};

use super::PerfEventCore;

static HEAD: AtomicPtr<PerfReleaseNode> = AtomicPtr::new(ptr::null_mut());
static WAIT: WaitQueue = WaitQueue::default();
const RETRY_DELAY_NS: i64 = 10_000_000;

/// The node is a separate stable allocation. Ownership moves only in the
/// direction inode -> intrusive queue -> worker; `PerfEventCore` never owns a
/// node, so there is no self-referential Arc cycle.
#[derive(Debug)]
pub(super) struct PerfReleaseNode {
    next: AtomicPtr<PerfReleaseNode>,
    core: Arc<PerfEventCore>,
}

impl PerfReleaseNode {
    pub(super) fn new(core: Arc<PerfEventCore>) -> Self {
        Self {
            next: AtomicPtr::new(ptr::null_mut()),
            core,
        }
    }
}

pub(super) fn enqueue(node: Box<PerfReleaseNode>) {
    let node = Box::into_raw(node);
    let mut head = HEAD.load(Ordering::Acquire);
    loop {
        unsafe { (*node).next.store(head, Ordering::Relaxed) };
        match HEAD.compare_exchange_weak(head, node, Ordering::Release, Ordering::Acquire) {
            Ok(_) => break,
            Err(actual) => head = actual,
        }
    }
    WAIT.wakeup(None);
}

pub(crate) fn init() {
    let kt_closure = KernelThreadClosure::EmptyClosure((Box::new(worker_loop), ()));
    KernelThreadMechanism::create_and_run(kt_closure, "perf_release".into())
        .expect("failed to create perf release worker");
}

fn worker_loop() -> i32 {
    loop {
        WAIT.wait_until(|| {
            if HEAD.load(Ordering::Acquire).is_null() {
                None
            } else {
                Some(())
            }
        });

        let mut list = HEAD.swap(ptr::null_mut(), Ordering::Acquire);
        let mut reversed = ptr::null_mut();
        while !list.is_null() {
            let next = unsafe { (*list).next.load(Ordering::Relaxed) };
            unsafe { (*list).next.store(reversed, Ordering::Relaxed) };
            reversed = list;
            list = next;
        }

        while !reversed.is_null() {
            let node = unsafe { Box::from_raw(reversed) };
            reversed = node.next.load(Ordering::Relaxed);
            node.next.store(ptr::null_mut(), Ordering::Relaxed);
            match super::PerfEventOps::release(node.core.event.as_ref()) {
                Ok(()) => drop(node),
                Err(system_error::SystemError::ETIMEDOUT) => {
                    // The rendezvous aborts before changing text or owner
                    // state. Keep the node and callbacks alive, then retry
                    // after a short backoff without blocking unrelated releases.
                    let _ = nanosleep(PosixTimeSpec::new(0, RETRY_DELAY_NS));
                    enqueue(node);
                }
                Err(error) => {
                    // Other failures indicate an invariant breach. Dropping
                    // callbacks while a static branch remains enabled would
                    // be memory unsafe.
                    panic!("perf event release failed: {:?}", error);
                }
            }
        }
    }
}
