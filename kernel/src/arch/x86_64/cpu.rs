use core::hint::spin_loop;

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use raw_cpuid::CpuId;
use system_error::SystemError;
use unified_init::macros::unified_init;
use x86::msr::{rdmsr, IA32_APIC_BASE, IA32_X2APIC_APICID};

use crate::{
    arch::{process::table::TSSManager, smp::SMP_BOOT_DATA},
    driver::base::kobject::{CommonKobj, DynamicKObjKType, KObject, KObjectManager},
    filesystem::{
        sysfs::{file::sysfs_emit_str, sysfs_instance, Attribute, SysFSOpsSupport},
        vfs::InodeMode,
    },
    init::initcall::INITCALL_POSTCORE,
    libs::lazy_init::Lazy,
    smp::cpu::{smp_cpu_manager, ProcessorId},
};

/// 获取当前CPU的逻辑编号
#[inline]
pub fn current_cpu_id() -> ProcessorId {
    if !SMP_BOOT_DATA.is_initialized() {
        return ProcessorId::new(0);
    }

    if let Some(cpu) = TSSManager::current_cpu_from_tr() {
        return cpu;
    }

    let x2apic_id = current_x2apic_physical_id();
    if let Some(apic_id) = x2apic_id {
        if let Some(cpu_id) = phys_apic_id_to_cpu_id(apic_id) {
            return cpu_id;
        }
    }

    let cpuid_apic_id = current_apic_physical_id();
    if let Some(cpu_id) = phys_apic_id_to_cpu_id(cpuid_apic_id) {
        return cpu_id;
    }

    panic!(
        "current cpu apic id is not present in SMP_BOOT_DATA: x2apic={x2apic_id:?}, cpuid={cpuid_apic_id}, bsp_phys_id={}",
        SMP_BOOT_DATA.bsp_phys_id()
    );
}

/// 获取当前CPU的物理APIC ID
#[inline]
pub fn current_apic_physical_id() -> usize {
    let cpuid = CpuId::new();

    if let Some(topology) = cpuid.get_extended_topology_info() {
        if let Some(apic_id) = topology.map(|level| level.x2apic_id()).next() {
            return apic_id as usize;
        }
    }

    if let Some(topology) = cpuid.get_processor_topology_info() {
        return topology.x2apic_id() as usize;
    }

    if let Some(feature_info) = cpuid.get_feature_info() {
        return feature_info.initial_local_apic_id() as usize;
    }

    return 0;
}

#[inline]
fn current_x2apic_physical_id() -> Option<usize> {
    let apic_base = unsafe { rdmsr(IA32_APIC_BASE) };
    if (apic_base & (1 << 10)) == 0 {
        return None;
    }

    Some(unsafe { rdmsr(IA32_X2APIC_APICID) as usize })
}

#[inline]
pub fn phys_apic_id_to_cpu_id(apic_id: usize) -> Option<ProcessorId> {
    if !SMP_BOOT_DATA.is_initialized() {
        return Some(ProcessorId::new(apic_id as u32));
    }

    for cpu in 0..SMP_BOOT_DATA.cpu_count() {
        if SMP_BOOT_DATA.phys_id(cpu) == apic_id {
            return Some(ProcessorId::new(cpu as u32));
        }
    }

    None
}

/// 重置cpu
#[allow(dead_code)]
pub unsafe fn cpu_reset() -> ! {
    // 重启计算机
    unsafe { x86::io::outb(0x64, 0xfe) };
    loop {
        spin_loop();
    }
}

/// 单个逻辑CPU的拓扑信息。
///
/// 这些值来自 CPUID 拓扑叶子与 ACPI MADT 中的 APIC ID，供
/// `/sys/devices/system/cpu/cpuN/topology/*` 和 `/proc/cpuinfo` 使用。
#[derive(Clone, Debug)]
pub struct CpuTopologyEntry {
    pub cpu_id: u32,
    pub core_id: u32,
    pub package_id: u32,
    pub thread_siblings: Vec<u32>,
    pub core_siblings: Vec<u32>,
    pub cores_per_package: u32,
}

static CPU_TOPOLOGY: Lazy<Vec<CpuTopologyEntry>> = Lazy::new();
/// 保持 `cpuN` 与 `cpuN/topology` 两层 kobject 存活。
static CPU_KOBS: Lazy<Vec<(Arc<CommonKobj>, Arc<CommonKobj>)>> = Lazy::new();

