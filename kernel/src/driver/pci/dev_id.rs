use alloc::sync::Arc;

use super::device::PciDevice;
const PCI_ANY_ID: u32 = 0xffff_ffff;

/// # 结构功能
/// 该结构用于驱动和设备之间的识别，驱动会有一个支持的设备ID列表，而设备会自带一个ID，如果设备的ID在驱动的支持列表中，则驱动和设备就可以识别了
/// 见https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/mod_devicetable.h#43
#[derive(Debug, Copy, Clone)]
pub struct PciDeviceID {
    vendor: u32,
    device_id: u32,
    subvendor: u32,
    subdevice: u32,
    class: u32,
    class_mask: u32,
    _driver_data: u64,
    _override_only: u32,
    /// 可能有些设备的识别方式比较特殊，那么可以通过设置该字段进行自定义的识别方式，只需要在PciSpecifiedData枚举中加入一个类型即可
    /// 若该字段不为None，则优先使用special_data进行识别；
    /// 该字段是为了增加灵活性
    special_data: Option<PciSpecifiedData>,
}

impl PciDeviceID {
    #[allow(dead_code)]
    pub fn set_special(&mut self, data: PciSpecifiedData) {
        self.special_data = Some(data);
    }

    pub fn dummpy() -> Self {
        return Self {
            vendor: PCI_ANY_ID,
            device_id: PCI_ANY_ID,
            subvendor: PCI_ANY_ID,
            subdevice: PCI_ANY_ID,
            class: PCI_ANY_ID,
            class_mask: PCI_ANY_ID,
            _driver_data: 0,
            _override_only: PCI_ANY_ID,
            special_data: None,
        };
    }
    pub fn match_dev(&self, dev: &Arc<dyn PciDevice>) -> bool {
        if let Some(d_data) = &dev.dynid().special_data {
            return d_data.match_dev(self.special_data);
        }
        if let Some(s_data) = &self.special_data {
            return s_data.match_dev(dev.dynid().special_data);
        } else {
            let d_id = dev.dynid();
            return self.general_match(d_id);
        }
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/pci/pci.h?fi=pci_match_one_device#195
    pub fn general_match(&self, id: PciDeviceID) -> bool {
        if (self.vendor == id.vendor() || self.vendor == PCI_ANY_ID)
            && (self.device_id == id.device_id() || self.device_id == PCI_ANY_ID)
            && (self.subvendor == id.subvendor() || self.subvendor == PCI_ANY_ID)
            && (self.subdevice == id.subdevice() || self.subdevice == PCI_ANY_ID)
            && self.class_check(&id)
        {
            return true;
        }
        return false;
    }

    pub fn class_check(&self, id: &Self) -> bool {
        return (self.class ^ id.class()) & self.class_mask == 0;
    }

    pub fn vendor(&self) -> u32 {
        self.vendor
    }

    pub fn device_id(&self) -> u32 {
        self.device_id
    }

    pub fn subvendor(&self) -> u32 {
        self.subvendor
    }

    pub fn subdevice(&self) -> u32 {
        self.subdevice
    }

    pub fn class(&self) -> u32 {
        self.class
    }

    pub fn _class_mask(&self) -> u32 {
        self.class_mask
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug, Copy, Clone)]
pub enum PciSpecifiedData {}

impl PciSpecifiedData {
    pub fn match_dev(&self, data: Option<Self>) -> bool {
        if let Some(data) = data {
            return *self == data;
        } else {
            return false;
        }
    }
}
