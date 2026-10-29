//! Real, thread-local allocation failure injection for host unit tests.
//! Other test threads and the test harness retain the system allocator.
use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use std::alloc::System;

std::thread_local! {
    static REMAINING: Cell<Option<usize>> = const { Cell::new(None) };
    static ATTEMPTS: Cell<usize> = const { Cell::new(0) };
}

struct TestAllocator;

#[global_allocator]
static ALLOCATOR: TestAllocator = TestAllocator;

fn reject() -> bool {
    REMAINING.with(|remaining| match remaining.get() {
        None => false,
        Some(value) => {
            ATTEMPTS.with(|attempts| attempts.set(attempts.get() + 1));
            if value == 0 {
                true
            } else {
                remaining.set(Some(value - 1));
                false
            }
        }
    })
}

unsafe impl GlobalAlloc for TestAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if reject() {
            core::ptr::null_mut()
        } else {
            System.alloc(layout)
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if reject() {
            core::ptr::null_mut()
        } else {
            System.alloc_zeroed(layout)
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if reject() {
            core::ptr::null_mut()
        } else {
            System.realloc(ptr, layout, size)
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }
}

/// Restores allocation before callers format/assert their test results.
pub(crate) struct FailureScope;

impl FailureScope {
    pub(crate) fn new(successful_allocations: usize) -> Self {
        REMAINING.with(|remaining| {
            assert!(remaining.get().is_none(), "allocation scopes must not nest");
            remaining.set(Some(successful_allocations));
        });
        ATTEMPTS.with(|attempts| attempts.set(0));
        Self
    }

    pub(crate) fn attempts(&self) -> usize {
        ATTEMPTS.with(Cell::get)
    }
}

pub(crate) fn reject_following_allocations() {
    REMAINING.with(|remaining| remaining.set(Some(0)));
}

impl Drop for FailureScope {
    fn drop(&mut self) {
        REMAINING.with(|remaining| remaining.set(None));
    }
}
