//! Host tests execute the production tree, including prepared publication and
//! detached retirement. They do not model kernel locks or IRQ scheduling.
#![feature(allocator_api)]
extern crate alloc;
extern crate self as log;
#[macro_export]
macro_rules! debug {
    ($($argument:tt)*) => {{ let _ = core::format_args!($($argument)*); }};
}
#[path = "../../kernel/src/libs/rbtree.rs"]
pub mod rbtree_impl;
mod libs {
    pub use crate::rbtree_impl as rbtree;
}

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};
thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    static LAST_ALLOCATED: Cell<usize> = const { Cell::new(0) };
    static LAST_FREED: Cell<usize> = const { Cell::new(0) };
}
struct CountedAllocator;
unsafe impl GlobalAlloc for CountedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        let pointer = unsafe { System.alloc(layout) };
        let _ = LAST_ALLOCATED.try_with(|last| last.set(pointer as usize));
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
        let _ = LAST_FREED.try_with(|last| last.set(pointer as usize));
    }
}
#[global_allocator]
static ALLOCATOR: CountedAllocator = CountedAllocator;

#[test]
fn prepared_publication_lookup_and_detachment_allocate_nothing() {
    use rbtree_impl::{PreparedNode, RBTree};
    let nodes: Vec<_> = (0..1024)
        .map(|key| PreparedNode::try_new(key, key * 2).unwrap())
        .collect();
    let mut tree = RBTree::new();
    let before = ALLOCATIONS.with(Cell::get);
    for node in nodes {
        tree.insert_prepared(node);
    }
    for key in 0..1024 {
        assert_eq!(
            tree.upper_bound(&key).map(|(&next, _)| next),
            (key < 1023).then_some(key + 1)
        );
        let node = tree.extract(&key).unwrap();
        assert_eq!(node.into_pair(), (key, key * 2));
    }
    assert_eq!(ALLOCATIONS.with(Cell::get), before);
    assert!(tree.is_empty());
}

#[test]
fn detached_storage_is_freed_before_its_value_receipt() {
    #[derive(Debug)]
    struct Receipt {
        allocation: usize,
    }
    impl Drop for Receipt {
        fn drop(&mut self) {
            assert_eq!(
                LAST_FREED.with(Cell::get),
                self.allocation,
                "resource accounting must outlive its physical allocation"
            );
        }
    }
    let mut node = rbtree_impl::PreparedNode::try_new(7, Receipt { allocation: 0 }).unwrap();
    node.value_mut().allocation = LAST_ALLOCATED.with(Cell::get);
    let mut tree = rbtree_impl::RBTree::new();
    tree.insert_prepared(node);
    drop(tree.extract(&7).unwrap().into_pair());
}
