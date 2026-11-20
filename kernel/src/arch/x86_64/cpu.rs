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
    libs::{cpumask::CpuMask, lazy_init::Lazy},
    mm::percpu::PerCpu,
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

/// Package-local topology shared by sysfs and /proc/cpuinfo.
#[derive(Debug)]
pub struct CpuTopologyEntry {
    pub cpu_id: u32,
    pub core_id: u32,
    pub package_id: u32,
    pub thread_siblings: CpuMask,
    pub package_siblings: CpuMask,
    pub cores_per_package: usize,
}

static CPU_TOPOLOGY: Lazy<Vec<CpuTopologyEntry>> = Lazy::new();
/// 保持 `cpuN` 与 `cpuN/topology` 两层 kobject 存活。
static CPU_KOBS: Lazy<Vec<(Arc<CommonKobj>, Arc<CommonKobj>)>> = Lazy::new();

#[derive(Clone, Copy, Default)]
struct ApicIdLayout {
    smt_shift: u32,
    package_shift: u32,
}

impl ApicIdLayout {
    fn package_and_core(self, apic_id: u32) -> (u32, u32) {
        let package_id = apic_id >> self.package_shift;
        let package_local_id = apic_id ^ (package_id << self.package_shift);
        (package_id, package_local_id >> self.smt_shift)
    }
}

/// Parse the SMT level and the highest valid topology level. With leaf 0x1F,
/// the latter can include modules, tiles or dies above the Core level.
fn extended_apic_id_layout(leaf: u32) -> Option<ApicIdLayout> {
    use core::arch::x86_64::__cpuid_count;

    let first = __cpuid_count(leaf, 0);
    if first.ebx & 0xffff == 0 || (first.ecx >> 8) & 0xff != 1 {
        return None;
    }

    let smt_shift = first.eax & 0x1f;
    let mut package_shift = smt_shift;
    for subleaf in 1..32 {
        let level = __cpuid_count(leaf, subleaf);
        if level.ebx & 0xffff == 0 || (level.ecx >> 8) & 0xff == 0 {
            return Some(ApicIdLayout {
                smt_shift,
                package_shift,
            });
        }

        let shift = level.eax & 0x1f;
        if shift < package_shift {
            return None;
        }
        package_shift = shift;
    }
    None
}

fn apic_id_width(count: u32) -> u32 {
    u32::BITS - count.saturating_sub(1).leading_zeros()
}

fn detect_apic_id_layout() -> ApicIdLayout {
    use core::arch::x86_64::{__cpuid, __cpuid_count};

    let max_leaf = __cpuid(0).eax;
    for leaf in [0x1f, 0x0b] {
        if max_leaf >= leaf {
            if let Some(layout) = extended_apic_id_layout(leaf) {
                return layout;
            }
        }
    }

    let cpuid = CpuId::new();
    let Some(features) = cpuid.get_feature_info() else {
        return ApicIdLayout::default();
    };
    match cpuid
        .get_vendor_info()
        .as_ref()
        .map(|vendor| vendor.as_str())
    {
        Some("AuthenticAMD" | "HygonGenuine") => {
            let Some(capacity) = cpuid.get_processor_capacity_feature_info() else {
                return ApicIdLayout::default();
            };
            let package_shift = match capacity.apic_id_size() {
                0 => apic_id_width(capacity.num_phys_threads() as u32),
                width => width as u32,
            };
            // Before Zen, this field describes cores per compute unit.
            let smt_shift = if features.family_id() >= 0x17
                && cpuid
                    .get_extended_processor_and_feature_identifiers()
                    .is_some_and(|features| features.has_topology_extensions())
                && __cpuid(0x8000_0000).eax >= 0x8000_001e
            {
                let topology = __cpuid(0x8000_001e);
                apic_id_width(((topology.ebx >> 8) & 0xff) + 1)
            } else {
                0
            };
            if smt_shift <= package_shift {
                return ApicIdLayout {
                    smt_shift,
                    package_shift,
                };
            }
        }
        Some("GenuineIntel") if features.has_htt() => {
            let logical = u32::from(features.max_logical_processor_ids());
            let cores = if max_leaf >= 4 {
                let cache = __cpuid_count(4, 0);
                if cache.eax & 0x1f == 0 {
                    1
                } else {
                    (cache.eax >> 26) + 1
                }
            } else {
                1
            };
            if logical >= cores {
                return ApicIdLayout {
                    smt_shift: apic_id_width(logical / cores),
                    package_shift: apic_id_width(logical),
                };
            }
        }
        _ => {}
    }

    // Without a topology description, each APIC ID is a standalone processor.
    ApicIdLayout::default()
}

