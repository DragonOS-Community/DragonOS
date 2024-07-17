use alloc::{string::ToString, sync::Arc};
use log::info;

use crate::driver::base::{
    device::{
        set_sys_dev_block_kset, set_sys_dev_char_kset, set_sys_devices_virtual_kset, sys_dev_kset,
        sys_devices_kset, DeviceManager, DEVICES_KSET_INSTANCE, DEVICE_MANAGER, DEV_KSET_INSTANCE,
    },
    kobject::KObject,
    kset::KSet,
};

use system_error::SystemError;

pub fn devices_init() -> Result<(), SystemError> {
    // 创建 `/sys/devices` 目录
    {
        let devices_kset = KSet::new("devices".to_string());
        devices_kset
            .register(None)
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
        let virtual_kset = KSet::new("virtual".to_string());
        let parent = devices_kset.clone() as Arc<dyn KObject>;
        virtual_kset.set_parent(Some(Arc::downgrade(&parent)));

        virtual_kset
            .register(Some(devices_kset))
            .expect("register virtual kset failed");
        unsafe { set_sys_devices_virtual_kset(virtual_kset) };
    }

    // 创建 `/sys/dev` 目录
    {
        let dev_kset = KSet::new("dev".to_string());
        dev_kset.register(None).expect("register dev kset failed");
        unsafe {
            DEV_KSET_INSTANCE = Some(dev_kset);
        }
    }

    // 创建 `/sys/dev/block` 目录
    {
        // debug!("create /sys/dev/block");
        let dev_kset = sys_dev_kset();
        let dev_block_kset = KSet::new("block".to_string());
        let parent = dev_kset.clone() as Arc<dyn KObject>;
        dev_block_kset.set_parent(Some(Arc::downgrade(&parent)));

        dev_block_kset
            .register(Some(dev_kset))
            .expect("register dev block kset failed");

        unsafe { set_sys_dev_block_kset(dev_block_kset) };
    }

    // 创建 `/sys/dev/char` 目录
    {
        // debug!("create /sys/dev/char");
        let dev_kset = sys_dev_kset();
        let dev_char_kset = KSet::new("char".to_string());
        let parent = dev_kset.clone() as Arc<dyn KObject>;
        dev_char_kset.set_parent(Some(Arc::downgrade(&parent)));

        dev_char_kset
            .register(Some(dev_kset))
            .expect("register dev char kset failed");

        unsafe { set_sys_dev_char_kset(dev_char_kset) };
    }

    info!("devices init success");

    return Ok(());
}
