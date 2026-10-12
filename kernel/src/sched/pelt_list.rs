//! Owner-rq PELT list. Each CFS queue prepares and reuses exactly one node.
use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};

use super::fair::CfsRunQueue;

#[derive(Debug)]
pub(super) struct PeltNode {
    pub(super) queue: Weak<CfsRunQueue>,
    pub(super) next: Option<Box<PeltNode>>,
}

impl PeltNode {
    pub(super) fn try_new() -> Result<Box<Self>, core::alloc::AllocError> {
        Box::try_new(Self {
            queue: Weak::new(),
            next: None,
        })
    }
}

impl Drop for PeltNode {
    fn drop(&mut self) {
        let mut next = self.next.take();
        while let Some(mut node) = next {
            next = node.next.take();
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct PeltList {
    head: Option<Box<PeltNode>>,
}

pub(super) enum PeltDisposition {
    Keep,
    Recycle(Arc<CfsRunQueue>),
    Retire,
}

impl PeltList {
    /// Insert a child immediately before its listed parent, or append a root.
    /// Raw pointers are used only for Weak identity, never dereferenced.
    pub(super) fn insert_before(
        &mut self,
        mut node: Box<PeltNode>,
        parent: Option<*const CfsRunQueue>,
    ) {
        debug_assert!(node.next.is_none());
        let mut link = &mut self.head;
        while link
            .as_ref()
            .is_some_and(|entry| Some(entry.queue.as_ptr()) != parent)
        {
            link = &mut link.as_mut().unwrap().next;
        }
        node.next = link.take();
        *link = Some(node);
    }

    pub(super) fn remove(&mut self, queue: *const CfsRunQueue) -> Option<Box<PeltNode>> {
        let mut link = &mut self.head;
        while let Some(entry) = link.as_ref() {
            if entry.queue.as_ptr() == queue {
                let mut node = link.take().unwrap();
                *link = node.next.take();
                return Some(node);
            }
            link = &mut link.as_mut().unwrap().next;
        }
        None
    }

    /// The visitor must not reenter or mutate this list. Live nodes return to
    /// their original queue; expired weak nodes are returned for lock-outside
    /// retirement, since their Weak may own the last allocation reference.
    pub(super) fn sweep(
        &mut self,
        mut visit: impl FnMut(&Weak<CfsRunQueue>) -> PeltDisposition,
    ) -> Option<Box<PeltNode>> {
        let mut retired = None;
        let mut link = &mut self.head;
        while let Some(entry) = link.as_ref() {
            match visit(&entry.queue) {
                PeltDisposition::Keep => link = &mut link.as_mut().unwrap().next,
                action => {
                    let mut node = link.take().unwrap();
                    *link = node.next.take();
                    match action {
                        PeltDisposition::Recycle(queue) => queue.force_mut().return_pelt_node(node),
                        PeltDisposition::Retire => {
                            node.next = retired;
                            retired = Some(node);
                        }
                        PeltDisposition::Keep => unreachable!(),
                    }
                }
            }
        }
        retired
    }
}