fn init_cpu_topology() -> Result<(), SystemError> {
    if CPU_TOPOLOGY.initialized() {
        return Ok(());
    }

    let cpus = smp_cpu_manager().present_cpus();
    if cpus.is_empty() {
        return Err(SystemError::ENODEV);
    }

    // POSTCORE runs before AP bring-up. All vCPUs in a guest use the same
    // APIC ID layout, while MADT provides each present CPU's full APIC ID.
    let layout = detect_apic_id_layout();
    let mut entries: Vec<_> = cpus
        .iter_cpu()
        .map(|cpu| {
            let apic_id = SMP_BOOT_DATA.phys_id(cpu.data() as usize) as u32;
            let (package_id, core_id) = layout.package_and_core(apic_id);
            CpuTopologyEntry {
                cpu_id: cpu.data(),
                core_id,
                package_id,
                thread_siblings: CpuMask::new(),
                package_siblings: CpuMask::new(),
                cores_per_package: 0,
            }
        })
        .collect();

    let ids: Vec<_> = entries
        .iter()
        .map(|entry| {
            (
                ProcessorId::new(entry.cpu_id),
                entry.package_id,
                entry.core_id,
            )
        })
        .collect();
    for entry in &mut entries {
        let mut cores = Vec::new();
        for &(cpu, package_id, core_id) in &ids {
            if package_id != entry.package_id {
                continue;
            }
            entry.package_siblings.set(cpu, true);
            if core_id == entry.core_id {
                entry.thread_siblings.set(cpu, true);
            }
            if !cores.contains(&core_id) {
                cores.push(core_id);
            }
        }
        entry.cores_per_package = cores.len();
    }

    CPU_TOPOLOGY.init(entries);
    Ok(())
}

/// 获取指定逻辑 CPU 的拓扑信息。
pub fn cpu_topology_entry(cpu_id: u32) -> Option<&'static CpuTopologyEntry> {
    if !CPU_TOPOLOGY.initialized() {
        return None;
    }
    CPU_TOPOLOGY.iter().find(|entry| entry.cpu_id == cpu_id)
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

fn topology_from_kobj(kobj: &Arc<dyn KObject>) -> Result<&'static CpuTopologyEntry, SystemError> {
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
        sysfs_emit_str(buf, &topo.thread_siblings.to_cpulist())
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
        sysfs_emit_str(buf, &topo.package_siblings.to_cpulist())
    }
}

/// Linux 的 `thread_siblings`/`core_siblings` 是十六进制 cpumask 文本。
fn cpumask_hex(cpus: &CpuMask) -> String {
    // Linux prints up to 32 bits per group, padded to nr_cpu_ids.
    const { assert!(PerCpu::MAX_CPU_NUM <= u32::BITS) };
    let width = smp_cpu_manager()
        .possible_cpus()
        .last()
        .map_or(1, |cpu| (cpu.data() as usize + 1).div_ceil(4));
    let mask = cpus
        .iter_cpu()
        .fold(0u32, |mask, cpu| mask | (1 << cpu.data()));
    format!("{mask:0width$x}\n")
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
        sysfs_emit_str(buf, &cpumask_hex(&topo.package_siblings))
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
