use crate::{
    driver::base::kobject::{CommonKobj, DynamicKObjKType, KObject, KObjectManager},
    filesystem::{
        sysfs::{sysfs_instance, Attribute, AttributeGroup},
        vfs::syscall::ModeType,
    },
    init::initcall::INITCALL_CORE,
};
use alloc::{string::ToString, sync::Arc};
use log::error;
use system_error::SystemError;
use unified_init::macros::unified_init;

/// `/sys/kernel`的kobject
static mut KERNEL_KOBJECT_INSTANCE: Option<Arc<CommonKobj>> = None;

#[inline(always)]
#[allow(dead_code)]
pub fn sys_kernel_kobj() -> Arc<CommonKobj> {
    unsafe { KERNEL_KOBJECT_INSTANCE.clone().unwrap() }
}

#[unified_init(INITCALL_CORE)]
fn ksysfs_init() -> Result<(), SystemError> {
    // let kernel_kset = KSet::new("kernel".to_string());
    // kernel_kset.register().expect("register kernel kset failed");
    let kernel_kobj = CommonKobj::new("kernel".to_string());
    KObjectManager::init_and_add_kobj(kernel_kobj.clone(), Some(&DynamicKObjKType))?;

    sysfs_instance()
        .create_groups(
            &(kernel_kobj.clone() as Arc<dyn KObject>),
            &[&KernelAttrGroup],
        )
        .map_err(|e| {
            error!("Failed to create sysfs groups for kernel kset: {:?}", e);
            KObjectManager::remove_kobj(kernel_kobj.clone());
            SystemError::ENOMEM
        })?;

    unsafe {
        KERNEL_KOBJECT_INSTANCE = Some(kernel_kobj);
    }

    return Ok(());
}

#[derive(Debug)]
struct KernelAttrGroup;

impl AttributeGroup for KernelAttrGroup {
    fn name(&self) -> Option<&str> {
        None
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        &[]
    }

    fn is_visible(
        &self,
        _kobj: Arc<dyn KObject>,
        attr: &'static dyn Attribute,
    ) -> Option<ModeType> {
        Some(attr.mode())
    }
}
