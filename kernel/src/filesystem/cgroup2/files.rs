use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use hashbrown::{HashMap, HashSet};
use system_error::SystemError;

use crate::cgroup::{
    cpuset::{self, CpusetFile},
    CgroupNode,
};

use super::{AVAILABLE_CONTROLLERS, DOMAIN_CONTROLLERS};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CgroupCoreFile {
    Procs,
    Threads,
    Controllers,
    SubtreeControl,
    Events,
    Type,
    Freeze,
    CpuStat,
    CpuStatLocal,
    CpuWeight,
    CpuWeightNice,
    CpuIdle,
    CpuMax,
    CpuMaxBurst,
    Cpuset(CpusetFile),
    MemoryCurrent,
    MemoryPeak,
    MemoryMin,
    MemoryLow,
    MemoryHigh,
    MemoryMax,
    MemoryOomGroup,
    MemoryReclaim,
    MemoryEvents,
    MemoryEventsLocal,
    MemoryStat,
    MemorySwapCurrent,
    MemorySwapPeak,
    MemorySwapHigh,
    MemorySwapMax,
    MemorySwapEvents,
    PidsCurrent,
    PidsMax,
    PidsEvents,
}

#[derive(Clone, Copy)]
pub(super) struct CgroupFileSpec {
    pub(super) name: &'static str,
    pub(super) ty: CgroupCoreFile,
    pub(super) init: &'static [u8],
    pub(super) mode: u16,
    visibility: CgroupFileVisibility,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CgroupFileVisibility {
    All,
    NotOnRoot,
}

impl CgroupFileSpec {
    fn visible_on(self, cgroup: &Arc<CgroupNode>) -> bool {
        match self.visibility {
            CgroupFileVisibility::All => true,
            CgroupFileVisibility::NotOnRoot => cgroup.parent().is_some(),
        }
    }
}

const BASE_FILE_SPECS: [CgroupFileSpec; 6] = [
    CgroupFileSpec {
        name: "cgroup.procs",
        ty: CgroupCoreFile::Procs,
        init: b"",
        mode: 0o644,
        visibility: CgroupFileVisibility::All,
    },
    CgroupFileSpec {
        name: "cgroup.threads",
        ty: CgroupCoreFile::Threads,
        init: b"",
        mode: 0o644,
        visibility: CgroupFileVisibility::All,
    },
    CgroupFileSpec {
        name: "cgroup.controllers",
        ty: CgroupCoreFile::Controllers,
        init: b"\n",
        mode: 0o444,
        visibility: CgroupFileVisibility::All,
    },
    CgroupFileSpec {
        name: "cgroup.subtree_control",
        ty: CgroupCoreFile::SubtreeControl,
        init: b"\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::All,
    },
    CgroupFileSpec {
        name: "cpu.stat",
        ty: CgroupCoreFile::CpuStat,
        init: b"",
        mode: 0o444,
        visibility: CgroupFileVisibility::All,
    },
    CgroupFileSpec {
        name: "cpu.stat.local",
        ty: CgroupCoreFile::CpuStatLocal,
        init: b"",
        mode: 0o444,
        visibility: CgroupFileVisibility::All,
    },
];

const NON_ROOT_CORE_FILE_SPECS: [CgroupFileSpec; 3] = [
    CgroupFileSpec {
        name: "cgroup.events",
        ty: CgroupCoreFile::Events,
        init: b"",
        mode: 0o444,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "cgroup.type",
        ty: CgroupCoreFile::Type,
        init: b"domain\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "cgroup.freeze",
        ty: CgroupCoreFile::Freeze,
        init: b"0\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
];

const CPU_FILE_SPECS: [CgroupFileSpec; 5] = [
    CgroupFileSpec {
        name: "cpu.weight",
        ty: CgroupCoreFile::CpuWeight,
        init: b"100\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "cpu.max",
        ty: CgroupCoreFile::CpuMax,
        init: b"max 100000\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "cpu.weight.nice",
        ty: CgroupCoreFile::CpuWeightNice,
        init: b"0\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "cpu.idle",
        ty: CgroupCoreFile::CpuIdle,
        init: b"0\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "cpu.max.burst",
        ty: CgroupCoreFile::CpuMaxBurst,
        init: b"0\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
];

const CPUSET_FILE_SPECS: [CgroupFileSpec; 4] = [
    CgroupFileSpec {
        name: "cpuset.cpus",
        ty: CgroupCoreFile::Cpuset(CpusetFile::Cpus),
        init: b"\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "cpuset.mems",
        ty: CgroupCoreFile::Cpuset(CpusetFile::Mems),
        init: b"\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "cpuset.cpus.effective",
        ty: CgroupCoreFile::Cpuset(CpusetFile::EffectiveCpus),
        init: b"\n",
        mode: 0o444,
        visibility: CgroupFileVisibility::All,
    },
    CgroupFileSpec {
        name: "cpuset.mems.effective",
        ty: CgroupCoreFile::Cpuset(CpusetFile::EffectiveMems),
        init: b"\n",
        mode: 0o444,
        visibility: CgroupFileVisibility::All,
    },
];

const MEMORY_FILE_SPECS: [CgroupFileSpec; 16] = [
    CgroupFileSpec {
        name: "memory.current",
        ty: CgroupCoreFile::MemoryCurrent,
        init: b"0\n",
        mode: 0o444,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.peak",
        ty: CgroupCoreFile::MemoryPeak,
        init: b"0\n",
        mode: 0o444,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.min",
        ty: CgroupCoreFile::MemoryMin,
        init: b"0\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.low",
        ty: CgroupCoreFile::MemoryLow,
        init: b"0\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.high",
        ty: CgroupCoreFile::MemoryHigh,
        init: b"max\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.max",
        ty: CgroupCoreFile::MemoryMax,
        init: b"max\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.events",
        ty: CgroupCoreFile::MemoryEvents,
        init: b"",
        mode: 0o444,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.oom.group",
        ty: CgroupCoreFile::MemoryOomGroup,
        init: b"0\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.reclaim",
        ty: CgroupCoreFile::MemoryReclaim,
        init: b"",
        mode: 0o200,
        visibility: CgroupFileVisibility::All,
    },
    CgroupFileSpec {
        name: "memory.events.local",
        ty: CgroupCoreFile::MemoryEventsLocal,
        init: b"",
        mode: 0o444,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.stat",
        ty: CgroupCoreFile::MemoryStat,
        init: b"",
        mode: 0o444,
        visibility: CgroupFileVisibility::All,
    },
    CgroupFileSpec {
        name: "memory.swap.current",
        ty: CgroupCoreFile::MemorySwapCurrent,
        init: b"0\n",
        mode: 0o444,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.swap.peak",
        ty: CgroupCoreFile::MemorySwapPeak,
        init: b"0\n",
        mode: 0o444,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.swap.high",
        ty: CgroupCoreFile::MemorySwapHigh,
        init: b"max\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.swap.max",
        ty: CgroupCoreFile::MemorySwapMax,
        init: b"max\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.swap.events",
        ty: CgroupCoreFile::MemorySwapEvents,
        init: b"",
        mode: 0o444,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
];

const PIDS_FILE_SPECS: [CgroupFileSpec; 3] = [
    CgroupFileSpec {
        name: "pids.current",
        ty: CgroupCoreFile::PidsCurrent,
        init: b"0\n",
        mode: 0o444,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "pids.max",
        ty: CgroupCoreFile::PidsMax,
        init: b"max\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "pids.events",
        ty: CgroupCoreFile::PidsEvents,
        init: b"max 0\n",
        mode: 0o444,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
];

pub(super) fn desired_file_specs(cgroup: &Arc<CgroupNode>) -> Vec<CgroupFileSpec> {
    let mut specs = Vec::new();
    push_visible_specs(&mut specs, cgroup, &BASE_FILE_SPECS);
    push_visible_specs(&mut specs, cgroup, &NON_ROOT_CORE_FILE_SPECS);
    for controller in available_controllers_for(cgroup) {
        push_visible_specs(&mut specs, cgroup, controller_specs(controller));
    }
    specs
}

pub(super) fn desired_file_names(cgroup: &Arc<CgroupNode>) -> HashSet<&'static str> {
    desired_file_specs(cgroup)
        .into_iter()
        .map(|spec| spec.name)
        .collect()
}

fn push_visible_specs(
    out: &mut Vec<CgroupFileSpec>,
    cgroup: &Arc<CgroupNode>,
    specs: &'static [CgroupFileSpec],
) {
    out.extend(specs.iter().copied().filter(|spec| spec.visible_on(cgroup)));
}

fn controller_specs(name: &str) -> &'static [CgroupFileSpec] {
    match name {
        "cpu" => &CPU_FILE_SPECS,
        "cpuset" => &CPUSET_FILE_SPECS,
        "memory" => &MEMORY_FILE_SPECS,
        "pids" => &PIDS_FILE_SPECS,
        _ => &[],
    }
}

fn available_controllers_for(cgroup: &Arc<CgroupNode>) -> Vec<&'static str> {
    let Some(parent) = cgroup.parent() else {
        return AVAILABLE_CONTROLLERS.to_vec();
    };
    let parent_enabled: HashSet<String> = parent.subtree_control().into_iter().collect();
    AVAILABLE_CONTROLLERS
        .iter()
        .copied()
        .filter(|name| {
            parent_enabled.contains(*name)
                && (!cgroup.is_threaded() || crate::cgroup::threaded::is_threaded_controller(name))
        })
        .collect()
}

fn is_known_controller(name: &str) -> bool {
    AVAILABLE_CONTROLLERS.contains(&name)
}

pub(super) fn read_file(
    cgroup: &Arc<CgroupNode>,
    ty: CgroupCoreFile,
    generation: u64,
    file_generation: u64,
) -> Result<Vec<u8>, SystemError> {
    if let CgroupCoreFile::Cpuset(file) = ty {
        return cpuset::read(cgroup, generation, file);
    }
    if matches!(
        ty,
        CgroupCoreFile::MemoryCurrent
            | CgroupCoreFile::MemoryPeak
            | CgroupCoreFile::MemoryEvents
            | CgroupCoreFile::MemoryEventsLocal
            | CgroupCoreFile::MemoryStat
            | CgroupCoreFile::MemoryMin
            | CgroupCoreFile::MemoryLow
            | CgroupCoreFile::MemoryHigh
            | CgroupCoreFile::MemoryMax
            | CgroupCoreFile::MemoryOomGroup
    ) {
        // Pin and sample the exact generation admitted by the file epoch.
        // Formatting can allocate, so it must run outside UPDATE. A concurrent
        // disable after this linearization point does not redirect the read.
        let (snapshot, policy) = {
            let _guard = crate::cgroup::lock();
            check_live_file(cgroup, ty, file_generation)?;
            let account = cgroup
                .local_memory_generation()
                .ok_or(SystemError::ENODEV)?
                .account;
            (account.snapshot(), account.policy())
        };
        return match ty {
            CgroupCoreFile::MemoryCurrent => {
                crate::cgroup::memory::stats::format_page_count(snapshot.current)
            }
            CgroupCoreFile::MemoryPeak => {
                crate::cgroup::memory::stats::format_page_count(snapshot.peak)
            }
            CgroupCoreFile::MemoryEvents => {
                crate::cgroup::memory::stats::format_events(&snapshot, false)
            }
            CgroupCoreFile::MemoryEventsLocal => {
                crate::cgroup::memory::stats::format_events(&snapshot, true)
            }
            CgroupCoreFile::MemoryStat => crate::cgroup::memory::stats::format_stats(&snapshot),
            CgroupCoreFile::MemoryMin => crate::cgroup::memory::stats::format_limit(
                (policy.min != crate::cgroup::memory::abi::PAGE_COUNTER_MAX).then_some(policy.min),
            ),
            CgroupCoreFile::MemoryLow => crate::cgroup::memory::stats::format_limit(
                (policy.low != crate::cgroup::memory::abi::PAGE_COUNTER_MAX).then_some(policy.low),
            ),
            CgroupCoreFile::MemoryHigh => crate::cgroup::memory::stats::format_limit(policy.high),
            CgroupCoreFile::MemoryMax => crate::cgroup::memory::stats::format_limit(policy.max),
            CgroupCoreFile::MemoryOomGroup => {
                crate::cgroup::memory::stats::format_oom_group(policy.oom_group)
            }
            _ => unreachable!(),
        };
    }
    let _guard = crate::cgroup::lock();
    check_live_file(cgroup, ty, file_generation)?;
    Ok(match ty {
        CgroupCoreFile::Cpuset(_) => unreachable!(),
        CgroupCoreFile::Procs => membership_list(cgroup, true)?,
        CgroupCoreFile::Threads => membership_list(cgroup, false)?,
        CgroupCoreFile::Controllers => {
            let items: Vec<String> = available_controllers_for(cgroup)
                .into_iter()
                .map(|s| s.to_string())
                .collect();
            encode_controller_list(&items)
        }
        CgroupCoreFile::SubtreeControl => {
            let items = cgroup.subtree_control();
            encode_controller_list(&items)
        }
        CgroupCoreFile::Events => {
            let populated = if is_populated(cgroup) { 1 } else { 0 };
            format!(
                "populated {}\nfrozen {}\n",
                populated,
                u8::from(cgroup.frozen())
            )
            .into_bytes()
        }
        CgroupCoreFile::Type => format!("{}\n", cgroup.type_name()).into_bytes(),
        CgroupCoreFile::Freeze => {
            format!("{}\n", if cgroup.freeze_requested() { 1 } else { 0 }).into_bytes()
        }
        CgroupCoreFile::CpuStat => cpu_stat(cgroup),
        CgroupCoreFile::CpuStatLocal => cpu_stat_local(cgroup),
        CgroupCoreFile::CpuWeight => format!("{}\n", cgroup.cpu_state().weight()).into_bytes(),
        CgroupCoreFile::CpuWeightNice => format!("{}\n", cgroup.cpu_state().nice()).into_bytes(),
        CgroupCoreFile::CpuIdle => {
            format!("{}\n", u8::from(cgroup.cpu_state().idle())).into_bytes()
        }
        CgroupCoreFile::CpuMaxBurst => format!("{}\n", cgroup.cpu_state().burst()).into_bytes(),
        CgroupCoreFile::CpuMax => {
            let (quota, period) = cgroup.cpu_state().max();
            encode_cpu_max(quota, period)
        }
        CgroupCoreFile::MemoryCurrent
        | CgroupCoreFile::MemoryPeak
        | CgroupCoreFile::MemoryEvents
        | CgroupCoreFile::MemoryEventsLocal
        | CgroupCoreFile::MemoryStat
        | CgroupCoreFile::MemoryMin
        | CgroupCoreFile::MemoryLow
        | CgroupCoreFile::MemoryHigh
        | CgroupCoreFile::MemoryMax
        | CgroupCoreFile::MemoryOomGroup => unreachable!(),
        CgroupCoreFile::MemoryReclaim => return Err(SystemError::EBADF),
        CgroupCoreFile::MemorySwapCurrent | CgroupCoreFile::MemorySwapPeak => b"0\n".to_vec(),
        CgroupCoreFile::MemorySwapHigh => encode_max_u64(cgroup.memory_state().swap_high()),
        CgroupCoreFile::MemorySwapMax => encode_max_u64(cgroup.memory_state().swap_max()),
        CgroupCoreFile::MemorySwapEvents => memory_swap_events(),
        CgroupCoreFile::PidsCurrent => format!("{}\n", cgroup.pids_current_count()).into_bytes(),
        CgroupCoreFile::PidsMax => encode_pids_max(cgroup.pids_max()),
        CgroupCoreFile::PidsEvents => format!("max {}\n", cgroup.pids_events_max()).into_bytes(),
    })
}

/// Caller validates file liveness and holds the cgroup update lock through
/// delegation admission and mutation.
pub(super) fn write_controller_file_locked(
    cgroup: &Arc<CgroupNode>,
    ty: CgroupCoreFile,
    generation: u64,
    input: &[u8],
) -> Result<Vec<u8>, SystemError> {
    let input = core::str::from_utf8(input).map_err(|_| SystemError::EINVAL)?;
    if let CgroupCoreFile::Cpuset(file) = ty {
        cpuset::write_locked(cgroup, generation, file, input)?;
        return Ok(Vec::new());
    }
    if ty == CgroupCoreFile::Type {
        if input.trim() != "threaded" {
            return Err(SystemError::EINVAL);
        }
        let had_memory = !cgroup.is_threaded() && cgroup.local_memory_generation().is_some();
        let mut permissions = Vec::new();
        let mut retired_permissions = Vec::new();
        if had_memory {
            permissions
                .try_reserve_exact(MEMORY_FILE_SPECS.len())
                .map_err(|_| SystemError::ENOMEM)?;
            retired_permissions
                .try_reserve_exact(MEMORY_FILE_SPECS.len())
                .map_err(|_| SystemError::ENOMEM)?;
            for spec in MEMORY_FILE_SPECS {
                permissions
                    .push(cgroup.prepare_file_permission_reset(spec.name, spec.mode as u32)?);
            }
        }
        let retired = {
            let _accounting = crate::cgroup::cgroup_accounting_lock().lock();
            crate::cgroup::threaded::enable_threaded(cgroup)?;
            for permission in permissions.drain(..) {
                retired_permissions.push(cgroup.commit_file_permission_reset(permission));
            }
            cgroup.replace_memory_generation(None)
        };
        if let Some(generation) = retired {
            generation.retire();
        }
        return Ok(Vec::new());
    }
    match ty {
        CgroupCoreFile::Cpuset(_) | CgroupCoreFile::Type => unreachable!(),
        CgroupCoreFile::Freeze => {
            let value = input.trim().parse::<i32>().map_err(|e| match e.kind() {
                core::num::IntErrorKind::PosOverflow | core::num::IntErrorKind::NegOverflow => {
                    SystemError::ERANGE
                }
                _ => SystemError::EINVAL,
            })?;
            if !(0..=1).contains(&value) {
                return Err(SystemError::ERANGE);
            }
            crate::cgroup::freezer::set_requested(cgroup, value == 1)?;
            Ok(format!("{}\n", value).into_bytes())
        }
        CgroupCoreFile::CpuWeight => {
            let weight = parse_cpu_unsigned(input)?;
            crate::cgroup::cpu::set_weight_locked(cgroup, weight)?;
            Ok(format!("{}\n", cgroup.cpu_state().weight()).into_bytes())
        }
        CgroupCoreFile::CpuWeightNice => {
            let nice = parse_cpu_signed(input)?;
            crate::cgroup::cpu::set_nice_locked(cgroup, nice)?;
            Ok(format!("{}\n", cgroup.cpu_state().nice()).into_bytes())
        }
        CgroupCoreFile::CpuIdle => {
            let idle = parse_cpu_signed(input)?;
            crate::cgroup::cpu::set_idle_locked(cgroup, idle)?;
            Ok(format!("{}\n", idle).into_bytes())
        }
        CgroupCoreFile::CpuMaxBurst => {
            let burst = parse_cpu_unsigned(input)?;
            let mut state = cgroup.cpu_state();
            state.set_burst(burst);
            crate::cgroup::cpu::set_bandwidth_locked(cgroup, state)?;
            Ok(format!("{}\n", burst).into_bytes())
        }
        CgroupCoreFile::CpuMax => {
            let (_, current_period) = cgroup.cpu_state().max();
            let (quota, period) = parse_cpu_max(input, current_period)?;
            let mut state = cgroup.cpu_state();
            state.set_max(quota, period);
            crate::cgroup::cpu::set_bandwidth_locked(cgroup, state)?;
            Ok(encode_cpu_max(quota, period))
        }
        CgroupCoreFile::MemorySwapHigh | CgroupCoreFile::MemorySwapMax => {
            let value = parse_max_u64(input)?;
            match ty {
                CgroupCoreFile::MemorySwapHigh => cgroup.set_memory_swap_high(value),
                CgroupCoreFile::MemorySwapMax => cgroup.set_memory_swap_max(value),
                _ => unreachable!(),
            }
            Ok(encode_max_u64(value))
        }
        CgroupCoreFile::PidsMax => {
            let new_limit = parse_pids_max(input)?;
            cgroup.set_pids_max(new_limit);
            Ok(encode_pids_max(new_limit))
        }
        CgroupCoreFile::Controllers
        | CgroupCoreFile::Events
        | CgroupCoreFile::CpuStat
        | CgroupCoreFile::CpuStatLocal
        | CgroupCoreFile::MemoryCurrent
        | CgroupCoreFile::MemoryPeak
        | CgroupCoreFile::MemoryEvents
        | CgroupCoreFile::MemoryEventsLocal
        | CgroupCoreFile::MemoryStat
        | CgroupCoreFile::MemorySwapCurrent
        | CgroupCoreFile::MemorySwapPeak
        | CgroupCoreFile::MemorySwapEvents
        | CgroupCoreFile::PidsCurrent
        | CgroupCoreFile::PidsEvents => Err(SystemError::EPERM),
        CgroupCoreFile::MemoryMin
        | CgroupCoreFile::MemoryLow
        | CgroupCoreFile::MemoryHigh
        | CgroupCoreFile::MemoryMax
        | CgroupCoreFile::MemoryOomGroup
        | CgroupCoreFile::MemoryReclaim
        | CgroupCoreFile::Procs
        | CgroupCoreFile::Threads
        | CgroupCoreFile::SubtreeControl => Err(SystemError::EINVAL),
    }
}

/// Validate the original file epoch without rendering or allocating a record.
pub(super) fn check_read_source(
    cgroup: &Arc<CgroupNode>,
    ty: CgroupCoreFile,
    cpuset_generation: u64,
    file_generation: u64,
) -> Result<(), SystemError> {
    let _update = crate::cgroup::lock();
    check_live_file(cgroup, ty, file_generation)?;
    if matches!(ty, CgroupCoreFile::Cpuset(_)) {
        cpuset::check_file(cgroup, cpuset_generation)?;
    }
    Ok(())
}

pub(super) fn check_live_file(
    cgroup: &Arc<CgroupNode>,
    ty: CgroupCoreFile,
    generation: u64,
) -> Result<(), SystemError> {
    if !crate::cgroup::cgroup_root().is_online(cgroup) {
        return Err(SystemError::ENODEV);
    }
    let spec = live_file_spec(cgroup, ty).ok_or(SystemError::ENODEV)?;
    if cgroup.file_generation(spec.name) != generation {
        return Err(SystemError::ENODEV);
    }
    Ok(())
}

/// Validate one static file without allocating a complete directory snapshot
/// under UPDATE. Keep availability identical to desired_file_specs().
fn live_file_spec(cgroup: &Arc<CgroupNode>, ty: CgroupCoreFile) -> Option<CgroupFileSpec> {
    if let Some(spec) = BASE_FILE_SPECS
        .iter()
        .chain(NON_ROOT_CORE_FILE_SPECS.iter())
        .find(|spec| spec.ty == ty)
    {
        return spec.visible_on(cgroup).then_some(*spec);
    }
    for controller in AVAILABLE_CONTROLLERS {
        let Some(spec) = controller_specs(controller)
            .iter()
            .find(|spec| spec.ty == ty)
        else {
            continue;
        };
        let available = match cgroup.parent() {
            None => true,
            Some(parent) => {
                parent.subtree_controller_enabled(controller)
                    && (!cgroup.is_threaded()
                        || crate::cgroup::threaded::is_threaded_controller(controller))
            }
        };
        return (available && spec.visible_on(cgroup)).then_some(*spec);
    }
    None
}

/// Membership and PID identities share the accounting transaction with exec.
/// Aggregate threaded descendants in this resource domain. Invalid ordinary
/// nodes can separate them after an ancestor is converted to threaded mode.
fn membership_list(cgroup: &Arc<CgroupNode>, processes: bool) -> Result<Vec<u8>, SystemError> {
    if processes && cgroup.is_threaded() {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    let _accounting = crate::cgroup::cgroup_accounting_lock().lock();
    let mut nodes = alloc::vec![cgroup.clone()];
    let mut ids = alloc::collections::BTreeMap::new();
    while let Some(node) = nodes.pop() {
        let in_domain = !processes
            || Arc::ptr_eq(&node, cgroup)
            || (node.is_threaded() && Arc::ptr_eq(&node.resource_domain(), cgroup));
        for pid in node.tasks().into_iter().filter(|_| in_domain) {
            let Some(task) = crate::process::ProcessManager::find(pid) else {
                continue;
            };
            if task.is_exited() {
                continue;
            }
            if processes {
                if let Some(pid) = task.task_pid_ptr(crate::process::pid::PidType::TGID) {
                    if let Some(raw) = pid.first_upid() {
                        ids.insert(raw.nr.data(), pid.pid_vnr().data());
                    }
                }
            } else {
                ids.insert(task.raw_pid().data(), task.task_pid_vnr().data());
            }
        }
        if processes {
            nodes.extend(
                node.children()
                    .into_iter()
                    .filter(|child| !child.is_valid_domain()),
            );
        }
    }
    let mut lines = String::new();
    for id in ids.into_values() {
        lines.push_str(&format!("{}\n", id));
    }
    Ok(lines.into_bytes())
}

fn validated_subtree_control(
    cgroup: &Arc<CgroupNode>,
    input: &str,
) -> Result<(HashSet<String>, HashSet<String>), SystemError> {
    let ops = fold_subtree_control_ops(input)?;
    let mut enabled = cgroup.subtree_control_snapshot()?;
    let old_enabled = clone_controller_set(&enabled)?;
    enabled
        .try_reserve(ops.len())
        .map_err(|_| SystemError::ENOMEM)?;

    for (name, is_enable) in ops {
        if is_enable {
            if enabled.contains(&name) {
                continue;
            }
            validate_enable_controller(cgroup, &name)?;
            enabled.insert(name);
        } else {
            if !enabled.contains(&name) {
                continue;
            }
            for child in cgroup.children_snapshot()? {
                if child.subtree_controller_enabled(&name) {
                    return Err(SystemError::EBUSY);
                }
            }
            enabled.remove(&name);
        }
    }

    if enabled.difference(&old_enabled).next().is_some() {
        if !cgroup.resource_domain().is_valid_domain() {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if cgroup.parent().is_some() {
            let domain_enable = enabled
                .difference(&old_enabled)
                .any(|name| DOMAIN_CONTROLLERS.contains(&name.as_str()));
            if domain_enable && (cgroup.is_thread_root() || cgroup.is_threaded()) {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
            if (domain_enable || (!cgroup.can_be_thread_root() && !cgroup.is_threaded()))
                && cgroup.has_tasks()
            {
                return Err(SystemError::EBUSY);
            }
        }
    }
    Ok((old_enabled, enabled))
}

pub(super) fn apply_subtree_control(
    cgroup: &Arc<CgroupNode>,
    input: &str,
) -> Result<Vec<u8>, SystemError> {
    let (old_enabled, enabled) = validated_subtree_control(cgroup, input)?;
    // Controller instances have independent lifetimes. Reset their state and
    // inode identity at the topology transaction, not at a mount's lazy lookup.
    for name in old_enabled.symmetric_difference(&enabled) {
        for child in cgroup.children() {
            child.reset_controller(name);
            for spec in controller_specs(name) {
                child.reset_file_permissions(spec.name, spec.mode as u32);
            }
        }
    }
    cgroup.set_subtree_control(enabled.clone());
    let mut out: Vec<String> = enabled.into_iter().collect();
    out.sort();
    Ok(encode_controller_list(&out))
}

struct PreparedMemoryChild {
    node: Arc<CgroupNode>,
    generation: Option<crate::cgroup::core::MemoryGeneration>,
    object: Option<crate::cgroup::memory::object::PreparedObjectAccount>,
    permissions: Vec<crate::cgroup::core::PreparedFilePermission>,
}

pub(super) struct PreparedSubtreeControl {
    enabled: HashSet<String>,
    changed: Vec<String>,
    children: Vec<PreparedMemoryChild>,
    retired: Vec<crate::cgroup::core::MemoryGeneration>,
    retired_permissions: Vec<crate::cgroup::core::PreparedFilePermission>,
    retired_enabled: Option<HashSet<String>>,
    data: Vec<u8>,
    memory_changed: bool,
    cpu: Option<crate::cgroup::cpu::PreparedCpuControl>,
}

/// All resource/metadata preparation occurs outside the accounting spinlock.
pub(super) fn prepare_subtree_control(
    node: &Arc<CgroupNode>,
    input: &str,
) -> Result<Option<PreparedSubtreeControl>, SystemError> {
    let (old, enabled) = validated_subtree_control(node, input)?;
    let memory_changed = old.contains("memory") != enabled.contains("memory");
    let cpu_changed = old.contains("cpu") != enabled.contains("cpu");
    if !memory_changed && !cpu_changed {
        return Ok(None);
    }
    let mut changed = Vec::new();
    changed
        .try_reserve_exact(old.len() + enabled.len())
        .map_err(|_| SystemError::ENOMEM)?;
    for name in old.symmetric_difference(&enabled) {
        changed.push(copy_controller_name(name)?);
    }
    let nodes = node.children_snapshot()?;
    let permission_count: usize = changed
        .iter()
        .map(|name| controller_specs(name).len())
        .sum();
    let mut children = Vec::new();
    children
        .try_reserve_exact(nodes.len())
        .map_err(|_| SystemError::ENOMEM)?;
    let mut retired = Vec::new();
    retired
        .try_reserve_exact(nodes.len())
        .map_err(|_| SystemError::ENOMEM)?;
    let mut retired_permissions = Vec::new();
    let total_permissions = nodes
        .len()
        .checked_mul(permission_count)
        .ok_or(SystemError::EOVERFLOW)?;
    retired_permissions
        .try_reserve_exact(total_permissions)
        .map_err(|_| SystemError::ENOMEM)?;
    let parent = node.memory_account();
    for child in nodes {
        let (generation, object) = if memory_changed
            && enabled.contains("memory")
            && !child.is_threaded()
        {
            let (generation, object) =
                crate::cgroup::core::MemoryGeneration::prepare(child.id(), Some(parent.clone()))?;
            (Some(generation), Some(object))
        } else {
            (None, None)
        };
        let mut permissions = Vec::new();
        permissions
            .try_reserve_exact(permission_count)
            .map_err(|_| SystemError::ENOMEM)?;
        for name in &changed {
            for spec in controller_specs(name) {
                permissions.push(child.prepare_file_permission_reset(spec.name, spec.mode as u32)?);
            }
        }
        children.push(PreparedMemoryChild {
            node: child,
            generation,
            object,
            permissions,
        });
    }
    let mut names = Vec::new();
    names
        .try_reserve_exact(enabled.len())
        .map_err(|_| SystemError::ENOMEM)?;
    for name in &enabled {
        names.push(name.as_str());
    }
    names.sort();
    let length = names
        .iter()
        .try_fold(1usize, |size, name| size.checked_add(name.len() + 1))
        .ok_or(SystemError::EOVERFLOW)?;
    let mut data = Vec::new();
    data.try_reserve_exact(length)
        .map_err(|_| SystemError::ENOMEM)?;
    for (index, name) in names.into_iter().enumerate() {
        if index != 0 {
            data.push(b' ');
        }
        data.extend_from_slice(name.as_bytes());
    }
    data.push(b'\n');
    let cpu = if cpu_changed {
        Some(crate::cgroup::cpu::prepare_control_locked(
            node,
            enabled.contains("cpu"),
        )?)
    } else {
        None
    };
    Ok(Some(PreparedSubtreeControl {
        enabled,
        changed,
        children,
        retired,
        retired_permissions,
        retired_enabled: None,
        data,
        memory_changed,
        cpu,
    }))
}

impl PreparedSubtreeControl {
    /// Caller holds UPDATE_LOCK and accounting lock; only prepared moves.
    pub(super) fn commit(&mut self, node: &Arc<CgroupNode>) {
        for child in &mut self.children {
            for name in &self.changed {
                child.node.reset_controller(name);
            }
            for permission in child.permissions.drain(..) {
                self.retired_permissions
                    .push(child.node.commit_file_permission_reset(permission));
            }
            if self.memory_changed {
                if let Some(old) = child
                    .node
                    .replace_memory_generation(child.generation.take())
                {
                    self.retired.push(old);
                }
            }
        }
        self.retired_enabled =
            Some(node.replace_subtree_control(core::mem::take(&mut self.enabled)));
    }

    /// Accounting/structure/inode locks are released; UPDATE_LOCK still held.
    pub(super) fn finish(
        self,
        node: &Arc<CgroupNode>,
        tasks: &[Arc<crate::process::ProcessControlBlock>],
    ) -> Vec<u8> {
        for generation in &self.retired {
            generation.retire();
        }
        for child in self.children {
            if let Some(object) = child.object {
                object.publish();
            }
        }
        if let Some(cpu) = self.cpu {
            crate::cgroup::cpu::controller_changed_locked(node, tasks, cpu);
        }
        self.data
    }
}

fn validate_enable_controller(cgroup: &Arc<CgroupNode>, name: &str) -> Result<(), SystemError> {
    if !is_known_controller(name)
        || cgroup.parent().is_some_and(|parent| {
            !parent.subtree_controller_enabled(name)
                || (cgroup.is_threaded() && !crate::cgroup::threaded::is_threaded_controller(name))
        })
    {
        return Err(SystemError::ENOENT);
    }

    Ok(())
}

fn parse_subtree_control_ops(input: &str) -> Result<Vec<(bool, &str)>, SystemError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let mut ops = Vec::new();
    ops.try_reserve_exact(trimmed.split_whitespace().count())
        .map_err(|_| SystemError::ENOMEM)?;
    for token in trimmed.split_whitespace() {
        let mut chars = token.chars();
        let op = chars.next().ok_or(SystemError::EINVAL)?;
        let enable = match op {
            '+' => true,
            '-' => false,
            _ => return Err(SystemError::EINVAL),
        };
        let name = chars.as_str();
        if name.is_empty() || name.contains('/') {
            return Err(SystemError::EINVAL);
        }
        ops.push((enable, name));
    }
    Ok(ops)
}

fn fold_subtree_control_ops(input: &str) -> Result<HashMap<String, bool>, SystemError> {
    let mut folded = HashMap::new();
    let ops = parse_subtree_control_ops(input)?;
    folded
        .try_reserve(ops.len())
        .map_err(|_| SystemError::ENOMEM)?;
    for (enable, name) in ops {
        if !is_known_controller(name) {
            return Err(SystemError::EINVAL);
        }
        folded.insert(copy_controller_name(name)?, enable);
    }
    Ok(folded)
}

fn copy_controller_name(name: &str) -> Result<String, SystemError> {
    let mut copy = String::new();
    copy.try_reserve_exact(name.len())
        .map_err(|_| SystemError::ENOMEM)?;
    copy.push_str(name);
    Ok(copy)
}

fn clone_controller_set(source: &HashSet<String>) -> Result<HashSet<String>, SystemError> {
    let mut copy = HashSet::new();
    copy.try_reserve(source.len())
        .map_err(|_| SystemError::ENOMEM)?;
    for name in source {
        copy.insert(copy_controller_name(name)?);
    }
    Ok(copy)
}

fn encode_controller_list(items: &[String]) -> Vec<u8> {
    if items.is_empty() {
        return b"\n".to_vec();
    }
    let mut sorted = items.to_vec();
    sorted.sort();
    let mut line = sorted.join(" ");
    line.push('\n');
    line.into_bytes()
}

fn encode_pids_max(limit: Option<usize>) -> Vec<u8> {
    match limit {
        Some(v) => format!("{}\n", v).into_bytes(),
        None => b"max\n".to_vec(),
    }
}

fn parse_pids_max(input: &str) -> Result<Option<usize>, SystemError> {
    let trimmed = input.trim();
    if trimmed == "max" {
        return Ok(None);
    }
    let value = trimmed.parse::<u64>().map_err(|_| SystemError::EINVAL)?;
    let value = usize::try_from(value).map_err(|_| SystemError::EINVAL)?;
    Ok(Some(value))
}

fn encode_max_u64(value: Option<u64>) -> Vec<u8> {
    match value {
        Some(v) => format!("{}\n", v).into_bytes(),
        None => b"max\n".to_vec(),
    }
}

fn parse_max_u64(input: &str) -> Result<Option<u64>, SystemError> {
    let trimmed = input.trim();
    if trimmed == "max" {
        return Ok(None);
    }
    let value = trimmed.parse::<u64>().map_err(|_| SystemError::EINVAL)?;
    Ok(Some(value))
}

fn encode_cpu_max(quota: Option<u64>, period_us: u64) -> Vec<u8> {
    match quota {
        Some(quota) => format!("{} {}\n", quota, period_us).into_bytes(),
        None => format!("max {}\n", period_us).into_bytes(),
    }
}

/// Scalar cftypes use Linux kstrtoull/kstrtoll(base=0), unlike cpu.max's
/// decimal two-field parser. Keep overflow distinct from malformed input.
fn parse_cpu_unsigned(input: &str) -> Result<u64, SystemError> {
    let input = input.trim().strip_prefix('+').unwrap_or(input.trim());
    let (digits, radix) = if let Some(hex) = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("0X"))
    {
        (hex, 16)
    } else if input.len() > 1 && input.starts_with('0') {
        (&input[1..], 8)
    } else {
        (input, 10)
    };
    if digits.starts_with('+') || digits.starts_with('-') {
        return Err(SystemError::EINVAL);
    }
    u64::from_str_radix(digits, radix).map_err(|error| match error.kind() {
        core::num::IntErrorKind::PosOverflow | core::num::IntErrorKind::NegOverflow => {
            SystemError::ERANGE
        }
        _ => SystemError::EINVAL,
    })
}

fn parse_cpu_signed(input: &str) -> Result<i64, SystemError> {
    let input = input.trim();
    let negative = input.starts_with('-');
    let digits = input.strip_prefix('-').unwrap_or(input);
    if negative && (digits.starts_with('+') || digits.starts_with('-')) {
        return Err(SystemError::EINVAL);
    }
    let magnitude = parse_cpu_unsigned(digits)?;
    if negative {
        if magnitude > i64::MAX as u64 + 1 {
            return Err(SystemError::ERANGE);
        }
        Ok((0u64.wrapping_sub(magnitude)) as i64)
    } else {
        i64::try_from(magnitude).map_err(|_| SystemError::ERANGE)
    }
}

fn parse_cpu_max(input: &str, current_period_us: u64) -> Result<(Option<u64>, u64), SystemError> {
    let mut parts = input.split_whitespace();
    let quota_raw = parts.next().ok_or(SystemError::EINVAL)?;
    let quota = if quota_raw == "max" {
        None
    } else {
        Some(quota_raw.parse::<u64>().map_err(|_| SystemError::EINVAL)?)
    };
    let period = match parts.next() {
        Some(raw) => raw.parse::<u64>().map_err(|_| SystemError::EINVAL)?,
        None => current_period_us,
    };
    if parts.next().is_some() || period == 0 {
        return Err(SystemError::EINVAL);
    }
    Ok((quota, period))
}

fn cpu_stat(cgroup: &Arc<CgroupNode>) -> Vec<u8> {
    let time = cgroup.cpu_accounting().snapshot();
    let mut output = format!(
        "usage_usec {}\nuser_usec {}\nsystem_usec {}\n",
        time.usage_ns / 1_000,
        time.user_ns / 1_000,
        time.system_ns / 1_000,
    );
    if let Some(group) = cgroup.cpu_group() {
        let stat = group.bandwidth_snapshot();
        output.push_str(&format!(
            "nr_periods {}\nnr_throttled {}\nthrottled_usec {}\nnr_bursts {}\nburst_usec {}\n",
            stat.nr_periods,
            stat.nr_throttled,
            stat.throttled_ns / 1_000,
            stat.nr_bursts,
            stat.burst_ns / 1_000,
        ));
    } else if cgroup.parent().is_none() {
        // The implicit root CPU css has no bandwidth limiter by definition.
        output.push_str(
            "nr_periods 0\nnr_throttled 0\nthrottled_usec 0\nnr_bursts 0\nburst_usec 0\n",
        );
    }
    output.into_bytes()
}

fn cpu_stat_local(cgroup: &Arc<CgroupNode>) -> Vec<u8> {
    if let Some(group) = cgroup.cpu_group() {
        format!(
            "throttled_usec {}\n",
            group.bandwidth_snapshot().local_throttled_ns / 1_000
        )
        .into_bytes()
    } else if cgroup.parent().is_none() {
        b"throttled_usec 0\n".to_vec()
    } else {
        Vec::new()
    }
}

fn memory_swap_events() -> Vec<u8> {
    b"high 0\nmax 0\nfail 0\n".to_vec()
}

fn is_populated(cgroup: &Arc<CgroupNode>) -> bool {
    cgroup.is_populated()
}

#[cfg(test)]
mod cpu_parse_tests {
    use super::*;

    #[test]
    fn scalar_base_zero_and_overflow_errno() {
        assert_eq!(parse_cpu_unsigned("+0x64\n"), Ok(100));
        assert_eq!(parse_cpu_unsigned("0144"), Ok(100));
        assert_eq!(parse_cpu_unsigned("-1"), Err(SystemError::EINVAL));
        assert_eq!(parse_cpu_unsigned("09"), Err(SystemError::EINVAL));
        assert_eq!(
            parse_cpu_unsigned("18446744073709551616"),
            Err(SystemError::ERANGE)
        );
        assert_eq!(parse_cpu_signed("-0x14"), Ok(-20));
        assert_eq!(parse_cpu_signed("-+1"), Err(SystemError::EINVAL));
        assert_eq!(parse_cpu_signed("-9223372036854775808"), Ok(i64::MIN));
        assert_eq!(
            parse_cpu_signed("9223372036854775808"),
            Err(SystemError::ERANGE)
        );
    }
}
