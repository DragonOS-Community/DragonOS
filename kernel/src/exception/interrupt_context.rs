use core::{
    marker::PhantomData,
    sync::atomic::{AtomicU32, Ordering},
};

use crate::{
    mm::percpu::PerCpu,
    smp::{core::smp_get_processor_id, cpu::ProcessorId},
};

static HARDIRQ_DEPTH: [AtomicU32; PerCpu::MAX_CPU_NUM as usize] =
    [const { AtomicU32::new(0) }; PerCpu::MAX_CPU_NUM as usize];

/// Marks the current CPU as executing a hard interrupt handler.
///
/// This accounting is deliberately lock-free: interrupt-context predicates
/// must remain usable from lock slow paths and from nested IRQ handlers.
#[must_use]
pub struct HardirqContextGuard {
    cpu_id: ProcessorId,
    _not_send: PhantomData<*mut ()>,
}

#[inline]
fn depth(cpu_id: ProcessorId) -> &'static AtomicU32 {
    &HARDIRQ_DEPTH[cpu_id.data() as usize]
}

#[inline]
pub(crate) fn enter_hardirq() -> HardirqContextGuard {
    let cpu_id = smp_get_processor_id();
    depth(cpu_id)
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
            old.checked_add(1)
        })
        .expect("hardirq nesting depth overflow");
    HardirqContextGuard {
        cpu_id,
        _not_send: PhantomData,
    }
}

#[inline]
pub fn in_hardirq() -> bool {
    depth(smp_get_processor_id()).load(Ordering::Relaxed) != 0
}

#[inline]
pub fn in_interrupt() -> bool {
    in_hardirq() || super::softirq::in_softirq()
}

impl Drop for HardirqContextGuard {
    fn drop(&mut self) {
        assert_eq!(smp_get_processor_id(), self.cpu_id);
        depth(self.cpu_id)
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                old.checked_sub(1)
            })
            .expect("unbalanced hardirq context exit");
    }
}
