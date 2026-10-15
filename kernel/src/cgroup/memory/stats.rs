//! Linux 6.6 memory ABI formatting, with no allocation after fallible reserve.
//!
//! Page and byte counters are kept distinct. This module never samples tasks,
//! changes policy, or invents usage from the shared kernel reservation budget.

use alloc::{string::String, vec::Vec};
use core::fmt::Write;
use system_error::SystemError;

use super::accounting::{KernelByteKind, MemorySnapshot, MemoryStat};
use crate::{arch::MMArch, mm::MemoryManagementArch};

const EVENT_NAMES: [&str; 6] = ["low", "high", "max", "oom", "oom_kill", "oom_group_kill"];

pub(crate) fn format_limit(pages: Option<u64>) -> Result<Vec<u8>, SystemError> {
    if let Some(pages) = pages {
        return format_page_count(pages);
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(4)
        .map_err(|_| SystemError::ENOMEM)?;
    output.extend_from_slice(b"max\n");
    Ok(output)
}

pub(crate) fn format_oom_group(enabled: bool) -> Result<Vec<u8>, SystemError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(2)
        .map_err(|_| SystemError::ENOMEM)?;
    output.extend_from_slice(if enabled { b"1\n" } else { b"0\n" });
    Ok(output)
}

pub(crate) fn format_page_count(pages: u64) -> Result<Vec<u8>, SystemError> {
    let bytes = pages
        .checked_mul(MMArch::PAGE_SIZE as u64)
        .ok_or(SystemError::EOVERFLOW)?;
    let mut output = String::new();
    output
        .try_reserve_exact(21)
        .map_err(|_| SystemError::ENOMEM)?;
    writeln!(&mut output, "{bytes}").map_err(|_| SystemError::ENOMEM)?;
    Ok(output.into_bytes())
}

fn format_pairs(pairs: &[(&str, u64)]) -> Result<Vec<u8>, SystemError> {
    let capacity = pairs
        .iter()
        .try_fold(0usize, |total, (name, _)| {
            total.checked_add(name.len() + 1 + 20 + 1)
        })
        .ok_or(SystemError::ENOMEM)?;
    let mut output = String::new();
    output
        .try_reserve_exact(capacity)
        .map_err(|_| SystemError::ENOMEM)?;
    for (name, value) in pairs {
        // Every line fits the reservation, even for u64::MAX. String's writer
        // cannot fail except through allocation, which the bound excludes.
        writeln!(&mut output, "{name} {value}").map_err(|_| SystemError::ENOMEM)?;
    }
    Ok(output.into_bytes())
}

pub(crate) fn format_events(
    snapshot: &MemorySnapshot,
    local: bool,
) -> Result<Vec<u8>, SystemError> {
    let values = if local {
        &snapshot.events_local
    } else {
        &snapshot.events
    };
    let pairs = core::array::from_fn::<_, 6, _>(|i| (EVENT_NAMES[i], values[i]));
    format_pairs(&pairs)
}

