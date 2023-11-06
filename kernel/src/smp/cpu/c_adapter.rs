use alloc::vec::Vec;
use hashbrown::HashSet;

use crate::{driver::acpi::acpi_manager, kdebug};

/// 这是一个临时的函数，用于在acpi、cpu模块被正式实现之前，让原本的C写的smp模块能正常运行
///
/// 请注意！这样写会使得smp模块与x86强耦合。正确的做法是：
/// - 在sysfs中新增acpi firmware
/// - 在acpi初始化的时候，初始化处理器拓扑信息
/// - 初始化cpu模块（加入到sysfs，放置在/sys/devices/system下面）
/// - smp模块从cpu模块处，获取到与架构无关的处理器拓扑信息
/// - smp根据上述信息，初始化指定的处理器（这部分在arch下面实现）
///
/// 但是由于acpi、cpu模块还没有被正式实现，所以暂时使用这个函数来代替，接下来会按照上述步骤进行编写代码
#[no_mangle]
unsafe extern "C" fn rs_smp_get_cpus(res: *mut X86CpuInfo) -> usize {
    let acpi_table = acpi_manager().tables().unwrap();
    let platform_info = acpi_table
        .platform_info()
        .expect("smp_get_cpu_topology(): failed to get platform info");
    let processor_info = platform_info
        .processor_info
        .expect("smp_get_cpu_topology(): failed to get processor info");

    let mut id_set = HashSet::new();
    let mut cpu_info = processor_info
        .application_processors
        .iter()
        .filter_map(|ap| {
            if id_set.contains(&ap.local_apic_id) {
                return None;
            }
            let can_boot = ap.state == acpi::platform::ProcessorState::WaitingForSipi;
            if !can_boot {
                return None;
            }

            id_set.insert(ap.local_apic_id);
            Some(X86CpuInfo::new(
                ap.local_apic_id,
                ap.processor_uid,
                can_boot,
            ))
        })
        .collect::<Vec<_>>();

    let bsp_info = X86CpuInfo::new(
        processor_info.boot_processor.local_apic_id,
        processor_info.boot_processor.processor_uid,
        processor_info.boot_processor.state == acpi::platform::ProcessorState::WaitingForSipi,
    );
    cpu_info.push(bsp_info);

    cpu_info.sort_by(|a, b| a.apic_id.cmp(&b.apic_id));
    kdebug!("cpu_info: {:?}", cpu_info);

    res.copy_from_nonoverlapping(cpu_info.as_ptr(), cpu_info.len());
    return cpu_info.len();
}

/// 这个是临时用于传数据给c版本代码的结构体，请勿用作其他用途
#[repr(C)]
#[derive(Debug)]
struct X86CpuInfo {
    apic_id: u32,
    core_id: u32,
    can_boot: core::ffi::c_char,
}

impl X86CpuInfo {
    fn new(apic_id: u32, core_id: u32, can_boot: bool) -> Self {
        Self {
            apic_id,
            core_id,
            can_boot: can_boot as core::ffi::c_char,
        }
    }
}
