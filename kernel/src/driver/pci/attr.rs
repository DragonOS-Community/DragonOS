use alloc::sync::Arc;
use intertrait::cast::CastArc;
use log::warn;
use system_error::SystemError;

use crate::{
    driver::base::kobject::KObject,
    filesystem::{
        sysfs::{
            file::sysfs_emit_str, Attribute, AttributeGroup, SysFSOpsSupport, SYSFS_ATTR_MODE_RO,
        },
        vfs::syscall::ModeType,
    },
};

use super::{device::PciDevice, pci_irq::IrqType};
#[derive(Debug)]
pub struct BasicPciReadOnlyAttrs;

impl AttributeGroup for BasicPciReadOnlyAttrs {
    fn name(&self) -> Option<&str> {
        None
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        &[
            &Vendor,
            &DeviceID,
            &SubsystemVendor,
            &SubsystemDevice,
            &Revision,
            &Class,
            &Irq,
            &Modalias,
        ]
    }

    fn is_visible(
        &self,
        _kobj: Arc<dyn KObject>,
        attr: &'static dyn Attribute,
    ) -> Option<ModeType> {
        return Some(attr.mode());
    }
}

#[derive(Debug)]
struct Vendor;

impl Attribute for Vendor {
    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn name(&self) -> &str {
        "vendor"
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        let dev = _kobj
            .cast::<dyn PciDevice>()
            .map_err(|e: Arc<dyn KObject>| {
                warn!("device:{:?} is not a pci device!", e);
                SystemError::EINVAL
            })?;
        return sysfs_emit_str(_buf, &format!("0x{:04x}", dev.vendor()));
    }

    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!()
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }
}

#[derive(Debug)]
struct DeviceID;

impl Attribute for DeviceID {
    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn name(&self) -> &str {
        "device"
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        let dev = _kobj
            .cast::<dyn PciDevice>()
            .map_err(|e: Arc<dyn KObject>| {
                warn!("device:{:?} is not a pci device!", e);
                SystemError::EINVAL
            })?;
        return sysfs_emit_str(_buf, &format!("0x{:04x}", dev.device_id()));
    }

    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!()
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }
}

#[derive(Debug)]
struct SubsystemVendor;

impl Attribute for SubsystemVendor {
    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn name(&self) -> &str {
        "subsystem_vendor"
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        let dev = _kobj
            .cast::<dyn PciDevice>()
            .map_err(|e: Arc<dyn KObject>| {
                warn!("device:{:?} is not a pci device!", e);
                SystemError::EINVAL
            })?;
        return sysfs_emit_str(_buf, &format!("0x{:04x}", dev.subsystem_vendor()));
    }

    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!()
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }
}

#[derive(Debug)]
struct SubsystemDevice;

impl Attribute for SubsystemDevice {
    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn name(&self) -> &str {
        "subsystem_device"
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        let dev = _kobj
            .cast::<dyn PciDevice>()
            .map_err(|e: Arc<dyn KObject>| {
                warn!("device:{:?} is not a pci device!", e);
                SystemError::EINVAL
            })?;
        return sysfs_emit_str(_buf, &format!("0x{:04x}", dev.subsystem_device()));
    }

    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!()
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }
}

#[derive(Debug)]
struct Revision;

impl Attribute for Revision {
    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn name(&self) -> &str {
        "revision"
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        let dev = _kobj
            .cast::<dyn PciDevice>()
            .map_err(|e: Arc<dyn KObject>| {
                warn!("device:{:?} is not a pci device!", e);
                SystemError::EINVAL
            })?;
        return sysfs_emit_str(_buf, &format!("0x{:02x}", dev.revision()));
    }

    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!()
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }
}

#[derive(Debug)]
struct Class;

impl Attribute for Class {
    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn name(&self) -> &str {
        "class"
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        let dev = _kobj
            .cast::<dyn PciDevice>()
            .map_err(|e: Arc<dyn KObject>| {
                warn!("device:{:?} is not a pci device!", e);
                SystemError::EINVAL
            })?;
        return sysfs_emit_str(_buf, &format!("0x{:06x}", dev.class_code()));
    }

    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!()
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }
}

#[derive(Debug)]
struct Irq;

impl Attribute for Irq {
    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn name(&self) -> &str {
        "irq"
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        let dev = _kobj
            .cast::<dyn PciDevice>()
            .map_err(|e: Arc<dyn KObject>| {
                warn!("device:{:?} is not a pci device!", e);
                SystemError::EINVAL
            })?;
        if let IrqType::Msi { .. } = *dev.irq_type().read() {
            // 见https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/pci/pci-sysfs.c#55
            return sysfs_emit_str(_buf, "todo:sry,msi device is unimplemented now");
        }
        return sysfs_emit_str(_buf, &format!("{}", dev.irq_line()));
    }

    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!()
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }
}

#[derive(Debug)]
struct Modalias;

impl Attribute for Modalias {
    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn name(&self) -> &str {
        "modalias"
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        let dev = _kobj
            .cast::<dyn PciDevice>()
            .map_err(|e: Arc<dyn KObject>| {
                warn!("device:{:?} is not a pci device!", e);
                SystemError::EINVAL
            })?;
        return sysfs_emit_str(
            _buf,
            &format!(
                "pci:v{:08X}d{:08X}sv{:08X}sd{:08X}bc{:02X}sc{:02X}i{:02X}",
                dev.vendor(),
                dev.device_id(),
                dev.subsystem_vendor(),
                dev.subsystem_device(),
                dev.class_code(),
                dev.subclass(),
                dev.interface_code(),
            ),
        );
    }

    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!()
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }
}
