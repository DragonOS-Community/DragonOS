pub mod dw_mshc;

use self::dw_mshc::mmc::MMC;
use crate::driver::base::block::block_device::BlockDevice;
use crate::driver::base::block::manager::block_dev_manager;
use crate::driver::open_firmware::fdt::open_firmware_fdt_driver;
use crate::init::initcall::INITCALL_DEVICE;
use alloc::sync::Arc;
use fdt::node::FdtNode;
use log::*;
use system_error::SystemError;
use unified_init::macros::unified_init;

#[unified_init(INITCALL_DEVICE)]
fn vf2_mmc_probe() -> Result<(), SystemError> {
    info!("Probing vf2 sdio(mmc)");

    let fdt = open_firmware_fdt_driver().fdt_ref()?;

    let do_check = |node: FdtNode| -> Result<(), SystemError> {
        let base_address = node.reg().unwrap().next().unwrap().starting_address as usize;
        let size = node.reg().unwrap().next().unwrap().size.unwrap();
        let irq_number = 33; // Hard-coded from JH7110
        let sdcard = MMC::new(base_address, size, irq_number);
        //debug!("MMC create done");
        sdcard.card_init();
        //debug!("MMC init done");
        block_dev_manager().register(sdcard as Arc<dyn BlockDevice>)?;
        //debug!("MMC register done");
        Ok(())
    };

    if let Some(node) = fdt.find_node("/soc/sdio1@16020000") {
        //debug!("Find node to vf2-sdio");
        do_check(node).ok();
    }

    info!("Probing vf2 sdio(mmc) done!");

    Ok(())
}