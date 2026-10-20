//! Private, never-loaded MM fixtures exercise the actual intrusive queue.
use super::{Queue, QUEUE};
use crate::{
    cgroup::memory::{object::ObjectAccount, MemoryAccount},
    mm::{oom::denial, ucontext::AddressSpace},
    process::{namespace::user_namespace::INIT_USER_NAMESPACE, ProcessManager},
};
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};
use system_error::SystemError;

static NEXT_ACCOUNT: AtomicU64 = AtomicU64::new(0x0171_0000_0000_0000);
const COUNT: usize = 65;

pub(super) fn run() -> Result<bool, SystemError> {
    let account = MemoryAccount::new(NEXT_ACCOUNT.fetch_add(1, Ordering::Relaxed), None)?;
    let creator = ObjectAccount::new(&account)?;
    let owner = ProcessManager::current_pcb();
    let mut owners = Vec::new();
    let mut weak = Vec::new();
    owners
        .try_reserve_exact(COUNT)
        .map_err(|_| SystemError::ENOMEM)?;
    weak.try_reserve_exact(COUNT)
        .map_err(|_| SystemError::ENOMEM)?;
    for _ in 0..COUNT {
        let mm = AddressSpace::new_for_task_with_creator(
            &owner,
            false,
            INIT_USER_NAMESPACE.clone(),
            1,
            &creator,
        )
        .map_err(|_| SystemError::ENOMEM)?;
        // These real MMs are never installed or loaded. Complete the normal
        // last-user work before testing the remaining metadata-owner queue.
        mm.mmput();
        weak.push(Arc::downgrade(&mm));
        owners.push(mm);
    }
    let charged = account.current_pages() > 0
        && account
            .snapshot()
            .kernel_bytes
            .iter()
            .any(|bytes| *bytes > 0);
    let originally_denied = denial::is_current_denied();
    let mut local = Queue::new();
    let (mut batch, mut second, coalesced, probes) = {
        let _scope = denial::Scope::enter(true);
        let before = denial::probe_denied();
        let (mut batch, coalesced) = {
            // The production lock also serializes links in private queues and
            // detached batches. None of these fixture nodes enter global work.
            let _queue = QUEUE.lock_irqsave();
            for mm in owners.drain(..) {
                local.push(mm);
            }
            let mut coalesced = true;
            if let Some(duplicate) = weak[0].upgrade() {
                local.push(duplicate);
            } else {
                coalesced = false;
            }
            let batch = local.take_all();
            coalesced &= local.head.is_none();
            // queued remains true while the original owner is in the taken
            // batch. The duplicate must not be inserted into the empty queue.
            if let Some(duplicate) = weak[0].upgrade() {
                local.push(duplicate);
            } else {
                coalesced = false;
            }
            coalesced &= local.head.is_none();
            (batch, coalesced)
        };
        let mut retired = 0;
        let mut requeued = false;
        for _ in 0..COUNT + 1 {
            let mm = {
                let _queue = QUEUE.lock_irqsave();
                batch.pop()
            };
            let Some(mm) = mm else { break };
            if retired == 0 {
                // pop cleared queued and detached next. A new owner can now
                // enter the next batch while this old batch Arc stays live.
                let _queue = QUEUE.lock_irqsave();
                local.push(mm.clone());
                requeued = true;
            }
            drop(mm);
            retired += 1;
        }
        let (mut second, first_empty) = {
            let _queue = QUEUE.lock_irqsave();
            (local.take_all(), batch.head.is_none())
        };
        let mut second_count = 0;
        for _ in 0..COUNT + 1 {
            let mm = {
                let _queue = QUEUE.lock_irqsave();
                second.pop()
            };
            let Some(mm) = mm else { break };
            drop(mm);
            second_count += 1;
        }
        let second_empty = second.head.is_none();
        let after = denial::probe_denied();
        // Carry any failed fixture remainder out of denial before cleanup.
        (
            batch,
            second,
            coalesced
                && requeued
                && retired == COUNT
                && first_empty
                && second_count == 1
                && second_empty,
            before && after,
        )
    };
    // Normally both queues are empty. Never let failure cleanup recursively
    // drop a whole linked chain, or run it in the tested denial scope.
    for _ in 0..COUNT + 1 {
        let mm = {
            let _queue = QUEUE.lock_irqsave();
            batch.pop().or_else(|| second.pop()).or_else(|| local.pop())
        };
        let Some(mm) = mm else { break };
        drop(mm);
    }
    let snapshot = account.snapshot();
    Ok(charged
        && coalesced
        && probes
        && denial::is_current_denied() == originally_denied
        && weak.iter().all(|mm| mm.upgrade().is_none())
        && account.current_pages() == 0
        && snapshot.kernel_bytes.iter().all(|bytes| *bytes == 0))
}
