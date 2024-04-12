use alloc::sync::Arc;

use super::pci_device::PciDevice;
const PCI_ANY_ID:u32=0xffff_ffff;
#[derive(Debug)]
pub struct PciDeviceID{
    vendor:u32,
    device_id:u32,
    subvendor:u32,
    subdevice:u32,
    class:u32,
    class_mask:u32,
    _driver_data:u64,
    _override_only:u32
}

impl PciDeviceID{
    pub fn dummpy()->Self{
        return Self { vendor: PCI_ANY_ID, device_id: PCI_ANY_ID, subvendor:PCI_ANY_ID, subdevice: PCI_ANY_ID, class: PCI_ANY_ID, class_mask: PCI_ANY_ID, _driver_data: 0, _override_only: PCI_ANY_ID }
    }
    pub fn match_dev(&self,dev:&Arc<dyn PciDevice>)->bool{
        let d_id=dev.dynid();
        if (self.vendor==d_id.vendor()||self.vendor==PCI_ANY_ID)&&
        (self.device_id==d_id.device_id()||self.device_id==PCI_ANY_ID)&&
        (self.subvendor==d_id.subvendor()||self.subvendor==PCI_ANY_ID)&&
        (self.subdevice==d_id.subdevice()||self.subdevice==PCI_ANY_ID)&&
        self.class_check(&d_id){
            return true
        }
        return false
    }

    pub fn class_check(&self,id:&Self)->bool{
        if (self.class ^ id.class())&self.class_mask==0{
            return true
        }else{
            return false
        }
    }

    pub fn vendor(&self)->u32{
        self.vendor
    }

    pub fn device_id(&self)->u32{
        self.device_id
    }

    pub fn subvendor(&self)->u32{
        self.subvendor
    }

    pub fn subdevice(&self)->u32{
        self.subdevice
    }

    pub fn class(&self)->u32{
        self.class
    }

    pub fn class_mask(&self)->u32{
        self.class_mask
    }
}

