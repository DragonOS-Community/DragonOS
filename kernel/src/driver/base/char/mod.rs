use alloc::sync::Arc;
use log::error;

use system_error::SystemError;

use super::{
    device::{
        device_manager,
        device_number::{DeviceNumber, Major},
        Device, IdTable, CHARDEVS, DEVMAP,
    },
    map::{
        kobj_map, kobj_unmap, DeviceStruct, DEV_MAJOR_DYN_END, DEV_MAJOR_DYN_EXT_END,
        DEV_MAJOR_DYN_EXT_START, DEV_MAJOR_HASH_SIZE, DEV_MAJOR_MAX,
    },
};

pub trait CharDevice: Device {
    /// Notice buffer对应设备按字节划分，使用u8类型
    /// Notice offset应该从0开始计数
    ///
    /// @brief: 从设备的第offset个字节开始，读取len个byte，存放到buf中
    /// @parameter offset: 起始字节偏移量
    /// @parameter len: 读取字节的数量
    /// @parameter buf: 目标数组
    /// @return: 如果操作成功，返回操作的长度(单位是字节)；否则返回错误码；如果操作异常，但是并没有检查出什么错误，将返回已操作的长度
    fn read(&self, len: usize, buf: &mut [u8]) -> Result<usize, SystemError>;

    /// @brief: 从设备的第offset个字节开始，把buf数组的len个byte，写入到设备中
    /// @parameter offset: 起始字节偏移量
    /// @parameter len: 读取字节的数量
    /// @parameter buf: 目标数组
    /// @return: 如果操作成功，返回操作的长度(单位是字节)；否则返回错误码；如果操作异常，但是并没有检查出什么错误，将返回已操作的长度
    fn write(&self, len: usize, buf: &[u8]) -> Result<usize, SystemError>;

    /// @brief: 同步信息，把所有的dirty数据写回设备 - 待实现
    fn sync(&self) -> Result<(), SystemError>;
}

/// @brief 字符设备框架函数集
pub struct CharDevOps;

impl CharDevOps {
    /// @brief: 主设备号转下标
    /// @parameter: major: 主设备号
    /// @return: 返回下标
    #[allow(dead_code)]
    fn major_to_index(major: Major) -> usize {
        return (major.data() % DEV_MAJOR_HASH_SIZE) as usize;
    }

    /// @brief: 动态获取主设备号
    /// @parameter: None
    /// @return: 如果成功，返回主设备号，否则，返回错误码
    #[allow(dead_code)]
    fn find_dynamic_major() -> Result<Major, SystemError> {
        let chardevs = CHARDEVS.lock();
        // 寻找主设备号为234～255的设备
        for index in (DEV_MAJOR_DYN_END.data()..DEV_MAJOR_HASH_SIZE).rev() {
            if let Some(item) = chardevs.get(index as usize) {
                if item.is_empty() {
                    return Ok(Major::new(index)); // 返回可用的主设备号
                }
            }
        }
        // 寻找主设备号在384～511的设备
        for index in
            ((DEV_MAJOR_DYN_EXT_END.data() + 1)..(DEV_MAJOR_DYN_EXT_START.data() + 1)).rev()
        {
            if let Some(chardevss) = chardevs.get(Self::major_to_index(Major::new(index))) {
                let mut flag = true;
                for item in chardevss {
                    if item.device_number().major().data() == index {
                        flag = false;
                        break;
                    }
                }
                if flag {
                    // 如果数组中不存在主设备号等于index的设备
                    return Ok(Major::new(index)); // 返回可用的主设备号
                }
            }
        }
        return Err(SystemError::EBUSY);
    }

    /// @brief: 注册设备号，该函数需要指定主设备号
    /// @parameter: from: 主设备号
    ///             count: 次设备号数量
    ///             name: 字符设备名
    /// @return: 如果注册成功，返回设备号，否则，返回错误码
    #[allow(dead_code)]
    pub fn register_chardev_region(
        from: DeviceNumber,
        count: u32,
        name: &'static str,
    ) -> Result<DeviceNumber, SystemError> {
        Self::__register_chardev_region(from, count, name)
    }

