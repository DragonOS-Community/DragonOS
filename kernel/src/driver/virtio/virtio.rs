use super::transport_pci::PciTransport;
use super::virtio_impl::HalImpl;
use crate::driver::base::device::DeviceId;
use crate::driver::net::virtio_net::virtio_net;
use crate::driver::pci::pci::{
    get_pci_device_structure_mut, PciDeviceStructure, PciDeviceStructureGeneralDevice,
    PCI_DEVICE_LINKEDLIST,
};
use crate::libs::rwlock::RwLockWriteGuard;
use crate::{kdebug, kerror, kwarn};
use alloc::sync::Arc;
use alloc::{boxed::Box, collections::LinkedList};
use virtio_drivers::transport::{DeviceType, Transport};
const NETWORK_CLASS: u8 = 0x2;
const ETHERNET_SUBCLASS: u8 = 0x0;

//Virtio设备寻找过程中出现的问题
enum VirtioError {
    VirtioNetNotFound,
    NetDeviceNotFound,
}

///@brief 寻找并加载所有virtio设备的驱动（目前只有virtio-net，但其他virtio设备也可添加）
pub fn virtio_probe() {
    let mut list = PCI_DEVICE_LINKEDLIST.write();
    if let Ok(virtio_list) = virtio_device_search(&mut list) {
        for virtio_device in virtio_list {
            let dev_id = virtio_device.common_header.device_id;
            let dev_id = DeviceId::new(None, Some(format!("virtio_{}", dev_id))).unwrap();
            match PciTransport::new::<HalImpl>(virtio_device, dev_id.clone()) {
                Ok(mut transport) => {
                    kdebug!(
                        "Detected virtio PCI device with device type {:?}, features {:#018x}",
                        transport.device_type(),
                        transport.read_device_features(),
                    );
                    virtio_device_init(transport, dev_id);
                }
                Err(err) => {
                    kerror!("Pci transport create failed because of error: {}", err);
                }
            }
        }
    } else {
        kerror!("Error occured when finding virtio device!");
    }
}

///@brief 为virtio设备寻找对应的驱动进行初始化
fn virtio_device_init(transport: impl Transport + 'static, dev_id: Arc<DeviceId>) {
    match transport.device_type() {
        DeviceType::Block => {
            kwarn!("Not support virtio_block device for now");
        }
        DeviceType::GPU => {
            kwarn!("Not support virtio_gpu device for now");
        }
        DeviceType::Input => {
            kwarn!("Not support virtio_input device for now");
        }
        DeviceType::Network => virtio_net(transport, dev_id),
        t => {
            kwarn!("Unrecognized virtio device: {:?}", t);
        }
    }
}

/// @brief 寻找所有的virtio设备
/// @param list 链表的写锁
/// @return Result<LinkedList<&'a mut Pci_Device_Structure_General_Device>, VirtioError>  成功则返回包含所有virtio设备结构体的可变引用的链表，失败则返回err
/// 该函数主要是为其他virtio设备预留支持
fn virtio_device_search<'a>(
    list: &'a mut RwLockWriteGuard<'_, LinkedList<Box<dyn PciDeviceStructure>>>,
) -> Result<LinkedList<&'a mut PciDeviceStructureGeneralDevice>, VirtioError> {
    let mut virtio_list: LinkedList<&mut PciDeviceStructureGeneralDevice> = LinkedList::new();
    let virtio_net_device = get_virtio_net_device(list)?;
    virtio_list.push_back(virtio_net_device);
    Ok(virtio_list)
}

/// @brief 寻找virtio-net设备
/// @param list 链表的写锁
/// @return Result<&'a mut Pci_Device_Structure_General_Device, VirtioError> 成功则返回virtio设备结构体的可变引用，失败则返回err
fn get_virtio_net_device<'a>(
    list: &'a mut RwLockWriteGuard<'_, LinkedList<Box<dyn PciDeviceStructure>>>,
) -> Result<&'a mut PciDeviceStructureGeneralDevice, VirtioError> {
    let result = get_pci_device_structure_mut(list, NETWORK_CLASS, ETHERNET_SUBCLASS);
    if result.is_empty() {
        return Err(VirtioError::NetDeviceNotFound);
    }
    for device in result {
        let standard_device = device.as_standard_device_mut().unwrap();
        let header = &standard_device.common_header;
        if header.vendor_id == 0x1AF4
            && header.device_id >= 0x1000
            && header.device_id <= 0x103F
            && standard_device.subsystem_id == 1
        {
            return Ok(standard_device);
        }
    }
    Err(VirtioError::VirtioNetNotFound)
}
