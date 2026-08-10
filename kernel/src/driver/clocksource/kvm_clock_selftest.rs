use alloc::{format, string::String, vec::Vec};

use crate::smp::cpu::ProcessorId;

use super::*;

struct FakeAllocator {
    allocate_calls: usize,
    map_calls: usize,
    clear_calls: usize,
    fail_allocate_at: Option<usize>,
    fail_map_at: Option<usize>,
    released: Vec<usize>,
}

impl FakeAllocator {
    fn new() -> Self {
        Self {
            allocate_calls: 0,
            map_calls: 0,
            clear_calls: 0,
            fail_allocate_at: None,
            fail_map_at: None,
            released: Vec::new(),
        }
    }
}

impl ClockPageAllocator for FakeAllocator {
    fn allocate_frame(&mut self) -> Option<PhysAddr> {
        self.allocate_calls += 1;
        if self.fail_allocate_at == Some(self.allocate_calls) {
            return None;
        }
        Some(PhysAddr::new(self.allocate_calls * 0x1000))
    }

    fn map_frame(&mut self, phys: PhysAddr) -> Option<VirtAddr> {
        self.map_calls += 1;
        if self.fail_map_at == Some(self.map_calls) {
            return None;
        }
        Some(VirtAddr::new(0xffff_8000_0000_0000usize + phys.data()))
    }

    fn clear_page(&mut self, _virt: VirtAddr) {
        self.clear_calls += 1;
    }

    fn release_frame(&mut self, phys: PhysAddr) {
        self.released.push(phys.data());
    }
}

struct Report {
    text: String,
    passed: usize,
    failed: usize,
}

impl Report {
    fn new() -> Self {
        Self {
            text: String::new(),
            passed: 0,
            failed: 0,
        }
    }

    fn case(&mut self, name: &str, ok: bool) {
        if ok {
            self.passed += 1;
            self.text.push_str(&format!("kvm_allocator.{name}=ok\n"));
        } else {
            self.failed += 1;
            self.text.push_str(&format!("kvm_allocator.{name}=fail\n"));
        }
    }
}

pub(crate) fn run_kvm_clock_allocator_selftests() -> (usize, usize, String) {
    let mut report = Report::new();
    let cpus = [
        ProcessorId::new(0),
        ProcessorId::new(2),
        ProcessorId::new(7),
    ];

    let mut empty_allocator = FakeAllocator::new();
    let empty = allocate_missing_pages(&[], &mut empty_allocator);
    report.case(
        "empty",
        empty.as_ref().is_ok_and(Vec::is_empty)
            && empty_allocator.allocate_calls == 0
            && empty_allocator.released.is_empty(),
    );

    let mut success_allocator = FakeAllocator::new();
    let success = allocate_missing_pages(&cpus, &mut success_allocator);
    let success_ok = success.as_ref().is_ok_and(|pages| {
        pages.len() == cpus.len()
            && pages
                .iter()
                .zip(cpus.iter())
                .all(|((actual_cpu, page), expected_cpu)| {
                    actual_cpu == expected_cpu && page.is_valid()
                })
    });
    report.case(
        "staged_success",
        success_ok
            && success_allocator.allocate_calls == cpus.len()
            && success_allocator.map_calls == cpus.len()
            && success_allocator.clear_calls == cpus.len()
            && success_allocator.released.is_empty(),
    );

    let mut all_allocation_failures_ok = true;
    for fail_at in 1..=cpus.len() {
        let mut allocator = FakeAllocator::new();
        allocator.fail_allocate_at = Some(fail_at);
        let result = allocate_missing_pages(&cpus, &mut allocator);
        all_allocation_failures_ok &= matches!(result, Err(SystemError::ENOMEM))
            && allocator.released.len() == fail_at - 1
            && allocator.clear_calls == fail_at - 1;
    }
    report.case("allocation_failure_rollback", all_allocation_failures_ok);

    let mut all_mapping_failures_ok = true;
    for fail_at in 1..=cpus.len() {
        let mut allocator = FakeAllocator::new();
        allocator.fail_map_at = Some(fail_at);
        let result = allocate_missing_pages(&cpus, &mut allocator);
        all_mapping_failures_ok &= matches!(result, Err(SystemError::EINVAL))
            && allocator.released.len() == fail_at
            && allocator.clear_calls == fail_at - 1;
        allocator.released.sort_unstable();
        allocator.released.dedup();
        all_mapping_failures_ok &= allocator.released.len() == fail_at;
    }
    report.case("mapping_failure_releases_frame", all_mapping_failures_ok);

    let mut retry_allocator = FakeAllocator::new();
    retry_allocator.fail_allocate_at = Some(2);
    let first = allocate_missing_pages(&cpus, &mut retry_allocator);
    retry_allocator.fail_allocate_at = None;
    let second = allocate_missing_pages(&cpus, &mut retry_allocator);
    report.case(
        "retry_after_rollback",
        matches!(first, Err(SystemError::ENOMEM))
            && second.as_ref().is_ok_and(|pages| pages.len() == cpus.len())
            && retry_allocator.released.len() == 1,
    );

    (report.passed, report.failed, report.text)
}