/// 探测 CPUID 0xB 给出的 SMT/Core 层级位移。
///
/// 返回 `(has_core_level, smt_shift, core_shift, cores_per_package)`。
/// 没有可用的 Core 层级时返回平坦拓扑，每个逻辑 CPU 视为一个独立核心。
fn detect_topology_layout() -> (bool, u32, u32, u32) {
    let cpuid = CpuId::new();
    let mut smt_shift = 0u32;
    let mut core_shift = 0u32;
    let mut smt_processors = 1u32;
    let mut core_processors = 0u32;
    let mut has_core_level = false;

    if let Some(levels) = cpuid.get_extended_topology_info() {
        for level in levels {
            match level.level_type() {
                raw_cpuid::TopologyType::SMT => {
                    smt_shift = level.shift_right_for_next_apic_id();
                    smt_processors = u32::from(level.processors()).max(1);
                }
                raw_cpuid::TopologyType::Core => {
                    core_shift = level.shift_right_for_next_apic_id();
                    core_processors = u32::from(level.processors());
                    has_core_level = true;
                }
                _ => {}
            }
        }
    }

    // Core 层级必须位于 SMT 之上，否则说明拓扑信息不可用。
    if !has_core_level || core_shift <= smt_shift || core_shift > 31 {
        return (false, 0, 0, 0);
    }

    let cores_per_package = if core_processors == 0 {
        0
    } else {
        core_processors / smt_processors
    };
    (true, smt_shift, core_shift, cores_per_package)
}

/// 初始化并缓存所有 present CPU 的拓扑。
pub fn init_cpu_topology() -> Result<(), SystemError> {
    if CPU_TOPOLOGY.initialized() {
        return Ok(());
    }

    let mut cpus: Vec<ProcessorId> = smp_cpu_manager().present_cpus().iter_cpu().collect();
    if cpus.is_empty() {
        cpus = smp_cpu_manager().possible_cpus().iter_cpu().collect();
    }
    if cpus.is_empty() {
        return Err(SystemError::ENODEV);
    }

    let (has_core_level, smt_shift, core_shift, cores_per_package) = detect_topology_layout();
    let mut entries = Vec::with_capacity(cpus.len());
    for cpu in &cpus {
        let apic_id = SMP_BOOT_DATA.phys_id(cpu.data() as usize) as u32;
        let (package_id, core_id) = if has_core_level {
            let core_width = core_shift - smt_shift;
            let core_id = if core_width == 0 {
                0
            } else {
                (apic_id >> smt_shift) & ((1u32 << core_width) - 1)
            };
            (apic_id >> core_shift, core_id)
        } else {
            (0, apic_id)
        };

        entries.push(CpuTopologyEntry {
            cpu_id: cpu.data(),
            core_id,
            package_id,
            thread_siblings: Vec::new(),
            core_siblings: Vec::new(),
            cores_per_package: if has_core_level && cores_per_package > 0 {
                cores_per_package
            } else {
                cpus.len() as u32
            },
        });
    }

    for i in 0..entries.len() {
        let mut thread_siblings = Vec::new();
        let mut core_siblings = Vec::new();
        for other in entries.iter() {
            if has_core_level {
                if other.package_id == entries[i].package_id && other.core_id == entries[i].core_id
                {
                    thread_siblings.push(other.cpu_id);
                }
                if other.package_id == entries[i].package_id {
                    core_siblings.push(other.cpu_id);
                }
            } else {
                // 无拓扑信息时，每个逻辑 CPU 单独构成一个 core，
                // package 内的所有 CPU 共享同一个 package。
                thread_siblings.push(entries[i].cpu_id);
                core_siblings.push(other.cpu_id);
            }
        }
        entries[i].thread_siblings = thread_siblings;
        entries[i].core_siblings = core_siblings;
    }

    CPU_TOPOLOGY.init(entries);
    Ok(())
}

/// 获取指定逻辑 CPU 的拓扑信息。
pub fn cpu_topology_entry(cpu_id: u32) -> Option<CpuTopologyEntry> {
    if !CPU_TOPOLOGY.initialized() {
        return None;
    }
    CPU_TOPOLOGY
        .iter()
        .find(|entry| entry.cpu_id == cpu_id)
        .cloned()
}

/// 将 CPU 编号列表格式化为 Linux 的 cpulist 形式（如 `0,2-3`）。
fn format_cpu_list(cpus: &[u32]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < cpus.len() {
        let start = cpus[i];
        let mut end = start;
        while i + 1 < cpus.len() && cpus[i + 1] == end + 1 {
            i += 1;
            end = cpus[i];
        }
        if !out.is_empty() {
            out.push(',');
        }
        if start == end {
            out.push_str(&format!("{start}"));
        } else {
            out.push_str(&format!("{start}-{end}"));
        }
        i += 1;
    }
    out
}

fn parse_cpu_id(name: &str) -> Option<u32> {
    name.strip_prefix("cpu")?.parse::<u32>().ok()
}

fn cpu_id_from_kobj(kobj: &Arc<dyn KObject>) -> Result<u32, SystemError> {
    // 拓扑属性挂在 cpuN/topology 上，需要从父目录解析逻辑 CPU 编号。
    if let Some(cpu_id) = parse_cpu_id(&kobj.name()) {
        return Ok(cpu_id);
    }
    if let Some(parent) = kobj.parent().and_then(|parent| parent.upgrade()) {
        if let Some(cpu_id) = parse_cpu_id(&parent.name()) {
            return Ok(cpu_id);
        }
    }
    Err(SystemError::EINVAL)
}

