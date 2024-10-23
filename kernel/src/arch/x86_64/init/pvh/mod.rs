//! x86/HVM启动
//!
//! 初始化代码可参考：https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/platform/pvh/enlighten.c#45
use alloc::string::{String, ToString};
use core::{ffi::CStr, hint::spin_loop};
use param::{E820Type, HvmMemmapTableEntry, HvmStartInfo};
use system_error::SystemError;

use crate::{
    arch::MMArch,
    driver::{
        serial::serial8250::send_to_default_serial8250_port, video::fbdev::base::BootTimeScreenInfo,
    },
    init::{
        boot::{register_boot_callbacks, BootCallbacks, BootloaderAcpiArg},
        boot_params,
    },
    libs::lazy_init::Lazy,
    mm::{memblock::mem_block_manager, MemoryManagementArch, PhysAddr},
};

mod param;

static START_INFO: Lazy<HvmStartInfo> = Lazy::new();

struct PvhBootCallback;

impl BootCallbacks for PvhBootCallback {
    fn init_bootloader_name(&self) -> Result<Option<String>, SystemError> {
        return Ok(Some("x86 PVH".to_string()));
    }

    fn init_acpi_args(&self) -> Result<BootloaderAcpiArg, SystemError> {
        let rsdp_paddr = PhysAddr::new(START_INFO.get().rsdp_paddr as usize);
        if rsdp_paddr.data() != 0 {
            Ok(BootloaderAcpiArg::Rsdp(rsdp_paddr))
        } else {
            Ok(BootloaderAcpiArg::NotProvided)
        }
    }

    fn init_kernel_cmdline(&self) -> Result<(), SystemError> {
        let cmdline_c_str: &CStr = unsafe {
            CStr::from_ptr(
                MMArch::phys_2_virt(PhysAddr::new(START_INFO.get().cmdline_paddr as usize))
                    .unwrap()
                    .data() as *const i8,
            )
        };
        let cmdline = cmdline_c_str.to_str().unwrap();
        boot_params()
            .write_irqsave()
            .boot_cmdline_append(cmdline.as_bytes());
        log::info!("pvh boot cmdline: {:?}", cmdline_c_str);
        Ok(())
    }

    fn early_init_framebuffer_info(
        &self,
        _scinfo: &mut BootTimeScreenInfo,
    ) -> Result<(), SystemError> {
        return Err(SystemError::ENODEV);
    }

    fn early_init_memory_blocks(&self) -> Result<(), SystemError> {
        let start_info = START_INFO.get();
        let mut total_mem_size = 0usize;
        let mut usable_mem_size = 0usize;
        send_to_default_serial8250_port("init_memory_area by pvh boot\n\0".as_bytes());

        if (start_info.version > 0) && start_info.memmap_entries > 0 {
            let mut ep = unsafe {
                MMArch::phys_2_virt(PhysAddr::new(start_info.memmap_paddr as usize)).unwrap()
            }
            .data() as *const HvmMemmapTableEntry;

            for _ in 0..start_info.memmap_entries {
                let entry = unsafe { *ep };
                let start = PhysAddr::new(entry.addr as usize);
                let size = entry.size as usize;
                let typ = E820Type::from(entry.type_);

                total_mem_size += size;
                match typ {
                    param::E820Type::Ram => {
                        usable_mem_size += size;
                        mem_block_manager()
                            .add_block(start, size)
                            .unwrap_or_else(|e| {
                                log::warn!(
                                    "Failed to add memory block: base={:?}, size={:#x}, error={:?}",
                                    start,
                                    size,
                                    e
                                );
                            });
                    }
                    _ => {
                        mem_block_manager()
                            .reserve_block(start, size)
                            .unwrap_or_else(|e| {
                                log::warn!(
                                    "Failed to reserve memory block: base={:?}, size={:#x}, error={:?}",
                                    start,
                                    size,
                                    e
                                );
                            });
                    }
                }
                ep = unsafe { ep.add(1) };
            }
        }
        send_to_default_serial8250_port("init_memory_area_from pvh boot end\n\0".as_bytes());
        log::info!(
            "Total memory size: {:#x}, Usable memory size: {:#x}",
            total_mem_size,
            usable_mem_size
        );
        Ok(())
    }
}

#[inline(never)]
pub(super) fn early_linux32_pvh_init(params_ptr: usize) -> Result<(), SystemError> {
    let start_info = unsafe { *(params_ptr as *const HvmStartInfo) };
    if start_info.magic != HvmStartInfo::XEN_HVM_START_MAGIC_VALUE {
        send_to_default_serial8250_port(
            "early_linux32_pvh_init failed: Magic number not matched.\n\0".as_bytes(),
        );

        loop {
            spin_loop();
        }
    }

    START_INFO.init(start_info);

    register_boot_callbacks(&PvhBootCallback);
    send_to_default_serial8250_port("early_linux32_pvh_init done.\n\0".as_bytes());
    Ok(())
}
