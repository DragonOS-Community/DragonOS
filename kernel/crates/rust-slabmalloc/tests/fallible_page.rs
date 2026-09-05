//! An isolated host allocator test: failure affects only the calling test thread.
use slabmalloc::ObjectPage;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static FAIL_NEXT_PAGE: Cell<bool> = const { Cell::new(false) };
}

struct PageFailureAllocator;

unsafe impl GlobalAlloc for PageFailureAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let fail = layout == Layout::new::<ObjectPage<'static>>()
            && FAIL_NEXT_PAGE
                .try_with(|armed| armed.replace(false))
                .unwrap_or(false);
        if fail {
            std::ptr::null_mut()
        } else {
            System.alloc(layout)
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }
}

#[global_allocator]
static ALLOCATOR: PageFailureAllocator = PageFailureAllocator;

#[test]
fn backing_page_failure_returns_error_and_can_retry() {
    FAIL_NEXT_PAGE.with(|armed| armed.set(true));
    let failed_page = ObjectPage::try_new();
    let failure_consumed = FAIL_NEXT_PAGE.with(|armed| !armed.replace(false));
    assert!(
        failure_consumed,
        "the backing-page allocation was intercepted"
    );
    assert!(failed_page.is_err(), "OOM must return instead of aborting");

    let page = ObjectPage::try_new().expect("the next backing allocation succeeds");
    let addr = page.as_ref() as *const ObjectPage<'_> as usize;
    assert_eq!(addr % std::mem::align_of::<ObjectPage<'_>>(), 0);
    drop(page);

    // Existing callers still use the same metadata initialization through new().
    drop(ObjectPage::new());
}
