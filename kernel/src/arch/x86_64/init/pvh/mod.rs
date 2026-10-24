//! x86/HVM启动
//!
//! 初始化代码可参考：https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/platform/pvh/enlighten.c#45
use alloc::string::{String, ToString};
use core::{ffi::CStr, hint::spin_loop};
use param::{E820Type, HvmMemmapTableEntry, HvmModlistEntry, HvmStartInfo};
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
    mm::{
        early_ioremap::EarlyIoRemap,
        memblock::{mem_block_manager, INITIAL_MEMORY_REGIONS_NUM},
        MemoryManagementArch, PhysAddr,
    },
};

mod param;

use super::rsdp::{cache_bios_rsdp_result, cached_bios_rsdp};

static START_INFO: Lazy<HvmStartInfo> = Lazy::new();

struct PvhBootCallback;

/// PVH module descriptors are trusted hypervisor boot metadata, like the E820
/// table and RSDP supplied by the same handoff. RESERVED does not imply that a
/// descriptor is unreadable. A temporary mapping avoids assuming it lies in
/// the bootstrap direct map; it does not validate arbitrary device addresses.
fn read_module_entry(physical: usize) -> Result<HvmModlistEntry, SystemError> {
    let mapped = EarlyIoRemap::map_not_aligned(
        PhysAddr::new(physical),
        core::mem::size_of::<HvmModlistEntry>(),
        true,
    )?;
    // SAFETY: the trusted PVH handoff supplies readable descriptor bytes;
    // the complete fixed-size entry remains mapped until the matching unmap.
    // The protocol does not require the list pointer to be Rust-aligned.
    let entry = unsafe { core::ptr::read_unaligned(mapped.data() as *const HvmModlistEntry) };
    EarlyIoRemap::unmap(mapped)?;
    Ok(entry)
}

fn reserve_boot_modules(start_info: &HvmStartInfo) -> Result<(), SystemError> {
    let count = usize::try_from(start_info.nr_modules).map_err(|_| SystemError::EINVAL)?;
    if count == 0 {
        return Ok(());
    }
    // Keep early work and memblock fragmentation within the existing boot
    // memory-region support bound, with no temporary descriptor array.
    if count > INITIAL_MEMORY_REGIONS_NUM || start_info.modlist_paddr == 0 {
        return Err(SystemError::EINVAL);
    }
    let base = usize::try_from(start_info.modlist_paddr).map_err(|_| SystemError::EINVAL)?;
    let bytes = count
        .checked_mul(core::mem::size_of::<HvmModlistEntry>())
        .ok_or(SystemError::EINVAL)?;
    base.checked_add(bytes).ok_or(SystemError::EINVAL)?;
    for index in 0..count {
        let physical = base
            .checked_add(
                index
                    .checked_mul(core::mem::size_of::<HvmModlistEntry>())
                    .ok_or(SystemError::EINVAL)?,
            )
            .ok_or(SystemError::EINVAL)?;
        let entry = read_module_entry(physical)?;
        let start = usize::try_from(entry.paddr).map_err(|_| SystemError::EINVAL)?;
        let size = usize::try_from(entry.size).map_err(|_| SystemError::EINVAL)?;
        let end = start.checked_add(size).ok_or(SystemError::EINVAL)?;
        // PVH address zero denotes absence. No payload is loaded or read here.
        if start == 0 || size == 0 {
            continue;
        }
        let mask = MMArch::PAGE_SIZE - 1;
        let first = start & !mask;
        let limit = end.checked_add(mask).ok_or(SystemError::EINVAL)? & !mask;
        // Like Linux's early_reserve_initrd, reserve the supplied physical
        // extent without requiring E820 RAM. Existing flags are preserved;
        // reserve_block never adds a hole or device range to usable memory.
        // The temporary descriptor mapping and its slot lock are already gone.
        mem_block_manager().reserve_block(PhysAddr::new(first), limit - first)?;
    }
    Ok(())
}

impl BootCallbacks for PvhBootCallback {
    fn init_bootloader_name(&self) -> Result<Option<String>, SystemError> {
        return Ok(Some("x86 PVH".to_string()));
    }

