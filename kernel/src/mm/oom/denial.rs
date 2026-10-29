//! Feature-only proof that OOM recovery can proceed with the ordinary heap denied.

use crate::process::{ProcessControlBlock, ProcessFlags, ProcessManager};
use alloc::sync::Arc;
use core::{
    alloc::{GlobalAlloc, Layout},
    sync::atomic::{AtomicUsize, Ordering},
};

static REJECTIONS: AtomicUsize = AtomicUsize::new(0);

/// The trigger restores its original flags on return; committed victims keep
/// denial through core-MM retirement, including when the trigger is the victim.
pub(crate) struct Scope {
    task: Option<Arc<ProcessControlBlock>>,
    previous: ProcessFlags,
}

impl Scope {
    pub(crate) fn enter(enabled: bool) -> Self {
        let task = enabled.then(ProcessManager::current_pcb);
        let previous = task.as_ref().map_or(ProcessFlags::empty(), |task| {
            let previous = task.flags().load() & test_flags();
            task.flags().insert(ProcessFlags::OOM_HEAP_DENIED);
            previous
        });
        Self { task, previous }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.flags()
                .finish_oom_heap_scope(self.previous.contains(ProcessFlags::OOM_HEAP_DENIED));
        }
    }
}

fn test_flags() -> ProcessFlags {
    ProcessFlags::OOM_HEAP_DENIED | ProcessFlags::OOM_HEAP_VICTIM
}

/// End the victim's core-MM proof after PTE/Page/PT retirement and its TLB
/// barriers, before arbitrary VMA/filesystem callbacks. This is not a promise
/// that every VFS close, I/O or unrelated exit callback is allocation-free.
pub(crate) fn end_victim_core_retirement() {
    let task = ProcessManager::current_pcb();
    if task
        .flags()
        .contains(ProcessFlags::OOM_HEAP_VICTIM | ProcessFlags::EXITING)
        && task.with_task_lock_irqsave(|| task.basic().user_vm().is_none())
    {
        // An OOM workspace may retire an unrelated dead MM while the caller
        // still owns its live image. Only its own exit-MM phase can end proof.
        task.flags().remove(test_flags());
    }
}

/// Called for each actually committed kill member, never merely a candidate.
pub(crate) fn mark_victim(task: &Arc<ProcessControlBlock>) {
    if is_current_denied() {
        task.flags().insert(test_flags());
    }
}

/// No task registry, lock, formatting or allocation is permitted in this hook.
pub(crate) fn is_current_denied() -> bool {
    ProcessManager::initialized()
        && ProcessManager::current_pcb()
            .flags()
            .contains(ProcessFlags::OOM_HEAP_DENIED)
}

pub(crate) fn note_rejection() {
    REJECTIONS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn rejected_count() -> usize {
    REJECTIONS.load(Ordering::Relaxed)
}

/// Exercise both real GlobalAlloc entry points, rather than an empty Vec or a
/// page-fault-only injection. Unexpected successful probes are returned safely.
pub(crate) fn probe_denied() -> bool {
    let allocator = crate::mm::allocator::kernel_allocator::KernelAllocator;
    let layout = Layout::from_size_align(8, 8).expect("fixed probe layout");
    unsafe {
        let ordinary = allocator.alloc(layout);
        let zeroed = allocator.alloc_zeroed(layout);
        let denied = ordinary.is_null() && zeroed.is_null();
        if !ordinary.is_null() {
            allocator.dealloc(ordinary, layout);
        }
        if !zeroed.is_null() {
            allocator.dealloc(zeroed, layout);
        }
        denied
    }
}
