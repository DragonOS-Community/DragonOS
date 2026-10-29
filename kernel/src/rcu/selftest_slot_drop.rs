//! Terminal ownership and replacement are distinct RCU lifetime boundaries.
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};
use system_error::SystemError;

use super::{rcu_barrier, PreparedRcuArcRetire, RcuArcSlot, RcuOptionArcSlot};

#[derive(Debug)]
struct DropProbe(Arc<AtomicUsize>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

pub(crate) fn heap_denied_slot_drop_selftest() -> Result<bool, SystemError> {
    // Every payload, counter and callback reservation is born before denial.
    // The probe destructor itself only increments an already allocated counter.
    let counts: [Arc<AtomicUsize>; 7] = core::array::from_fn(|_| Arc::new(AtomicUsize::new(0)));
    let plain = RcuArcSlot::new(Arc::new(DropProbe(counts[0].clone())));
    let optional = RcuOptionArcSlot::new_some(Arc::new(DropProbe(counts[1].clone())));
    let empty = RcuOptionArcSlot::<DropProbe>::new_none();
    let pinned_plain = RcuArcSlot::new(Arc::new(DropProbe(counts[2].clone())));
    let plain_pin = pinned_plain.load();
    let pinned_optional = RcuOptionArcSlot::new_some(Arc::new(DropProbe(counts[3].clone())));
    let optional_pin = pinned_optional.load().ok_or(SystemError::EINVAL)?;
    let mut taken = RcuArcSlot::new(Arc::new(DropProbe(counts[4].clone())));
    let replacement = RcuArcSlot::new(Arc::new(DropProbe(counts[5].clone())));
    let next = Arc::new(DropProbe(counts[6].clone()));
    let prepared = PreparedRcuArcRetire::prepare().map_err(|_| SystemError::ENOMEM)?;

    let checked = {
        let _scope = crate::mm::oom::denial::Scope::enter(true);
        let entry_denied = crate::mm::oom::denial::probe_denied();
        drop(plain);
        drop(optional);
        drop(empty);
        drop(pinned_plain);
        drop(pinned_optional);
        let pinned_survived =
            counts[2].load(Ordering::SeqCst) == 0 && counts[3].load(Ordering::SeqCst) == 0;
        drop(plain_pin);
        drop(optional_pin);
        // SAFETY: this private slot has no readers and is never used again.
        drop(unsafe { taken.take_for_drop() });
        drop(taken);

        let replacement_protected = replacement.with_read(|old| {
            replacement.swap_prepared(next, prepared).enqueue();
            old.0.load(Ordering::SeqCst) == 0
        });
        drop(replacement);
        let terminal_once = [0, 1, 2, 3, 4, 6]
            .iter()
            .all(|index| counts[*index].load(Ordering::SeqCst) == 1);
        let exit_denied = crate::mm::oom::denial::probe_denied();
        entry_denied && pinned_survived && replacement_protected && terminal_once && exit_denied
    };
    // Sleepable GP completion belongs outside heap denial. The replaced owner
    // remains on its existing RCU retirement path, unlike terminal destruction.
    rcu_barrier();
    Ok(checked && counts[5].load(Ordering::SeqCst) == 1)
}