    /// @brief: 注册设备号，该函数自动分配主设备号
    /// @parameter: baseminor: 次设备号
    ///             count: 次设备号数量
    ///             name: 字符设备名
    /// @return: 如果注册成功，返回，否则，返回false
    #[allow(dead_code)]
    pub fn alloc_chardev_region(
        baseminor: u32,
        count: u32,
        name: &'static str,
    ) -> Result<DeviceNumber, SystemError> {
        Self::__register_chardev_region(
            DeviceNumber::new(Major::UNNAMED_MAJOR, baseminor),
            count,
            name,
        )
    }

    /// @brief: 注册设备号
    /// @parameter: device_number: 设备号，主设备号如果为0，则动态分配
    ///             minorct: 次设备号数量
    ///             name: 字符设备名
    /// @return: 如果注册成功，返回设备号，否则，返回错误码
    fn __register_chardev_region(
        device_number: DeviceNumber,
        minorct: u32,
        name: &'static str,
    ) -> Result<DeviceNumber, SystemError> {
        let mut major = device_number.major();
        let baseminor = device_number.minor();
        if major >= DEV_MAJOR_MAX {
            error!(
                "DEV {} major requested {:?} is greater than the maximum {}\n",
                name,
                major,
                DEV_MAJOR_MAX.data() - 1
            );
        }
        if minorct > DeviceNumber::MINOR_MASK + 1 - baseminor {
            error!("DEV {} minor range requested ({}-{}) is out of range of maximum range ({}-{}) for a single major\n",
                name, baseminor, baseminor + minorct - 1, 0, DeviceNumber::MINOR_MASK);
        }
        let chardev = DeviceStruct::new(DeviceNumber::new(major, baseminor), minorct, name);
        if major == Major::UNNAMED_MAJOR {
            // 如果主设备号为0,则自动分配主设备号
            major = Self::find_dynamic_major().expect("Find synamic major error.\n");
        }
        if let Some(items) = CHARDEVS.lock().get_mut(Self::major_to_index(major)) {
            let mut insert_index: usize = 0;
            for (index, item) in items.iter().enumerate() {
                insert_index = index;
                match item.device_number().major().cmp(&major) {
                    core::cmp::Ordering::Less => continue,
                    core::cmp::Ordering::Greater => {
                        break; // 大于则向后插入
                    }
                    core::cmp::Ordering::Equal => {
                        if item.device_number().minor() + item.minorct() <= baseminor {
                            continue; // 下一个主设备号大于或者次设备号大于被插入的次设备号最大值
                        }
                        if item.base_minor() >= baseminor + minorct {
                            break; // 在此处插入
                        }
                        return Err(SystemError::EBUSY); // 存在重合的次设备号
                    }
                }
            }
            items.insert(insert_index, chardev);
        }

        return Ok(DeviceNumber::new(major, baseminor));
    }

    /// @brief: 注销设备号
    /// @parameter: major: 主设备号，如果为0，动态分配
    ///             baseminor: 起始次设备号
    ///             minorct: 次设备号数量
    /// @return: 如果注销成功，返回()，否则，返回错误码
    fn __unregister_chardev_region(
        device_number: DeviceNumber,
        minorct: u32,
    ) -> Result<(), SystemError> {
        if let Some(items) = CHARDEVS
            .lock()
            .get_mut(Self::major_to_index(device_number.major()))
        {
            for (index, item) in items.iter().enumerate() {
                if item.device_number() == device_number && item.minorct() == minorct {
                    // 设备号和数量都相等
                    items.remove(index);
                    return Ok(());
                }
            }
        }
        return Err(SystemError::EBUSY);
    }

    /// @brief: 字符设备注册
    /// @parameter: cdev: 字符设备实例
    ///             dev_t: 字符设备号
    ///             range: 次设备号范围
    /// @return: none
    #[allow(dead_code)]
    pub fn cdev_add(
        cdev: Arc<dyn CharDevice>,
        id_table: IdTable,
        range: usize,
    ) -> Result<(), SystemError> {
        if id_table.device_number().data() == 0 {
            error!("Device number can't be 0!\n");
        }
        device_manager().add_device(cdev.clone())?;
        kobj_map(
            DEVMAP.clone(),
            id_table.device_number(),
            range,
            cdev.clone(),
        );

        return Ok(());
    }

    /// @brief: 字符设备注销
    /// @parameter: dev_t: 字符设备号
    ///             range: 次设备号范围
    /// @return: none
    #[allow(dead_code)]
    pub fn cdev_del(id_table: IdTable, range: usize) {
        device_manager().remove_device(&id_table);
        kobj_unmap(DEVMAP.clone(), id_table.device_number(), range);
    }
}
