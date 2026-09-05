use alloc::{string::String, sync::Arc};

use system_error::SystemError;

use crate::{
    driver::base::kobject::{KObject, KObjectState},
    filesystem::{
        kernfs::{KernFSInode, KernFSRenameSpec, PreparedKernFSRename},
        sysfs::sysfs_instance,
    },
    libs::casting::DowncastArc,
};

use super::device::Device;

/// Driver-core projection of a device rename. `rename` is absent for devices
/// which were deliberately registered without sysfs.
pub(crate) struct PreparedDeviceSysfsRename {
    rename: Option<PreparedKernFSRename>,
    old_devpath: Option<String>,
}

impl PreparedDeviceSysfsRename {
    pub(crate) fn commit(self) -> Result<Option<String>, SystemError> {
        if let Some(rename) = self.rename {
            rename.commit()?;
        }
        Ok(self.old_devpath)
    }
}

/// Prepares the identity-preserving directory and class-link re-key for one
/// class device. The device's logical name is published by its owning control
/// transaction after this structural commit succeeds.
pub(crate) fn prepare_class_device_sysfs_rename(
    device: &Arc<dyn Device>,
    new_name: String,
) -> Result<PreparedDeviceSysfsRename, SystemError> {
    let registered = device.kobj_state().contains(KObjectState::IN_SYSFS);
    let device_inode = device.inode();
    if !registered {
        return if device_inode.is_none() {
            Ok(PreparedDeviceSysfsRename {
                rename: None,
                old_devpath: None,
            })
        } else {
            Err(SystemError::EIO)
        };
    }

    let device_inode = device_inode.ok_or(SystemError::EIO)?;
    let device_parent = device_inode.parent().ok_or(SystemError::EIO)?;
    let old_sysfs_name = device_inode.try_name()?;
    let class = device.class().ok_or(SystemError::EIO)?;
    let class_kobject = class.subsystem().subsys() as Arc<dyn KObject>;
    let class_parent = class_kobject.inode().ok_or(SystemError::EIO)?;
    let namespace = device_inode.namespace().ok_or(SystemError::EIO)?;
    let class_link = class_parent
        .find_ns(&old_sysfs_name, namespace)
        .map_err(|_| SystemError::EIO)?
        .downcast_arc::<KernFSInode>()
        .ok_or(SystemError::EIO)?;
    if !class_link
        .symlink_target()
        .is_some_and(|target| Arc::ptr_eq(&target, &device_inode))
    {
        return Err(SystemError::EIO);
    }

    let target_path = if class_parent.namespace_children_enabled() {
        sysfs_instance().child_relative_path(&class_parent, &device_parent, new_name.as_str())?
    } else {
        sysfs_instance().child_absolute_path(&device_parent, new_name.as_str())?
    };
    let old_devpath = sysfs_instance().try_kernfs_path(&device_inode)?;
    let class_name = try_copy_string(&new_name)?;
    let device_spec = KernFSRenameSpec::new(device_inode, new_name);
    let class_spec = KernFSRenameSpec::new(class_link, class_name)
        .with_symlink_target_absolute_path(target_path);
    Ok(PreparedDeviceSysfsRename {
        rename: Some(PreparedKernFSRename::prepare(device_spec, class_spec)?),
        old_devpath: Some(old_devpath),
    })
}

fn try_copy_string(source: &str) -> Result<String, SystemError> {
    let mut result = String::new();
    result
        .try_reserve_exact(source.len())
        .map_err(|_| SystemError::ENOMEM)?;
    result.push_str(source);
    Ok(result)
}
