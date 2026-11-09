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

/// 从指定 CPUID 拓扑叶子读取 SMT 层级的 APIC ID 位移。
///
/// `CPUID.1F` 只表示该叶子被 CPU 支持，并不保证其内容有效：QEMU/KVM
/// 可能返回全零的子叶。因此必须检查 subleaf 0 的 level type；内容无效
/// 时返回 None，让调用方回退到 0xB。
fn smt_shift_from_leaf(leaf: u32) -> Option<u32> {
    for subleaf in 0..16 {
        let result = core::arch::x86_64::__cpuid_count(leaf, subleaf);
        let level_type = (result.ecx >> 8) & 0xff;

        if level_type == 0 {
            // subleaf 0 都没有有效层级，说明该叶子不可用。
            if subleaf == 0 {
                return None;
            }
            break;
        }
        if level_type == 1 {
            // SMT 层的 shift_right_for_next_apic_id，位于 EAX[4:0]。
            return Some(result.eax & 0x1f);
        }
        if subleaf == 0 {
            // 有效拓扑，但首个层级不是 SMT（threads=1）。
            return Some(0);
        }
    }
    None
}

/// 读取 SMT 层级的 APIC ID 位移：优先使用有效的 0x1F，回退到 0xB。
fn detect_smt_shift() -> u32 {
    for leaf in [0x1f, 0x0b] {
        if let Some(shift) = smt_shift_from_leaf(leaf) {
            return shift;
        }
    }
    0
}

/// 初始化并缓存所有 present CPU 的拓扑。
///
/// DragonOS 面向容器/沙箱 guest，当前按单 socket 约定导出拓扑：
/// `physical_package_id` 固定为 0，`core_id` 吸收 SMT 以上的 Die/Tile
/// 位，因此多 Die 的单 socket 仍能保持 core ID 唯一。多 socket guest
/// 不在该 ABI 的覆盖范围内，会被报告为一个 socket。
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

    let smt_shift = detect_smt_shift();
    let mut entries = Vec::with_capacity(cpus.len());
    for cpu in &cpus {
        let apic_id = SMP_BOOT_DATA.phys_id(cpu.data() as usize) as u32;
        entries.push(CpuTopologyEntry {
            cpu_id: cpu.data(),
            core_id: apic_id >> smt_shift,
            package_id: 0,
            thread_siblings: Vec::new(),
            core_siblings: Vec::new(),
            cores_per_package: 0,
        });
    }

    let mut distinct_cores: Vec<u32> = Vec::new();
    for entry in &entries {
        if !distinct_cores.contains(&entry.core_id) {
            distinct_cores.push(entry.core_id);
        }
    }
    let cores_per_package = distinct_cores.len() as u32;
    let all_cpus: Vec<u32> = entries.iter().map(|entry| entry.cpu_id).collect();

    for i in 0..entries.len() {
        let mut thread_siblings = Vec::new();
        for other in entries.iter() {
            if other.core_id == entries[i].core_id {
                thread_siblings.push(other.cpu_id);
            }
        }
        entries[i].thread_siblings = thread_siblings;
        entries[i].core_siblings = all_cpus.clone();
        entries[i].cores_per_package = cores_per_package.max(1);
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
        // 单 u64 cpumask 的硬上限；当前 PerCpu::MAX_CPU_NUM=16。
        debug_assert!((cpu as usize) < 64, "cpu {cpu} exceeds 64-bit cpumask");
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
