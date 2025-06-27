use alloc::{string::ToString, sync::Arc};
use log::info;

use crate::driver::base::{
    device::{
        set_sys_dev_block_kobj, set_sys_dev_char_kobj, set_sys_devices_virtual_kobj, sys_dev_kobj,
        sys_devices_kset, DeviceManager, DEVICES_KSET_INSTANCE, DEVICE_MANAGER,
        DEV_KOBJECT_INSTANCE,
    },
    kobject::{CommonKobj, DynamicKObjKType, KObject, KObjectManager},
    kset::KSet,
};

use system_error::SystemError;

pub fn devices_init() -> Result<(), SystemError> {
    // 创建 `/sys/devices` 目录
    {
        let devices_kset = KSet::new("devices".to_string());
        devices_kset
            .register()
            .expect("register devices kset failed");

        unsafe {
            DEVICES_KSET_INSTANCE = Some(devices_kset);
            // 初始化全局设备管理器
            DEVICE_MANAGER = Some(DeviceManager::new());
        }
    }

    // 创建 `/sys/devices/virtual` 目录
    {
        let devices_kset = sys_devices_kset();
        let virtual_kobj = CommonKobj::new("virtual".to_string());
        let parent = devices_kset.clone() as Arc<dyn KObject>;
        virtual_kobj.set_parent(Some(Arc::downgrade(&parent)));
        KObjectManager::init_and_add_kobj(virtual_kobj.clone(), Some(&DynamicKObjKType))?;

        unsafe { set_sys_devices_virtual_kobj(virtual_kobj) };
    }

    // 创建 `/sys/dev` 目录
    {
        let dev_kobj = CommonKobj::new("dev".to_string());
        KObjectManager::init_and_add_kobj(dev_kobj.clone(), Some(&DynamicKObjKType))?;
        unsafe {
            DEV_KOBJECT_INSTANCE = Some(dev_kobj);
        }
    }

    // 创建 `/sys/dev/block` 目录
    {
        let dev_kobj = sys_dev_kobj();
        let dev_block_kobj = CommonKobj::new("block".to_string());
        let parent = dev_kobj.clone() as Arc<dyn KObject>;
        dev_block_kobj.set_parent(Some(Arc::downgrade(&parent)));
        KObjectManager::init_and_add_kobj(dev_block_kobj.clone(), Some(&DynamicKObjKType))?;

        unsafe { set_sys_dev_block_kobj(dev_block_kobj) };
    }

    // 创建 `/sys/dev/char` 目录
    {
        let dev_kobj = sys_dev_kobj();
        let dev_char_kobj = CommonKobj::new("char".to_string());
        let parent = dev_kobj.clone() as Arc<dyn KObject>;
        dev_char_kobj.set_parent(Some(Arc::downgrade(&parent)));
        KObjectManager::init_and_add_kobj(dev_char_kobj.clone(), Some(&DynamicKObjKType))?;

        unsafe { set_sys_dev_char_kobj(dev_char_kobj) };
    }

    info!("devices init success");

    return Ok(());
}