fn topology_from_kobj(kobj: &Arc<dyn KObject>) -> Result<CpuTopologyEntry, SystemError> {
    let cpu_id = cpu_id_from_kobj(kobj)?;
    cpu_topology_entry(cpu_id).ok_or(SystemError::ENOENT)
}

#[derive(Debug)]
struct AttrCoreId;

impl Attribute for AttrCoreId {
    fn name(&self) -> &str {
        "core_id"
    }

    fn mode(&self) -> InodeMode {
        InodeMode::S_IRUGO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let topo = topology_from_kobj(&kobj)?;
        sysfs_emit_str(buf, &format!("{}\n", topo.core_id))
    }
}

#[derive(Debug)]
struct AttrPhysicalPackageId;

impl Attribute for AttrPhysicalPackageId {
    fn name(&self) -> &str {
        "physical_package_id"
    }

    fn mode(&self) -> InodeMode {
        InodeMode::S_IRUGO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let topo = topology_from_kobj(&kobj)?;
        sysfs_emit_str(buf, &format!("{}\n", topo.package_id))
    }
}

#[derive(Debug)]
struct AttrThreadSiblingsList;

impl Attribute for AttrThreadSiblingsList {
    fn name(&self) -> &str {
        "thread_siblings_list"
    }

    fn mode(&self) -> InodeMode {
        InodeMode::S_IRUGO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let topo = topology_from_kobj(&kobj)?;
        sysfs_emit_str(
            buf,
            &format!("{}\n", format_cpu_list(&topo.thread_siblings)),
        )
    }
}

#[derive(Debug)]
struct AttrCoreSiblingsList;

impl Attribute for AttrCoreSiblingsList {
    fn name(&self) -> &str {
        "core_siblings_list"
    }

    fn mode(&self) -> InodeMode {
        InodeMode::S_IRUGO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let topo = topology_from_kobj(&kobj)?;
        sysfs_emit_str(buf, &format!("{}\n", format_cpu_list(&topo.core_siblings)))
    }
}

/// Linux 的 `thread_siblings`/`core_siblings` 是十六进制 cpumask 文本。
fn cpumask_hex(cpus: &[u32]) -> String {
    let mut mask = 0u64;
    for &cpu in cpus {
        if (cpu as usize) < 64 {
            mask |= 1u64 << cpu;
        }
    }
    format!("{mask:x}\n")
}

#[derive(Debug)]
struct AttrThreadSiblings;

impl Attribute for AttrThreadSiblings {
    fn name(&self) -> &str {
        "thread_siblings"
    }

    fn mode(&self) -> InodeMode {
        InodeMode::S_IRUGO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let topo = topology_from_kobj(&kobj)?;
        sysfs_emit_str(buf, &cpumask_hex(&topo.thread_siblings))
    }
}

#[derive(Debug)]
struct AttrCoreSiblings;

impl Attribute for AttrCoreSiblings {
    fn name(&self) -> &str {
        "core_siblings"
    }

    fn mode(&self) -> InodeMode {
        InodeMode::S_IRUGO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let topo = topology_from_kobj(&kobj)?;
        sysfs_emit_str(buf, &cpumask_hex(&topo.core_siblings))
    }
}

#[unified_init(INITCALL_POSTCORE)]
fn cpu_topology_sysfs_init() -> Result<(), SystemError> {
    init_cpu_topology()?;
    if CPU_KOBS.initialized() {
        return Ok(());
    }

    let root = crate::driver::base::cpu::cpu_root_kobj().ok_or(SystemError::ENODEV)?;
    let mut kobs = Vec::with_capacity(CPU_TOPOLOGY.len());
    for entry in CPU_TOPOLOGY.iter() {
        let cpu_kobj = CommonKobj::new(format!("cpu{}", entry.cpu_id));
        cpu_kobj.set_parent(Some(Arc::downgrade(&root)));
        KObjectManager::init_and_add_kobj(cpu_kobj.clone(), Some(&DynamicKObjKType))?;

        let topology_kobj = CommonKobj::new("topology".to_string());
        let cpu_kobj_dyn: Arc<dyn KObject> = cpu_kobj.clone();
        topology_kobj.set_parent(Some(Arc::downgrade(&cpu_kobj_dyn)));
        KObjectManager::init_and_add_kobj(topology_kobj.clone(), Some(&DynamicKObjKType))?;

        let topology_kobj_dyn: Arc<dyn KObject> = topology_kobj.clone();
        sysfs_instance().create_file(&topology_kobj_dyn, &AttrCoreId)?;
        sysfs_instance().create_file(&topology_kobj_dyn, &AttrPhysicalPackageId)?;
        sysfs_instance().create_file(&topology_kobj_dyn, &AttrThreadSiblingsList)?;
        sysfs_instance().create_file(&topology_kobj_dyn, &AttrCoreSiblingsList)?;

        sysfs_instance().create_file(&topology_kobj_dyn, &AttrThreadSiblings)?;
        sysfs_instance().create_file(&topology_kobj_dyn, &AttrCoreSiblings)?;

        kobs.push((cpu_kobj, topology_kobj));
    }
    CPU_KOBS.init(kobs);
    Ok(())
}