    fn init_acpi_args(&self) -> Result<BootloaderAcpiArg, SystemError> {
        let si = START_INFO.get();
        log::info!(
            "pvh: HvmStartInfo: magic={:#010x} version={} flags={:#x} nr_modules={} rsdp_paddr={:#x} memmap_paddr={:#x} memmap_entries={}",
            si.magic, si.version, si.flags, si.nr_modules,
            si.rsdp_paddr, si.memmap_paddr, si.memmap_entries
        );

        let rsdp_paddr = PhysAddr::new(si.rsdp_paddr as usize);

        // if RSDP has been provided
        if rsdp_paddr.data() != 0 {
            return Ok(BootloaderAcpiArg::Rsdp(rsdp_paddr));
        }

        // The bootloader did not provide an RSDP. Consume the shared x86
        // early-discovery result without touching EarlyIoRemap after mm_init.
        if let Some(paddr) = cached_bios_rsdp()? {
            log::info!("pvh: found RSDP at {:#x}", paddr.data());
            return Ok(BootloaderAcpiArg::Rsdp(paddr));
        }

        log::warn!("pvh: RSDP not found");
        Ok(BootloaderAcpiArg::NotProvided)
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

    fn init_initramfs(&self) -> Result<(), SystemError> {
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
                    E820Type::Ram => {
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
        reserve_boot_modules(start_info)?;
        send_to_default_serial8250_port("init_memory_area_from pvh boot end\n\0".as_bytes());
        log::info!(
            "Total memory size: {:#x}, Usable memory size: {:#x}",
            total_mem_size,
            usable_mem_size
        );

        // Discover the RSDP here, in the early boot stage: the bootstrap page
        // table is still active and mm_init has not run yet, so EarlyIoRemap may
        // be used safely (this mirrors Linux, which scans the RSDP in setup_arch
        // via early_memremap). If the bootloader already provided rsdp_paddr we
        // skip the scan. The result is cached for init_acpi_args.
        if start_info.rsdp_paddr == 0 {
            cache_bios_rsdp_result();
        }

        Ok(())
    }

    fn init_memmap_sysfs(&self) -> Result<(), SystemError> {
        crate::mm::sysfs::early_memmap_init();

        let start_info = START_INFO.get();
        if (start_info.version > 0) && start_info.memmap_entries > 0 {
            let mut ep = unsafe {
                MMArch::phys_2_virt(PhysAddr::new(start_info.memmap_paddr as usize)).unwrap()
            }
            .data() as *const HvmMemmapTableEntry;

            for i in 0..start_info.memmap_entries {
                let entry = unsafe { *ep };

                let t = match E820Type::from(entry.type_) {
                    E820Type::Ram => 1,
                    E820Type::Reserved => 2,
                    E820Type::Acpi => 3,
                    _ => 4,
                };

                let memmapd = crate::mm::sysfs::MemmapDesc::new(
                    i.to_string(),
                    entry.addr as usize,
                    (entry.addr + entry.size) as usize,
                    t,
                );
                crate::mm::sysfs::memmap_desc_manager().insert(i as usize, memmapd);

                ep = unsafe { ep.add(1) };
            }
        }

        Ok(())
    }

    fn init_memmap_bp(&self) -> Result<(), SystemError> {
        let start_info = START_INFO.get();
        if (start_info.version > 0) && start_info.memmap_entries > 0 {
            let mut ep = unsafe {
                MMArch::phys_2_virt(PhysAddr::new(start_info.memmap_paddr as usize)).unwrap()
            }
            .data() as *const HvmMemmapTableEntry;
            let total_entries = start_info.memmap_entries as usize;

            for (imported, _) in (0..start_info.memmap_entries).enumerate() {
                let entry = unsafe { *ep };
                let e820_type = E820Type::from(entry.type_);

                let t = match e820_type {
                    E820Type::Ram => 1,
                    E820Type::Reserved => 2,
                    E820Type::Acpi => 3,
                    E820Type::Nvs => 4,
                    E820Type::Unusable => 5,
                    E820Type::Pmem => 7,
                    E820Type::Pram => 12,
                    E820Type::SoftReserved => 0xefffffff,
                    E820Type::ReservedKern => 128,
                };

                let mut bp = boot_params().write_irqsave();
                if !bp.arch.add_e820_entry(entry.addr, entry.size, t) {
                    log::warn!(
                        "PVH E820 map truncated: imported {} of {} entries (capacity={})",
                        imported,
                        total_entries,
                        bp.arch.e820_table.len()
                    );
                    break;
                }
                drop(bp);

                ep = unsafe { ep.add(1) };
            }
        }
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
