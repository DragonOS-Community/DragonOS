use system_error::SystemError;

use crate::{
    driver::open_firmware::fdt::OpenFirmwareFdtDriver,
    init::boot_params,
    libs::align::page_align_up,
    mm::{mmio_buddy::mmio_pool, MemoryManagementArch, PhysAddr},
};

impl OpenFirmwareFdtDriver {
    #[allow(dead_code)]
    pub unsafe fn map_fdt(&self) -> Result<(), SystemError> {
        let bp_guard = boot_params().read();
        let fdt_size = bp_guard.arch.fdt_size;
        let fdt_paddr = bp_guard.arch.fdt_paddr;

        let offset = fdt_paddr.data() & crate::arch::MMArch::PAGE_OFFSET_MASK;
        let map_size = page_align_up(fdt_size + offset);
        let map_paddr = PhysAddr::new(fdt_paddr.data() & crate::arch::MMArch::PAGE_MASK);
        // debug!(
        //     "map_fdt paddr: {:?}, map_pa: {:?},fdt_size: {},  size: {:?}",
        //     fdt_paddr,
        //     map_paddr,
        //     fdt_size,
        //     map_size
        // );
        let mmio_guard = mmio_pool().create_mmio(map_size)?;

        // drop the boot params guard in order to avoid deadlock
        drop(bp_guard);
        // debug!("map_fdt: map fdt to {:?}, size: {}", map_paddr, map_size);
        mmio_guard.map_phys(map_paddr, map_size)?;
        let mut bp_guard = boot_params().write();
        let vaddr = mmio_guard.vaddr() + offset;

        self.set_fdt_map_guard(Some(mmio_guard));
        bp_guard.arch.fdt_vaddr.replace(vaddr);

        return Ok(());
    }
}
