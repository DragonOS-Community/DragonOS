use core::ffi::CStr;

use alloc::string::{String, ToString};

use multiboot::MultibootInfo;
use system_error::SystemError;

use crate::{
    arch::MMArch,
    driver::{
        serial::serial8250::send_to_default_serial8250_port,
        video::fbdev::{
            base::{BootTimeScreenInfo, BootTimeVideoType},
            vesafb::vesafb_early_map,
        },
    },
    init::{
        boot::{register_boot_callbacks, BootCallbacks, BootloaderAcpiArg},
        boot_params,
    },
    libs::lazy_init::Lazy,
    mm::{memblock::mem_block_manager, MemoryManagementArch, PhysAddr},
};

static MB1_INFO: Lazy<MultibootInfo> = Lazy::new();

struct Mb1Ops;

impl multiboot::MultibootOps for Mb1Ops {
    fn phys_2_virt(&self, paddr: usize) -> usize {
        unsafe { MMArch::phys_2_virt(PhysAddr::new(paddr)).unwrap().data() }
    }
}
struct Mb1Callback;

impl BootCallbacks for Mb1Callback {
    fn init_bootloader_name(&self) -> Result<Option<String>, SystemError> {
        let info = MB1_INFO.get();
        if info.boot_loader_name != 0 {
            // SAFETY: the bootloader name is C-style zero-terminated string.
            unsafe {
                let cstr_ptr =
                    MMArch::phys_2_virt(PhysAddr::new(info.boot_loader_name as usize)).unwrap();
                let cstr = CStr::from_ptr(cstr_ptr.data() as *const i8);

                let result = cstr.to_str().unwrap_or("unknown").to_string();
                return Ok(Some(result));
            }
        }
        Ok(None)
    }

    fn init_acpi_args(&self) -> Result<BootloaderAcpiArg, SystemError> {
        // MB1不提供rsdp信息。因此，将来需要让内核支持从UEFI获取RSDP表。
        Ok(BootloaderAcpiArg::NotProvided)
    }

    fn init_kernel_cmdline(&self) -> Result<(), SystemError> {
        let info = MB1_INFO.get();

        if !info.has_cmdline() {
            log::debug!("No kernel command line found in multiboot1 info");
            return Ok(());
        }

        if let Some(cmdline) = unsafe { info.cmdline(&Mb1Ops) } {
            let mut guard = boot_params().write_irqsave();
            guard.boot_cmdline_append(cmdline.as_bytes());

            log::info!("Kernel command line: {}\n", cmdline);
        }

        Ok(())
    }

    fn early_init_framebuffer_info(
        &self,
        scinfo: &mut BootTimeScreenInfo,
    ) -> Result<(), SystemError> {
        let info = MB1_INFO.get();
        let fb_table = info.framebuffer_table;
        let width = fb_table.width;
        let height = fb_table.height;
        scinfo.is_vga = true;
        scinfo.lfb_base = PhysAddr::new(fb_table.paddr as usize);
        let fb_type = fb_table.color_info().unwrap();

        match fb_type {
            multiboot::ColorInfoType::Palette(_) => todo!(),
            multiboot::ColorInfoType::Rgb(rgb) => {
                scinfo.lfb_width = width;
                scinfo.lfb_height = height;
                scinfo.video_type = BootTimeVideoType::Vlfb;
                scinfo.lfb_depth = fb_table.bpp;
                scinfo.red_pos = rgb.red_field_position;
                scinfo.red_size = rgb.red_mask_size;
                scinfo.green_pos = rgb.green_field_position;
                scinfo.green_size = rgb.green_mask_size;
                scinfo.blue_pos = rgb.blue_field_position;
                scinfo.blue_size = rgb.blue_mask_size;
            }
            multiboot::ColorInfoType::Text => {
                scinfo.origin_video_cols = width as u8;
                scinfo.origin_video_lines = height as u8;
                scinfo.video_type = BootTimeVideoType::Mda;
                scinfo.lfb_depth = 8;
            }
        }
        scinfo.lfb_size = (width * height * ((scinfo.lfb_depth as u32 + 7) / 8)) as usize;

        scinfo.lfb_virt_base = Some(vesafb_early_map(scinfo.lfb_base, scinfo.lfb_size)?);

        return Ok(());
    }

    fn early_init_memory_blocks(&self) -> Result<(), SystemError> {
        let info = MB1_INFO.get();
        let mut total_mem_size = 0usize;
        let mut usable_mem_size = 0usize;
        for entry in unsafe { info.memory_map(&Mb1Ops) } {
            let start = PhysAddr::new(entry.base_addr() as usize);
            let size = entry.length() as usize;
            let area_typ = entry.memory_type();
            total_mem_size += size;

            match area_typ {
                multiboot::MemoryType::Available => {
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
        }
        send_to_default_serial8250_port("init_memory_area_from_multiboot1 end\n\0".as_bytes());
        log::info!(
            "Total memory size: {:#x}, Usable memory size: {:#x}",
            total_mem_size,
            usable_mem_size
        );

        if let Some(modules_iter) = unsafe { info.modules(&Mb1Ops) } {
            for m in modules_iter {
                let base = PhysAddr::new(m.start() as usize);
                let size = m.end() as usize - m.start() as usize;
                mem_block_manager()
                    .reserve_block(base, size)
                    .unwrap_or_else(|e| {
                        log::warn!(
                        "Failed to reserve modules memory block: base={:?}, size={:#x}, error={:?}",
                        base,
                        size,
                        e
                    );
                    });
            }
        }

        Ok(())
    }
}

pub(super) fn early_multiboot_init(boot_magic: u32, boot_info: u64) -> Result<(), SystemError> {
    assert_eq!(boot_magic, multiboot::MAGIC);
    let boot_info = unsafe { MMArch::phys_2_virt(PhysAddr::new(boot_info as usize)).unwrap() };
    let mb1_info = unsafe { (boot_info.data() as *const MultibootInfo).as_ref().unwrap() };
    MB1_INFO.init(*mb1_info);

    register_boot_callbacks(&Mb1Callback);

    Ok(())
}
