use super::transport_pci::PciTransport;
use super::virtio_impl::HalImpl;
use crate::driver::pci::pci::DeviceFunction;
use crate::include::bindings::bindings::get_virtio_net_device;
use crate::{kdebug, kerror, kwarn};
use alloc::{boxed::Box, collections::LinkedList};
use virtio_drivers::device::net::VirtIONet;
use virtio_drivers::transport::{DeviceType, Transport};

//Virtio设备寻找过程中出现的问题
enum VirtioError {
    VirtioNetNotFound,
}

///@brief 寻找并加载所有virtio设备的驱动（目前只有virtio-net，但其他virtio设备也可添加）（for c）
#[no_mangle]
pub extern "C" fn c_virtio_probe() {
    if let Ok(virtio_list) = virtio_device_search() {
        for device_function in virtio_list {
            match PciTransport::new::<HalImpl>(*device_function) {
                Ok(mut transport) => {
                    kdebug!(
                        "Detected virtio PCI device with device type {:?}, features {:#018x}",
                        transport.device_type(),
                        transport.read_device_features(),
                    );
                    virtio_device(transport);
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

///@brief 寻找并加载所有virtio设备的驱动（目前只有virtio-net，但其他virtio设备也可添加）
fn virtio_probe() {
    if let Ok(virtio_list) = virtio_device_search() {
        for device_function in virtio_list {
            match PciTransport::new::<HalImpl>(*device_function) {
                Ok(mut transport) => {
                    kdebug!(
                        "Detected virtio PCI device with device type {:?}, features {:#018x}",
                        transport.device_type(),
                        transport.read_device_features(),
                    );
                    virtio_device(transport);
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
fn virtio_device(transport: impl Transport) {
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
        DeviceType::Network => virtio_net(transport),
        t => {
            kwarn!("Unrecognized virtio device: {:?}", t);
        }
    }
}

///@brief virtio-net 驱动的初始化与测试
fn virtio_net<T: Transport>(transport: T) {
    let mut driver_net = match VirtIONet::<HalImpl, T>::new(transport) {
        Ok(mut net) => {
            kdebug!("Virtio-net driver init successfully.");
            net
        }
        Err(_) => {
            kerror!("VirtIONet init failed");
            return;
        }
    };
    // let mut buf = [0u8; 0x100];
    // let len = match driver_net.recv(&mut buf)
    // {
    //     Ok(len) =>{len},
    //     Err(_) =>{kerror!("virtio_net recv failed");return;}
    // };
    // kdebug!("recv: {:?}", &buf[..len]);
    // match driver_net.send(&buf[..len])
    // {
    //     Ok(_) =>{kdebug!("virtio_net send success");},
    //     Err(_) =>{kerror!("virtio_net send failed");return;},
    // }
    let mac = driver_net.mac();
    kdebug!("virtio_net MAC={:?}", mac);
    kdebug!("virtio-net test finished");
}

/// @brief 寻找所有的virtio设备
/// @return Result<LinkedList<Box<DeviceFunction>>,VirtioError> 成功则返回包含所有virtio设备的链表，失败则返回err
/// 该函数主要是为其他virtio设备预留支持
fn virtio_device_search() -> Result<LinkedList<Box<DeviceFunction>>, VirtioError> {
    let mut virtio_list: LinkedList<Box<DeviceFunction>> = LinkedList::new();
    let (bus, device, function) = unsafe {
        let mut bus: u8 = 0;
        let mut device: u8 = 0;
        let mut function: u8 = 0;
        let bus_ptr = &mut bus as *mut u8;
        let device_ptr = &mut device as *mut u8;
        let function_ptr = &mut function as *mut u8;
        get_virtio_net_device(bus_ptr, device_ptr, function_ptr);
        (bus, device, function)
    };
    if bus == 0 && device == 0 && function == 0 {
        kdebug!("get_virtio_net_device failed");
        return Err(VirtioError::VirtioNetNotFound);
    }
    let device_function = DeviceFunction {
        bus: bus,
        device: device,
        function: function,
    };
    virtio_list.push_back(Box::new(device_function));
    Ok(virtio_list)
}
