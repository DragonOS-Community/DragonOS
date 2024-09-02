use crate::{
    driver::base::{kobject::KObject, kset::KSet},
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

/// `/sys/kernel`的kset
static mut KERNEL_KSET_INSTANCE: Option<Arc<KSet>> = None;

#[inline(always)]
#[allow(dead_code)]
pub fn sys_kernel_kset() -> Arc<KSet> {
    unsafe { KERNEL_KSET_INSTANCE.clone().unwrap() }
}

#[unified_init(INITCALL_CORE)]
fn ksysfs_init() -> Result<(), SystemError> {
    let kernel_kset = KSet::new("kernel".to_string());
    kernel_kset
        .register(None)
        .expect("register kernel kset failed");

    sysfs_instance()
        .create_groups(&kernel_kset.as_kobject(), &[&KernelAttrGroup])
        .map_err(|e| {
            error!("Failed to create sysfs groups for kernel kset: {:?}", e);
            kernel_kset.unregister();
            SystemError::ENOMEM
        })?;

    unsafe {
        KERNEL_KSET_INSTANCE = Some(kernel_kset);
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
