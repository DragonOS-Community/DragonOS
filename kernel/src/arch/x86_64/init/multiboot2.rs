use core::hint::spin_loop;

use acpi::rsdp::Rsdp;
use alloc::string::{String, ToString};
use multiboot2::{BootInformation, BootInformationHeader, MemoryAreaType, RsdpV1Tag};
use system_error::SystemError;

use crate::{
    arch::mm::x86_64_set_kernel_load_base_paddr,
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
    mm::{memblock::mem_block_manager, PhysAddr},
};

pub(super) const MULTIBOOT2_ENTRY_MAGIC: u32 = multiboot2::MAGIC;
static MB2_INFO: Lazy<BootInformation> = Lazy::new();
const MB2_RAW_INFO_MAX_SIZE: usize = 4096;

static mut MB2_RAW_INFO: [u8; MB2_RAW_INFO_MAX_SIZE] = [0u8; MB2_RAW_INFO_MAX_SIZE];

fn mb2_rsdp_v1_tag_to_rsdp_struct(tag: &RsdpV1Tag) -> Rsdp {
    Rsdp {
        signature: tag.signature,
        checksum: tag.checksum,
        oem_id: tag.oem_id,
        revision: tag.revision,
        rsdt_address: tag.rsdt_address,
        length: 0,
        xsdt_address: 0,
        ext_checksum: 0,
        reserved: [0u8; 3],
    }
}

fn mb2_rsdp_v2_tag_to_rsdp_struct(tag: &multiboot2::RsdpV2Tag) -> Rsdp {
    Rsdp {
        signature: tag.signature,
        checksum: tag.checksum,
        oem_id: tag.oem_id,
        revision: tag.revision,
        rsdt_address: tag.rsdt_address,
        length: tag.length,
        xsdt_address: tag.xsdt_address,
        ext_checksum: tag.ext_checksum,
        reserved: tag._reserved,
    }
}
struct Mb2Callback;

impl BootCallbacks for Mb2Callback {
    fn init_bootloader_name(&self) -> Result<Option<String>, SystemError> {
        let name = MB2_INFO
            .get()
            .boot_loader_name_tag()
            .expect("MB2: Bootloader name tag not found!")
            .name()
            .expect("Failed to parse bootloader name!")
            .to_string();
        Ok(Some(name))
    }

    fn init_acpi_args(&self) -> Result<BootloaderAcpiArg, SystemError> {
        if let Some(v1_tag) = MB2_INFO.get().rsdp_v1_tag() {
            Ok(BootloaderAcpiArg::Rsdt(mb2_rsdp_v1_tag_to_rsdp_struct(
                v1_tag,
            )))
        } else if let Some(v2_tag) = MB2_INFO.get().rsdp_v2_tag() {
            Ok(BootloaderAcpiArg::Xsdt(mb2_rsdp_v2_tag_to_rsdp_struct(
                v2_tag,
            )))
        } else {
            Ok(BootloaderAcpiArg::NotProvided)
        }
    }

    fn init_kernel_cmdline(&self) -> Result<(), SystemError> {
        let cmdline = MB2_INFO
            .get()
            .command_line_tag()
            .expect("Mb2: Command line tag not found!")
            .cmdline()
            .expect("Mb2: Failed to parse command line!");
        boot_params()
            .write_irqsave()
            .boot_cmdline_append(cmdline.as_bytes());
        Ok(())
    }

