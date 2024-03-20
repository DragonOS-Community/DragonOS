use core::{any::Any, fmt::Debug};

use alloc::sync::Arc;

use crate::{
    driver::{base::device::Device, pci::pci_irq::PciIrqMsg},
    filesystem::sysfs::Attribute,
    libs::spinlock::SpinLock,
};

use super::IrqNumber;

#[derive(Clone, Copy)]
pub struct MsiMsg {
    pub address_lo: u32,
    pub address_hi: u32,
    pub data: u32,
}

impl Debug for MsiMsg {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "MsiMsg {{ address_lo: 0x{:x}, address_hi: 0x{:x}, data: 0x{:x} }}",
            self.address_lo, self.address_hi, self.data
        )
    }
}

#[allow(dead_code)]
impl MsiMsg {
    /// Create a new MSI message
    pub const fn new(address: u64, data: u32) -> Self {
        MsiMsg {
            address_lo: address as u32,
            address_hi: (address >> 32) as u32,
            data,
        }
    }

    /// Create a new MSI message
    pub const fn new_lo_hi(address_lo: u32, address_hi: u32, data: u32) -> Self {
        MsiMsg {
            address_lo,
            address_hi,
            data,
        }
    }

    /// Get the address of the MSI message
    pub const fn address(&self) -> u64 {
        (self.address_hi as u64) << 32 | self.address_lo as u64
    }

    pub const fn new_zeroed() -> Self {
        MsiMsg {
            address_lo: 0,
            address_hi: 0,
            data: 0,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct MsiDesc {
    inner: SpinLock<InnerMsiDesc>,
}

#[allow(dead_code)]
#[derive(Debug)]
struct InnerMsiDesc {
    /// The base interrupt number
    irq: IrqNumber,
    /// The number of vectors used
    nvec_used: u32,
    /// Pointer to the device which uses this descriptor
    dev: Option<Arc<dyn Device>>,
    /// The last set MSI message cached for reuse
    msg: MsiMsg,
    /// Pointer to sysfs device attribute
    sysfs_attribute: Option<&'static dyn Attribute>,
    /// Pointer to MSI callback function
    func: Option<&'static dyn MsiDescFunc>,
    /// The index of the MSI descriptor
    index: u32,
    /// PCI specific msi descriptor data
    pci_msg: PciIrqMsg,
}

#[allow(dead_code)]
impl MsiDesc {
    pub const fn new(
        irq: IrqNumber,
        nvec_used: u32,
        dev: Option<Arc<dyn Device>>,
        index: u32,
        pci_msg: PciIrqMsg,
    ) -> Self {
        MsiDesc {
            inner: SpinLock::new(InnerMsiDesc {
                irq,
                nvec_used,
                dev,
                msg: MsiMsg {
                    address_lo: 0,
                    address_hi: 0,
                    data: 0,
                },
                sysfs_attribute: None,
                func: None,
                index,
                pci_msg,
            }),
        }
    }

    pub fn set_msg(&self, msg: MsiMsg) {
        self.inner.lock().msg = msg;
    }

    pub fn msg(&self) -> MsiMsg {
        self.inner.lock().msg
    }
}

pub trait MsiDescFunc: Debug + Send + Sync {
    /// Callback that may be called when the MSI message
    /// address or data changes.
    fn write_msi_msg(&self, data: Arc<dyn MsiDescFuncData>);
}

/// Data parameter for the `MsiDescFunc` callback.
pub trait MsiDescFuncData: Send + Sync + Any {}