pub(crate) fn format_stats(snapshot: &MemorySnapshot) -> Result<Vec<u8>, SystemError> {
    let pages = |value: u64| value.saturating_mul(MMArch::PAGE_SIZE as u64);
    let stat = |kind: MemoryStat| snapshot.stats[kind as usize];
    let bytes = |kind: KernelByteKind| snapshot.kernel_bytes[kind as usize];
    let slab_reclaimable = bytes(KernelByteKind::SlabReclaimable);
    let slab_unreclaimable = bytes(KernelByteKind::SlabUnreclaimable);
    let slab = slab_reclaimable.saturating_add(slab_unreclaimable);
    let kernel = bytes(KernelByteKind::Stack)
        .saturating_add(bytes(KernelByteKind::PageTables))
        .saturating_add(bytes(KernelByteKind::KernelOther))
        .saturating_add(bytes(KernelByteKind::Vmalloc))
        .saturating_add(slab);
    format_pairs(&[
        ("anon", pages(snapshot.anon)),
        // Accounting includes shmem in file already; do not count it twice.
        ("file", pages(snapshot.file)),
        ("kernel", kernel),
        ("kernel_stack", bytes(KernelByteKind::Stack)),
        ("pagetables", bytes(KernelByteKind::PageTables)),
        // DragonOS has no secondary MMU/per-cpu memcg allocation class.
        ("sec_pagetables", 0),
        ("percpu", 0),
        ("sock", pages(snapshot.sock)),
        ("vmalloc", bytes(KernelByteKind::Vmalloc)),
        ("shmem", pages(snapshot.shmem)),
        ("file_mapped", pages(stat(MemoryStat::FileMapped))),
        ("file_dirty", pages(stat(MemoryStat::FileDirty))),
        ("file_writeback", pages(stat(MemoryStat::FileWriteback))),
        ("inactive_anon", pages(stat(MemoryStat::InactiveAnon))),
        ("active_anon", pages(stat(MemoryStat::ActiveAnon))),
        ("inactive_file", pages(stat(MemoryStat::InactiveFile))),
        ("active_file", pages(stat(MemoryStat::ActiveFile))),
        ("unevictable", pages(stat(MemoryStat::Unevictable))),
        ("slab_reclaimable", slab_reclaimable),
        ("slab_unreclaimable", slab_unreclaimable),
        ("slab", slab),
        // There is no shadow/refault working-set algorithm in the current LRU;
        // these algorithm events do not occur. This is not adaptive-LRU support.
        ("workingset_refault_anon", 0),
        ("workingset_refault_file", 0),
        ("workingset_activate_anon", 0),
        ("workingset_activate_file", 0),
        ("workingset_restore_anon", 0),
        ("workingset_restore_file", 0),
        ("workingset_nodereclaim", 0),
        ("pgscan", stat(MemoryStat::PgScan)),
        ("pgsteal", stat(MemoryStat::PgSteal)),
        ("pgfault", stat(MemoryStat::PgFault)),
        ("pgmajfault", stat(MemoryStat::PgMajFault)),
        ("pgrefill", stat(MemoryStat::PgRefill)),
        ("pgactivate", stat(MemoryStat::PgActivate)),
        ("pgdeactivate", stat(MemoryStat::PgDeactivate)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(text: &[u8], key: &str) -> u64 {
        core::str::from_utf8(text)
            .unwrap()
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(' ')?;
                (name == key).then(|| value.parse().unwrap())
            })
            .unwrap()
    }

    #[test]
    fn real_bytes_not_shared_kernel_budget_or_sock() {
        let mut snapshot = MemorySnapshot::default();
        snapshot.kernel_object = 500;
        snapshot.kernel_stack = 500;
        snapshot.pagetables = 500;
        snapshot.sock = 3;
        snapshot.kernel_bytes = [8192, 4096, 7, 19, 123];
        let output = format_stats(&snapshot).unwrap();
        assert_eq!(value(&output, "kernel"), 8192 + 4096 + 26 + 123);
        assert_eq!(value(&output, "slab"), 26);
        assert_eq!(value(&output, "kernel_stack"), 8192);
        assert_eq!(value(&output, "sock"), 3 * MMArch::PAGE_SIZE as u64);
    }

    #[test]
    fn page_states_and_events_have_different_units() {
        let mut snapshot = MemorySnapshot::default();
        snapshot.anon = 2;
        snapshot.file = 5;
        snapshot.shmem = 3;
        snapshot.stats[MemoryStat::FileMapped as usize] = 4;
        snapshot.stats[MemoryStat::ActiveFile as usize] = 2;
        snapshot.stats[MemoryStat::PgFault as usize] = 29;
        let output = format_stats(&snapshot).unwrap();
        assert_eq!(value(&output, "file"), 5 * MMArch::PAGE_SIZE as u64);
        assert_eq!(value(&output, "shmem"), 3 * MMArch::PAGE_SIZE as u64);
        assert_eq!(value(&output, "file_mapped"), 4 * MMArch::PAGE_SIZE as u64);
        assert_eq!(value(&output, "active_file"), 2 * MMArch::PAGE_SIZE as u64);
        assert_eq!(value(&output, "pgfault"), 29);
        assert!(!core::str::from_utf8(&output)
            .unwrap()
            .contains("swapcached"));
        assert!(!core::str::from_utf8(&output).unwrap().contains("anon_thp"));
    }

    #[test]
    fn local_and_hierarchical_events_do_not_share_values() {
        let mut snapshot = MemorySnapshot::default();
        snapshot.events = [11, 12, 13, 14, 15, 16];
        snapshot.events_local = [1, 2, 3, 4, 5, 6];
        assert_eq!(
            format_events(&snapshot, false).unwrap(),
            b"low 11\nhigh 12\nmax 13\noom 14\noom_kill 15\noom_group_kill 16\n"
        );
        assert_eq!(
            format_events(&snapshot, true).unwrap(),
            b"low 1\nhigh 2\nmax 3\noom 4\noom_kill 5\noom_group_kill 6\n"
        );
    }

    #[test]
    fn reservation_covers_largest_integer_and_conversion_saturates() {
        let output = format_pairs(&[("largest", u64::MAX)]).unwrap();
        assert_eq!(output, b"largest 18446744073709551615\n");
        let mut snapshot = MemorySnapshot::default();
        snapshot.anon = u64::MAX;
        snapshot.kernel_bytes =
            [u64::MAX; crate::cgroup::memory::accounting::KERNEL_BYTE_KIND_COUNT];
        let output = format_stats(&snapshot).unwrap();
        assert_eq!(value(&output, "anon"), u64::MAX);
        assert_eq!(value(&output, "kernel"), u64::MAX);
    }

    #[test]
    fn numeric_usage_is_bytes_and_never_silently_wraps() {
        assert_eq!(format_page_count(0).unwrap(), b"0\n");
        assert_eq!(format_page_count(3).unwrap(), b"12288\n");
        assert_eq!(format_page_count(u64::MAX), Err(SystemError::EOVERFLOW));
        let maximum = super::super::accounting::PAGE_COUNTER_MAX;
        assert_eq!(
            format_page_count(maximum).unwrap(),
            alloc::format!("{}\n", maximum * MMArch::PAGE_SIZE as u64).into_bytes()
        );
    }
}
