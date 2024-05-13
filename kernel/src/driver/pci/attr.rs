use alloc::sync::Arc;
use intertrait::cast::CastArc;
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

use super::device::PciDevice;
#[derive(Debug)]
pub struct BasicPciReadOnlyAttrs;

impl AttributeGroup for BasicPciReadOnlyAttrs {
    fn name(&self) -> Option<&str> {
        None
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        &[&Vendor, &DeviceID, &SubsystemVendor, &SubsystemDevice]
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
pub struct Vendor;

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
                kwarn!("device:{:?} is not a pci device!", e);
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
pub struct DeviceID;

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
                kwarn!("device:{:?} is not a pci device!", e);
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
pub struct SubsystemVendor;

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
                kwarn!("device:{:?} is not a pci device!", e);
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
pub struct SubsystemDevice;

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
                kwarn!("device:{:?} is not a pci device!", e);
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