    fn early_init_framebuffer_info(
        &self,
        scinfo: &mut BootTimeScreenInfo,
    ) -> Result<(), SystemError> {
        let Some(Ok(fb_tag)) = MB2_INFO.get().framebuffer_tag() else {
            return Err(SystemError::ENODEV);
        };
        let width = fb_tag.width();
        let height = fb_tag.height();
        scinfo.is_vga = true;
        scinfo.lfb_base = PhysAddr::new(fb_tag.address() as usize);

        let fb_type = fb_tag.buffer_type().unwrap();
        match fb_type {
            multiboot2::FramebufferType::Indexed { palette: _ } => todo!(),
            multiboot2::FramebufferType::RGB { red, green, blue } => {
                scinfo.lfb_width = width;
                scinfo.lfb_height = height;
                scinfo.video_type = BootTimeVideoType::Vlfb;
                scinfo.lfb_depth = fb_tag.bpp();
                scinfo.red_pos = red.position;
                scinfo.red_size = red.size;
                scinfo.green_pos = green.position;
                scinfo.green_size = green.size;
                scinfo.blue_pos = blue.position;
                scinfo.blue_size = blue.size;
            }
            multiboot2::FramebufferType::Text => {
                scinfo.origin_video_cols = width as u8;
                scinfo.origin_video_lines = height as u8;
                scinfo.video_type = BootTimeVideoType::Mda;
                scinfo.lfb_depth = 8;
            }
        };

        scinfo.lfb_size = (width * height * ((scinfo.lfb_depth as u32 + 7) / 8)) as usize;

        scinfo.lfb_virt_base = Some(vesafb_early_map(scinfo.lfb_base, scinfo.lfb_size)?);

        return Ok(());
    }

    fn early_init_memory_blocks(&self) -> Result<(), SystemError> {
        let mb2_info = MB2_INFO.get();
        send_to_default_serial8250_port("init_memory_area_from_multiboot2\n\0".as_bytes());

        let mem_regions_tag = mb2_info
            .memory_map_tag()
            .expect("MB2: Memory map tag not found!");
        let mut total_mem_size = 0usize;
        let mut usable_mem_size = 0usize;
        for region in mem_regions_tag.memory_areas() {
            let start = PhysAddr::new(region.start_address() as usize);
            let size = region.size() as usize;
            let area_typ = MemoryAreaType::from(region.typ());
            total_mem_size += size;

            match area_typ {
                MemoryAreaType::Available => {
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
        send_to_default_serial8250_port("init_memory_area_from_multiboot2 end\n\0".as_bytes());
        log::info!(
            "Total memory size: {:#x}, Usable memory size: {:#x}",
            total_mem_size,
            usable_mem_size
        );

        // Add the boot module region since Grub does not specify it.
        let mb2_module_tag = mb2_info.module_tags();
        for module in mb2_module_tag {
            let start = PhysAddr::new(module.start_address() as usize);
            let size = module.module_size() as usize;
            mem_block_manager()
                .reserve_block(start, size)
                .unwrap_or_else(|e| {
                    log::warn!(
                        "Failed to reserve memory block for mb2 modules: base={:?}, size={:#x}, error={:?}",
                        start,
                        size,
                        e
                    );
                });
        }

        // setup kernel load base
        self.setup_kernel_load_base();

        Ok(())
    }
}

impl Mb2Callback {
    fn setup_kernel_load_base(&self) {
        let mb2_info = MB2_INFO.get();
        let kernel_start = mb2_info
            .load_base_addr_tag()
            .expect("MB2: Load base address tag not found!")
            .load_base_addr();
        let loadbase = PhysAddr::new(kernel_start as usize);
        x86_64_set_kernel_load_base_paddr(loadbase);
    }
}
pub(super) fn early_multiboot2_init(boot_magic: u32, boot_info: u64) -> Result<(), SystemError> {
    assert_eq!(boot_magic, MULTIBOOT2_ENTRY_MAGIC);
    let bi_ptr = boot_info as usize as *const BootInformationHeader;
    let bi_size = unsafe { (*bi_ptr).total_size() as usize };
    assert!(bi_size <= MB2_RAW_INFO_MAX_SIZE);
    unsafe {
        core::ptr::copy_nonoverlapping(bi_ptr as *const u8, MB2_RAW_INFO.as_mut_ptr(), bi_size);
    }

    let boot_info =
        unsafe { BootInformation::load(MB2_RAW_INFO.as_mut_ptr() as *const BootInformationHeader) }
            .inspect_err(|_| loop {
                spin_loop();
            })
            .unwrap();

    MB2_INFO.init(boot_info);

    register_boot_callbacks(&Mb2Callback);

    return Ok(());
}
