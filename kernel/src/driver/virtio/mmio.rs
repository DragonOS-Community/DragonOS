use fdt::node::FdtNode;
use log::error;
use system_error::SystemError;

use crate::driver::{
    open_firmware::fdt::open_firmware_fdt_driver, virtio::transport_mmio::VirtIOMmioTransport,
};

use super::{transport::VirtIOTransport, virtio::virtio_device_init};

pub(super) fn virtio_probe_mmio() {
    if let Err(e) = do_probe_virtio_mmio() {
        error!("virtio_probe_mmio failed: {:?}", e);
    }
}

fn do_probe_virtio_mmio() -> Result<(), SystemError> {
    let fdt = open_firmware_fdt_driver().fdt_ref()?;

    let do_check = |node: FdtNode| -> Result<(), SystemError> {
        let mmio_transport = VirtIOMmioTransport::new(node)?;
        let device_id = mmio_transport.device_id();
        virtio_device_init(VirtIOTransport::Mmio(mmio_transport), device_id, None);
        Ok(())
    };

    for node in open_firmware_fdt_driver().find_node_by_compatible(&fdt, "virtio,mmio") {
        do_check(node).ok();
    }
    Ok(())
}
