//! Linux host 压测/长稳工具：对 slabmalloc 的 SCAllocator<ObjectPage> 执行随机 alloc/free 序列。
//!
//! 典型用法：
//! - `cargo run --release --bin slab_stress -- --iters 500000 --max-live 4096 --size 64 --seed 1`
//! - `valgrind --leak-check=full --show-leak-kinds=all target/release/slab_stress --iters 200000`
//!
//! 说明：该工具只依赖 std + crate 本身，方便在 Linux 主机上跑 valgrind/miri/stress。

use slabmalloc::*;

use rand::{rngs::SmallRng, Rng, SeedableRng};
use std::alloc::Layout;
use std::collections::HashSet;
use std::env;
use std::mem::transmute;
use std::ptr::NonNull;
use std::time::Instant;

struct Pager {
    base_pages: HashSet<*mut u8>,
}

impl Pager {
    fn new() -> Self {
        Self {
            base_pages: HashSet::with_capacity(1 << 14),
        }
    }

    fn currently_allocated(&self) -> usize {
        self.base_pages.len()
    }

    fn alloc_page(&mut self, page_size: usize) -> *mut u8 {
        let p =
            unsafe { std::alloc::alloc(Layout::from_size_align(page_size, page_size).unwrap()) };
        if p.is_null() {
            panic!("alloc_page({}) failed", page_size);
        }
        match page_size {
            ObjectPage::SIZE => {
                self.base_pages.insert(p);
            }
            _ => panic!("unsupported page size {}", page_size),
        }
        p
    }

    fn dealloc_page(&mut self, ptr: *mut u8, page_size: usize) {
        let layout = Layout::from_size_align(page_size, page_size).unwrap();
        match page_size {
            ObjectPage::SIZE => {
                assert!(
                    self.base_pages.remove(&ptr),
                    "freeing unknown page {:p}",
                    ptr
                );
            }
            _ => panic!("unsupported page size {}", page_size),
        }
        unsafe { std::alloc::dealloc(ptr, layout) };
    }

    fn allocate_page<'a>(&mut self) -> &'a mut ObjectPage<'a> {
        let raw = self.alloc_page(ObjectPage::SIZE);
        unsafe { transmute(raw as usize) }
    }

    fn release_page<'a>(&mut self, p: &'a mut ObjectPage<'a>) {
        self.dealloc_page(p as *const ObjectPage as *mut u8, ObjectPage::SIZE);
    }
}

fn arg_u64(name: &str, default: u64) -> u64 {
    let mut it = env::args().skip(1);
    while let Some(a) = it.next() {
        if a == name {
            return it
                .next()
                .unwrap_or_else(|| panic!("missing value for {}", name))
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("invalid u64 for {}", name));
        }
    }
    default
}

fn arg_usize(name: &str, default: usize) -> usize {
    arg_u64(name, default as u64) as usize
}

fn main() {
    let iters = arg_u64("--iters", 200_000) as usize;
    let max_live = arg_usize("--max-live", 4096);
    let size = arg_usize("--size", 64);
    let seed = arg_u64("--seed", 1);

    assert!(size > 0 && size <= ZoneAllocator::MAX_BASE_ALLOC_SIZE);
    let layout = Layout::from_size_align(size, 1).unwrap();

    let mut rng = SmallRng::seed_from_u64(seed);
    let mut pager = Pager::new();
    let mut sa: SCAllocator<ObjectPage> = SCAllocator::new(size);

    // 预先给一页，避免一开始就 OOM
    let page = pager.allocate_page();
    unsafe { sa.refill(page) };

    let mut live: Vec<NonNull<u8>> = Vec::with_capacity(max_live);
    let start = Instant::now();
    let mut allocs = 0usize;
    let mut frees = 0usize;
    let mut refills = 1usize;
    let mut reclaims = 0usize;

    for i in 0..iters {
        let do_alloc = live.is_empty() || (live.len() < max_live && rng.gen_bool(0.60));
        if do_alloc {
            loop {
                match sa.allocate(layout) {
                    Ok(p) => {
                        live.push(p);
                        allocs += 1;
                        break;
                    }
                    Err(AllocationError::OutOfMemory) => {
                        let page = pager.allocate_page();
                        unsafe { sa.refill(page) };
                        refills += 1;
                    }
                    Err(AllocationError::InvalidLayout) => unreachable!(),
                }
            }
        } else {
            let idx = rng.gen_range(0..live.len());
            let p = live.swap_remove(idx);
            unsafe { sa.deallocate(p, layout).expect("dealloc failed") };
            frees += 1;
        }

        // 偶尔回收空页
        if (i & 0x3fff) == 0x3fff {
            let reclaimed = sa.try_reclaim_pages(1, &mut |p: *mut ObjectPage| unsafe {
                pager.release_page(&mut *p)
            });
            if reclaimed > 0 {
                reclaims += reclaimed;
            }
        }
    }

    for p in live.drain(..) {
        unsafe { sa.deallocate(p, layout).expect("dealloc failed") };
        frees += 1;
    }

    sa.try_reclaim_pages(usize::MAX, &mut |p: *mut ObjectPage| unsafe {
        pager.release_page(&mut *p)
    });

    let dur = start.elapsed();
    println!(
        "slab_stress done: iters={} size={} allocs={} frees={} refills={} reclaims={} pages_left={} elapsed={:?}",
        iters,
        size,
        allocs,
        frees,
        refills,
        reclaims,
        pager.currently_allocated(),
        dur
    );

    assert_eq!(pager.currently_allocated(), 0, "leaked pages");
}
