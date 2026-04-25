use proptest::prelude::*;

use crate::*;

/// 随机序列/属性测试：在“可按需 refill 页面”的模型下，反复 alloc/free 不应崩溃，
/// 且最终可把所有页面回收到 pager（无泄漏）。
///
/// 说明：
/// - 这里用的是 SCAllocator<ObjectPage>，避开 ZoneAllocator 的回调复杂度，聚焦页状态迁移与位图正确性。
/// - 该测试只在 Linux host 上作为 dev/test 运行（cargo test）。
mod prop {
    use super::*;
    use rand::{rngs::SmallRng, Rng, SeedableRng};
    use std::alloc::Layout;
    use std::collections::HashSet;
    use std::mem::transmute;
    use std::ptr::NonNull;
    use std::vec::Vec;

    struct Pager {
        base_pages: HashSet<*mut u8>,
    }

    impl Pager {
        fn new() -> Self {
            Self {
                base_pages: HashSet::with_capacity(4096),
            }
        }

        fn currently_allocated(&self) -> usize {
            self.base_pages.len()
        }

        fn alloc_page(&mut self, page_size: usize) -> Option<*mut u8> {
            let r = unsafe {
                std::alloc::alloc(Layout::from_size_align(page_size, page_size).unwrap())
            };
            if r.is_null() {
                return None;
            }
            match page_size {
                OBJECT_PAGE_SIZE => {
                    self.base_pages.insert(r);
                }
                _ => unreachable!("invalid page-size supplied"),
            }
            Some(r)
        }

        fn dealloc_page(&mut self, ptr: *mut u8, page_size: usize) {
            let layout = match page_size {
                OBJECT_PAGE_SIZE => {
                    assert!(
                        self.base_pages.contains(&ptr),
                        "Trying to deallocate invalid base-page"
                    );
                    self.base_pages.remove(&ptr);
                    Layout::from_size_align(OBJECT_PAGE_SIZE, OBJECT_PAGE_SIZE).unwrap()
                }
                _ => unreachable!("invalid page-size supplied"),
            };
            unsafe { std::alloc::dealloc(ptr, layout) };
        }

        fn allocate_page<'a>(&mut self) -> Option<&'a mut ObjectPage<'a>> {
            self.alloc_page(OBJECT_PAGE_SIZE)
                .map(|r| unsafe { transmute(r as usize) })
        }

        fn release_page<'a>(&mut self, p: &'a mut ObjectPage<'a>) {
            self.dealloc_page(p as *const ObjectPage as *mut u8, OBJECT_PAGE_SIZE);
        }
    }

    proptest! {
        // 控制规模：避免 CI / 本机跑太久
        #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]

        #[test]
        fn prop_random_alloc_free_sequence(seed in any::<u64>(), ops in 200usize..2000usize) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let mut pager = Pager::new();

            // 在多个 size class 上覆盖（只测 base page 范围）
            let sizes = [8usize, 16, 32, 64, 128, 256, 512, 1024, 2048];
            let size = sizes[rng.gen_range(0..sizes.len())];
            let mut sa: SCAllocator<ObjectPage> = SCAllocator::new(size);
            let layout = Layout::from_size_align(size, 1).unwrap();

            // live objects
            let mut live: Vec<NonNull<u8>> = Vec::new();

            // 确保至少有一页可用
            let page = pager.allocate_page().expect("Can't allocate a page");
            unsafe { sa.refill(page) };

            for _ in 0..ops {
                let do_alloc = live.is_empty() || rng.gen_bool(0.60);
                if do_alloc {
                    loop {
                        match sa.allocate(layout) {
                            Ok(p) => {
                                live.push(p);
                                break;
                            }
                            Err(AllocationError::OutOfMemory) => {
                                let page = pager.allocate_page().expect("Can't allocate a page");
                                unsafe { sa.refill(page) };
                                continue;
                            }
                            Err(AllocationError::InvalidLayout) => unreachable!("Unexpected error"),
                        }
                    }
                } else {
                    let idx = rng.gen_range(0..live.len());
                    let p = live.swap_remove(idx);
                    unsafe { sa.deallocate(p, layout).expect("Can't deallocate") };
                }

                // 偶尔尝试回收空页（模拟内存压力路径）
                if rng.gen_bool(0.05) {
                    sa.try_reclaim_pages(1, &mut |p: *mut ObjectPage| unsafe {
                        pager.release_page(&mut *p)
                    });
                }
            }

            // 全部释放，确保可以回收到 pager
            for p in live.drain(..) {
                unsafe { sa.deallocate(p, layout).expect("Can't deallocate") };
            }

            sa.try_reclaim_pages(usize::MAX, &mut |p: *mut ObjectPage| unsafe {
                pager.release_page(&mut *p)
            });

            prop_assert_eq!(pager.currently_allocated(), 0);
        }
    }
}
