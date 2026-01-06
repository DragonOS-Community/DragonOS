//! `/proc/cpuinfo` 文件操作
//! 提供 CPU 信息的只读接口

use crate::{
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{IndexNode, InodeMode},
    },
    smp::cpu::smp_cpu_manager,
};
use alloc::sync::{Arc, Weak};
use alloc::{format, string::String, vec::Vec};
use system_error::SystemError;

/// `/proc/cpuinfo` 文件
#[derive(Debug)]
pub struct CpuInfoFileOps;

impl CpuInfoFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for CpuInfoFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _: crate::libs::mutex::MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let mut data: Vec<u8> = vec![];
        let cpu_manager = smp_cpu_manager();

        // 遍历所有present的CPU
        for cpu_id in cpu_manager.present_cpus().iter_cpu() {
            // 生成每个 CPU 的信息
            let cpu_info = generate_cpu_info(cpu_id);
            data.extend_from_slice(cpu_info.as_bytes());

            // 在每个CPU信息之间添加空行分隔
            data.push(b'\n');
        }

        proc_read(offset, len, buf, &data)
    }
}

/// 生成单个 CPU 的信息字符串
/// 当前使用的 raw_cpuid 库仅支持 x86_64 架构
/// 当在某个线程或核心上执行 CPUID 指令时，它返回的是 当前核心视角下的 CPU 信息。
/// 所以需要通过 sched_setaffinity() 把线程绑定到不同核心，然后在该线程执行 CPUID。
/// 但是目前还没实现sched_setaffinity系统调用
fn generate_cpu_info(cpu_id: crate::smp::cpu::ProcessorId) -> String {
    let mut info = String::new();

    #[cfg(target_arch = "x86_64")]
    {
        use raw_cpuid::CpuId;

        info.push_str(&format!("processor\t: {}\n", cpu_id.data()));

        let cpuid = CpuId::new();

        // 获取厂商信息
        if let Some(vendor_info) = cpuid.get_vendor_info() {
            let vendor_string = vendor_info.as_str();
            if vendor_string != "GenuineIntel" && vendor_string != "AuthenticAMD" {
                info.push_str("vendor_id\t: Unknown\n");
                return info;
            }
            info.push_str(&format!("vendor_id\t: {}\n", vendor_string));
        } else {
            info.push_str("vendor_id\t: Unknown\n");
            return info;
        }

        // 获取 CPU 特性信息
        if let Some(feature_info) = cpuid.get_feature_info() {
            info.push_str(&format!("cpu family\t: {}\n", feature_info.family_id()));
            info.push_str(&format!("model\t\t: {}\n", feature_info.model_id()));
            info.push_str(&format!("stepping\t: {}\n", feature_info.stepping_id()));
        }

        // 获取处理器品牌字符串
        if let Some(brand_string) = cpuid.get_processor_brand_string() {
            info.push_str(&format!("model name\t: {}\n", brand_string.as_str()));
        } else {
            info.push_str("model name\t: Unknown\n");
        }

        // 微代码版本（在虚拟化环境中通常无法获取真实值 && raw_cpuid 库只支持amd cpu获取microcode）
        // info.push_str("microcode\t: 0xffffffff\n");

        // CPU 频率
        if let Some(cpu_fre) = cpuid.get_processor_frequency_info() {
            let cpu_mhz = cpu_fre.processor_base_frequency() as f64;
            info.push_str(&format!("cpu MHz\t\t: {}\n", cpu_mhz));
        } else {
            let tsc_khz = crate::arch::driver::tsc::TSCManager::cpu_khz();
            if tsc_khz > 0 {
                let cpu_mhz = tsc_khz as f64 / 1000.0;
                info.push_str(&format!("cpu MHz\t\t: {:.3}\n", cpu_mhz));
            } else {
                info.push_str("cpu MHz\t\t: unknown\n");
            }
        }

        // L3缓存大小
        if let Some(cache_params) = cpuid.get_cache_parameters() {
            for cache in cache_params {
                if cache.level() == 3 {
                    let cache_size =
                        cache.sets() * cache.associativity() * cache.coherency_line_size();
                    info.push_str(&format!("cache size\t: {} KB\n", cache_size / 1024));
                    break;
                }
            }
        } else {
            info.push_str("cache size\t: unknown\n");
        }

        let boot_data = &crate::arch::x86_64::smp::SMP_BOOT_DATA;

        // // 物理 CPU 封装 ID

        // siblings - 同一个物理cpu中的逻辑处理器数量
        let siblings = smp_cpu_manager().present_cpus_count();
        info.push_str(&format!("siblings\t: {}\n", siblings));

        // // core id - 在同一个物理cpu内的核心ID

        // // cpu cores - 每个物理cpu中的核心数

        // APIC ID // 不确定是哪个
        let apic_id = boot_data.phys_id(cpu_id.data() as usize);
        info.push_str(&format!("apicid\t\t: {}\n", apic_id));
        info.push_str(&format!("initial apicid\t: {}\n", apic_id));

        // FPU 和其他特性
        if let Some(feature_info) = cpuid.get_feature_info() {
            info.push_str(&format!(
                "fpu\t\t: {}\n",
                if feature_info.has_fpu() { "yes" } else { "no" }
            ));
            // info.push_str("fpu_exception\t: yes\n");

            // CPUID level
            // if let Some(vendor_info) = cpuid.get_extended_feature_info() {
            //     info.push_str(&format!("cpuid level\t: {}\n", vendor_info.));
            // }

            // wp 标志
            let cr0: u64;
            unsafe {
                core::arch::asm!("mov {}, cr0", out(reg) cr0);
            }
            let wp_enabled = (cr0 >> 16) & 1 != 0;
            info.push_str(&format!(
                "wp\t\t: {}\n",
                if wp_enabled { "yes" } else { "no" }
            ));

            // CPU 特性标志 - 只包含真实检测到的特性
            info.push_str("flags\t\t: ");
            let mut flags = Vec::new();

            if feature_info.has_fpu() {
                flags.push("fpu");
            }
            if feature_info.has_vme() {
                flags.push("vme");
            }
            if feature_info.has_de() {
                flags.push("de");
            }
            if feature_info.has_pse() {
                flags.push("pse");
            }
            if feature_info.has_tsc() {
                flags.push("tsc");
            }
            if feature_info.has_msr() {
                flags.push("msr");
            }
            if feature_info.has_pae() {
                flags.push("pae");
            }
            if feature_info.has_mce() {
                flags.push("mce");
            }
            if feature_info.has_cmpxchg8b() {
                flags.push("cx8");
            }
            if feature_info.has_apic() {
                flags.push("apic");
            }
            if feature_info.has_sysenter_sysexit() {
                flags.push("sep");
            }
            if feature_info.has_mtrr() {
                flags.push("mtrr");
            }
            if feature_info.has_pge() {
                flags.push("pge");
            }
            if feature_info.has_mca() {
                flags.push("mca");
            }
            if feature_info.has_cmov() {
                flags.push("cmov");
            }
            if feature_info.has_pat() {
                flags.push("pat");
            }
            if feature_info.has_pse36() {
                flags.push("pse36");
            }
            if feature_info.has_clflush() {
                flags.push("clflush");
            }
            if feature_info.has_mmx() {
                flags.push("mmx");
            }
            if feature_info.has_fxsave_fxstor() {
                flags.push("fxsr");
            }
            if feature_info.has_sse() {
                flags.push("sse");
            }
            if feature_info.has_sse2() {
                flags.push("sse2");
            }
            if feature_info.has_ss() {
                flags.push("ss");
            }
            if feature_info.has_htt() {
                flags.push("ht");
            }

            info.push_str(&flags.join(" "));
            info.push('\n');

            // bugs 内核根据 CPUID、微码版本以及已知漏洞列表汇总出来
            // 这里暂时不实现

            info.push_str(&format!(
                "clflush size\t: {}\n",
                feature_info.cflush_cache_line_size()
            ));
            info.push_str(&format!(
                "cache_alignment\t: {}\n",
                feature_info.cflush_cache_line_size()
            ));
        }

        // BogoMIPS（基于TSC频率的简化计算）在虚拟机中无意义 暂时不实现
        // let tsc_khz = crate::arch::driver::tsc::TSCManager::cpu_khz();
        // if tsc_khz > 0 {
        //     let bogomips = (tsc_khz as f64 / 1000.0) * 2.0;
        //     info.push_str(&format!("bogomips\t: {:.2}\n", bogomips));
        // }

        // info.push_str("cache_alignment\t: 64\n");
        if let Some(processor_capacity_feature_info) = cpuid.get_processor_capacity_feature_info() {
            info.push_str(&format!(
                "address sizes\t: {} bits physical, {} bits virtual\n",
                processor_capacity_feature_info.physical_address_bits(),
                processor_capacity_feature_info.linear_address_bits()
            ));
        }
        // info.push_str("power management:\n");
    }

    info
}
