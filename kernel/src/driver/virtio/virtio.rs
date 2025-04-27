use super::mmio::virtio_probe_mmio;
use super::transport_pci::PciTransport;
use super::virtio_impl::HalImpl;
use crate::driver::base::device::bus::Bus;
use crate::driver::base::device::{Device, DeviceId};
use crate::driver::block::virtio_blk::virtio_blk;
use crate::driver::char::virtio_console::virtio_console;
use crate::driver::net::virtio_net::virtio_net;
use crate::driver::pci::pci::{
    get_pci_device_structures_mut_by_vendor_id, PciDeviceStructureGeneralDevice,
    PCI_DEVICE_LINKEDLIST,
};
use crate::driver::pci::subsys::pci_bus;
use crate::driver::virtio::transport::VirtIOTransport;
use crate::init::initcall::INITCALL_DEVICE;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use log::{debug, error, warn};
use system_error::SystemError;
use unified_init::macros::unified_init;
use virtio_drivers::transport::{DeviceType, Transport};

///@brief 寻找并加载所有virtio设备的驱动（目前只有virtio-net，但其他virtio设备也可添加）
#[unified_init(INITCALL_DEVICE)]
fn virtio_probe() -> Result<(), SystemError> {
    #[cfg(not(target_arch = "riscv64"))]
    virtio_probe_pci();
    virtio_probe_mmio();

    Ok(())
}

#[allow(dead_code)]
fn virtio_probe_pci() {
    let virtio_list = virtio_device_search();
    for virtio_device in virtio_list {
        let dev_id = virtio_device.common_header.device_id;
        let dev_id = DeviceId::new(None, Some(format!("{dev_id}"))).unwrap();
        match PciTransport::new::<HalImpl>(virtio_device.clone(), dev_id.clone()) {
            Ok(mut transport) => {
                debug!(
                    "Detected virtio PCI device with device type {:?}, features {:#018x}",
                    transport.device_type(),
                    transport.read_device_features(),
                );
                let transport = VirtIOTransport::Pci(transport);
                // 这里暂时通过设备名称在sysfs中查找设备，但是我感觉用设备ID更好
                let bus = pci_bus() as Arc<dyn Bus>;
                let name: String = virtio_device.common_header.bus_device_function.into();
                let pci_raw_device = bus.find_device_by_name(name.as_str());
                virtio_device_init(transport, dev_id, pci_raw_device);
            }
            Err(err) => {
                error!("Pci transport create failed because of error: {}", err);
            }
        }
    }
}

///@brief 为virtio设备寻找对应的驱动进行初始化
pub(super) fn virtio_device_init(
    transport: VirtIOTransport,
    dev_id: Arc<DeviceId>,
    dev_parent: Option<Arc<dyn Device>>,
) {
    match transport.device_type() {
        DeviceType::Block => virtio_blk(transport, dev_id, dev_parent),
        DeviceType::GPU => {
            warn!("Not support virtio_gpu device for now");
        }
        DeviceType::Console => virtio_console(transport, dev_id, dev_parent),
        DeviceType::Input => {
            warn!("Not support virtio_input device for now");
        }
        DeviceType::Network => virtio_net(transport, dev_id, dev_parent),
        t => {
            warn!("Unrecognized virtio device: {:?}", t);
        }
    }
}

/// # virtio_device_search - 在给定的PCI设备列表中搜索符合特定标准的virtio设备
///
/// 该函数搜索一个PCI设备列表，找到所有由特定厂商ID（0x1AF4）和设备ID范围（0x1000至0x103F）定义的virtio设备。
///
/// ## 参数
///
/// - list: &'a mut RwLockWriteGuard<'_, LinkedList<Box<dyn PciDeviceStructure>>> - 一个可写的PCI设备结构列表的互斥锁。
///
/// ## 返回值
///
/// 返回一个包含所有找到的virtio设备的数组
fn virtio_device_search() -> Vec<Arc<PciDeviceStructureGeneralDevice>> {
    let list = &*PCI_DEVICE_LINKEDLIST;
    let mut virtio_list = Vec::new();
    let result = get_pci_device_structures_mut_by_vendor_id(list, 0x1AF4);

    for device in result {
        let standard_device = device.as_standard_device().unwrap();
        let header = &standard_device.common_header;
        if header.device_id >= 0x1000 && header.device_id <= 0x103F {
            virtio_list.push(standard_device);
        }
    }
    return virtio_list;
}
