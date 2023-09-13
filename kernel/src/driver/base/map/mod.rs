use core::cmp::Ordering;

use super::{
    block::block_device::BlockDevice,
    char::CharDevice,
    device::{mkdev, DeviceNumber, IdTable, KObject, BLOCKDEVS, CHARDEVS, DEVICE_MANAGER, DEVMAP},
};
use crate::{kerror, libs::spinlock::SpinLock, syscall::SystemError};
use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

const KOBJMAP_HASH_SIZE: usize = 255;
const DEV_MAJOR_HASH_SIZE: usize = 255;
const DEV_MAJOR_MAX: usize = 512;
const MINOR_BITS: usize = 20;
const MINOR_MASK: usize = 1 << MINOR_BITS - 1;
/* Marks the bottom of the first segment of free char majors */
const DEV_MAJOR_DYN_END: usize = 234;
/* Marks the top and bottom of the second segment of free char majors */
const DEV_MAJOR_DYN_EXT_START: usize = 511;
const DEV_MAJOR_DYN_EXT_END: usize = 384;

/// @brief: 字符设备与块设备管理结构体
#[derive(Debug, Clone)]
struct Probe(Arc<dyn KObject>);

impl Probe {
    /// @brief: 新建probe实例
    /// @parameter: data: probe实例
    /// @return: probe实例
    pub fn new(data: Arc<dyn KObject>) -> Self {
        Self(data)
    }
}

/// @brief: 字符设备和块设备管理实例(锁)
#[derive(Debug)]
pub struct LockedKObjMap(SpinLock<KObjMap>);

impl Default for LockedKObjMap {
    fn default() -> Self {
        Self(SpinLock::new(KObjMap::default()))
    }
}

/// @brief: 字符设备和块设备管理实例
#[derive(Debug, Clone)]
struct KObjMap(Vec<BTreeMap<DeviceNumber, Probe>>);

impl Default for KObjMap {
    fn default() -> Self {
        Self(vec![BTreeMap::new(); KOBJMAP_HASH_SIZE])
    }
}

/// @brief: obj设备注册
/// @parameter: domain: 管理实例
///             dev_t: 设备号
///             range: 次设备号范围
///             data: 设备实例
/// @return: none
pub fn kobj_map(
    domain: Arc<LockedKObjMap>,
    dev_t: DeviceNumber,
    range: usize,
    data: Arc<dyn KObject>,
) {
    if let Some(map) = domain.0.lock().0.get_mut(dev_t.major() % 255) {
        for i in 0..range {
            map.insert(
                mkdev(dev_t.major(), dev_t.minor() + i),
                Probe::new(data.clone()),
            );
        }
    }
}

/// @brief: obj设备注销
/// @parameter: domain: 管理实例
///             dev_t: 设备号
///             range: 次设备号范围
/// @return: none
pub fn kobj_unmap(domain: Arc<LockedKObjMap>, dev_t: DeviceNumber, range: usize) {
    if let Some(map) = domain.0.lock().0.get_mut(dev_t.major() % 255) {
        for i in 0..range {
            let rm_dev_t = &DeviceNumber::new(Into::<usize>::into(dev_t) + i);
            match map.get(rm_dev_t) {
                Some(_) => {
                    map.remove(rm_dev_t);
                }
                None => {}
            }
        }
    }
}

/// @brief: 设备查找
/// @parameter: domain: 管理实例
///             dev_t: 设备号
/// @return: 查找成功，返回设备实例，否则返回None
#[allow(dead_code)]
pub fn kobj_lookup(domain: Arc<LockedKObjMap>, dev_t: DeviceNumber) -> Option<Arc<dyn KObject>> {
    if let Some(map) = domain.0.lock().0.get(dev_t.major() % 255) {
        match map.get(&dev_t) {
            Some(value) => {
                return Some(value.0.clone());
            }
            None => {
                return None;
            }
        }
    }
    return None;
}

// 管理字符设备号的map(加锁)
pub struct LockedDevsMap(SpinLock<DevsMap>);

impl Default for LockedDevsMap {
    fn default() -> Self {
        LockedDevsMap(SpinLock::new(DevsMap::default()))
    }
}

// 管理字符设备号的map
#[derive(Debug)]
struct DevsMap(Vec<Vec<DeviceStruct>>);

impl Default for DevsMap {
    fn default() -> Self {
        DevsMap(vec![Vec::new(); DEV_MAJOR_HASH_SIZE])
    }
}

// 字符设备在系统中的实例，devfs通过该结构与实际字符设备进行联系
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DeviceStruct {
    dev_t: DeviceNumber, //起始设备号
    minorct: usize,      // 次设备号数量
    name: &'static str,  //字符设备名
}

impl DeviceStruct {
    /// @brief: 创建实例
    /// @parameter: dev_t: 设备号
    ///             minorct: 次设备号数量
    ///             name: 字符设备名
    ///             char: 字符设备实例
    /// @return: 实例
    ///
    #[allow(dead_code)]
    pub fn new(dev_t: DeviceNumber, minorct: usize, name: &'static str) -> Self {
        Self {
            dev_t,
            minorct,
            name,
        }
    }

    /// @brief: 获取起始次设备号
    /// @parameter: None
    /// @return: 起始设备号
    ///
    #[allow(dead_code)]
    pub fn device_number(&self) -> DeviceNumber {
        self.dev_t
    }

    /// @brief: 获取起始次设备号
    /// @parameter: None
    /// @return: 起始设备号
    ///
    #[allow(dead_code)]
    pub fn base_minor(&self) -> usize {
        self.dev_t.minor()
    }

    /// @brief: 获取次设备号数量
    /// @parameter: None
    /// @return: 次设备号数量
    #[allow(dead_code)]
    pub fn minorct(&self) -> usize {
        self.minorct
    }
}

// 这下面是考虑到 块设备的注册和字符设备的注册在设备号的自动分配上要有所区别，
// 暂时没有去查具体是怎么做区分的，因此暂时还是一样的

/// @brief 块设备框架函数集
pub struct BlockDeviceOps;

impl BlockDeviceOps {
    /// @brief: 主设备号转下标
    /// @parameter: major: 主设备号
    /// @return: 返回下标
    #[allow(dead_code)]
    fn major_to_index(major: usize) -> usize {
        return major % DEV_MAJOR_HASH_SIZE;
    }

    /// @brief: 动态获取主设备号
    /// @parameter: None
    /// @return: 如果成功，返回主设备号，否则，返回错误码
    #[allow(dead_code)]
    fn find_dynamic_major() -> Result<usize, SystemError> {
        let blockdevs = BLOCKDEVS.0.lock();
        // 寻找主设备号为234～255的设备
        for index in (DEV_MAJOR_DYN_END..DEV_MAJOR_HASH_SIZE).rev() {
            if let Some(item) = blockdevs.0.get(index) {
                if item.is_empty() {
                    return Ok(index); // 返回可用的主设备号
                }
            }
        }
        // 寻找主设备号在384～511的设备
        for index in (DEV_MAJOR_DYN_EXT_END + 1..DEV_MAJOR_DYN_EXT_START + 1).rev() {
            if let Some(blockdevss) = blockdevs.0.get(Self::major_to_index(index)) {
                let mut flag = true;
                for item in blockdevss {
                    if item.device_number().major() == index {
                        flag = false;
                        break;
                    }
                }
                if flag {
                    // 如果数组中不存在主设备号等于index的设备
                    return Ok(index); // 返回可用的主设备号
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
    pub fn register_blockdev_region(
        from: DeviceNumber,
        count: usize,
        name: &'static str,
    ) -> Result<DeviceNumber, SystemError> {
        Self::__register_blockdev_region(from, count, name)
    }

    /// @brief: 注册设备号，该函数自动分配主设备号
    /// @parameter: baseminor: 主设备号
    ///             count: 次设备号数量
    ///             name: 字符设备名
    /// @return: 如果注册成功，返回，否则，返回false
    #[allow(dead_code)]
    pub fn alloc_blockdev_region(
        baseminor: usize,
        count: usize,
        name: &'static str,
    ) -> Result<DeviceNumber, SystemError> {
        Self::__register_blockdev_region(mkdev(0, baseminor), count, name)
    }

    /// @brief: 注册设备号
    /// @parameter: device_number: 设备号，主设备号如果为0，则动态分配
    ///             minorct: 次设备号数量
    ///             name: 字符设备名
    /// @return: 如果注册成功，返回设备号，否则，返回错误码
    fn __register_blockdev_region(
        device_number: DeviceNumber,
        minorct: usize,
        name: &'static str,
    ) -> Result<DeviceNumber, SystemError> {
        let mut major = device_number.major();
        let baseminor = device_number.minor();
        if major >= DEV_MAJOR_MAX {
            kerror!(
                "DEV {} major requested {} is greater than the maximum {}\n",
                name,
                major,
                DEV_MAJOR_MAX - 1
            );
        }
        if minorct > MINOR_MASK + 1 - baseminor {
            kerror!("DEV {} minor range requested ({}-{}) is out of range of maximum range ({}-{}) for a single major\n",
                name, baseminor, baseminor + minorct - 1, 0, MINOR_MASK);
        }
        let blockdev = DeviceStruct::new(mkdev(major, baseminor), minorct, name);
        if major == 0 {
            // 如果主设备号为0,则自动分配主设备号
            major = Self::find_dynamic_major().expect("Find synamic major error.\n");
        }
        if let Some(items) = BLOCKDEVS.0.lock().0.get_mut(Self::major_to_index(major)) {
            let mut insert_index: usize = 0;
            for (index, item) in items.iter().enumerate() {
                insert_index = index;
                match item.device_number().major().cmp(&major) {
                    Ordering::Less => continue,
                    Ordering::Greater => {
                        break; // 大于则向后插入
                    }
                    Ordering::Equal => {
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
            items.insert(insert_index, blockdev);
        }
        return Ok(mkdev(major, baseminor));
    }

    /// @brief: 注销设备号
    /// @parameter: major: 主设备号，如果为0，动态分配
    ///             baseminor: 起始次设备号
    ///             minorct: 次设备号数量
    /// @return: 如果注销成功，返回()，否则，返回错误码
    fn __unregister_blockdev_region(
        device_number: DeviceNumber,
        minorct: usize,
    ) -> Result<(), SystemError> {
        if let Some(items) = BLOCKDEVS
            .0
            .lock()
            .0
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

    /// @brief: 块设备注册
    /// @parameter: cdev: 字符设备实例
    ///             dev_t: 字符设备号
    ///             range: 次设备号范围
    /// @return: none
    #[allow(dead_code)]
    pub fn bdev_add(bdev: Arc<dyn BlockDevice>, id_table: IdTable) {
        if Into::<usize>::into(id_table.device_number()) == 0 {
            kerror!("Device number can't be 0!\n");
        }
        DEVICE_MANAGER.add_device(id_table, bdev.device())
    }

    /// @brief: block设备注销
    /// @parameter: dev_t: 字符设备号
    ///             range: 次设备号范围
    /// @return: none
    #[allow(dead_code)]
    pub fn bdev_del(_devnum: DeviceNumber, _range: usize) {}
}

/// @brief 字符设备框架函数集
pub struct CharDevOps;

impl CharDevOps {
    /// @brief: 主设备号转下标
    /// @parameter: major: 主设备号
    /// @return: 返回下标
    #[allow(dead_code)]
    fn major_to_index(major: usize) -> usize {
        return major % DEV_MAJOR_HASH_SIZE;
    }

    /// @brief: 动态获取主设备号
    /// @parameter: None
    /// @return: 如果成功，返回主设备号，否则，返回错误码
    #[allow(dead_code)]
    fn find_dynamic_major() -> Result<usize, SystemError> {
        let chardevs = CHARDEVS.0.lock();
        // 寻找主设备号为234～255的设备
        for index in (DEV_MAJOR_DYN_END..DEV_MAJOR_HASH_SIZE).rev() {
            if let Some(item) = chardevs.0.get(index) {
                if item.is_empty() {
                    return Ok(index); // 返回可用的主设备号
                }
            }
        }
        // 寻找主设备号在384～511的设备
        for index in (DEV_MAJOR_DYN_EXT_END + 1..DEV_MAJOR_DYN_EXT_START + 1).rev() {
            if let Some(chardevss) = chardevs.0.get(Self::major_to_index(index)) {
                let mut flag = true;
                for item in chardevss {
                    if item.device_number().major() == index {
                        flag = false;
                        break;
                    }
                }
                if flag {
                    // 如果数组中不存在主设备号等于index的设备
                    return Ok(index); // 返回可用的主设备号
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
        count: usize,
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
        baseminor: usize,
        count: usize,
        name: &'static str,
    ) -> Result<DeviceNumber, SystemError> {
        Self::__register_chardev_region(mkdev(0, baseminor), count, name)
    }

    /// @brief: 注册设备号
    /// @parameter: device_number: 设备号，主设备号如果为0，则动态分配
    ///             minorct: 次设备号数量
    ///             name: 字符设备名
    /// @return: 如果注册成功，返回设备号，否则，返回错误码
    fn __register_chardev_region(
        device_number: DeviceNumber,
        minorct: usize,
        name: &'static str,
    ) -> Result<DeviceNumber, SystemError> {
        let mut major = device_number.major();
        let baseminor = device_number.minor();
        if major >= DEV_MAJOR_MAX {
            kerror!(
                "DEV {} major requested {} is greater than the maximum {}\n",
                name,
                major,
                DEV_MAJOR_MAX - 1
            );
        }
        if minorct > MINOR_MASK + 1 - baseminor {
            kerror!("DEV {} minor range requested ({}-{}) is out of range of maximum range ({}-{}) for a single major\n",
                name, baseminor, baseminor + minorct - 1, 0, MINOR_MASK);
        }
        let chardev = DeviceStruct::new(mkdev(major, baseminor), minorct, name);
        if major == 0 {
            // 如果主设备号为0,则自动分配主设备号
            major = Self::find_dynamic_major().expect("Find synamic major error.\n");
        }
        if let Some(items) = CHARDEVS.0.lock().0.get_mut(Self::major_to_index(major)) {
            let mut insert_index: usize = 0;
            for (index, item) in items.iter().enumerate() {
                insert_index = index;
                match item.device_number().major().cmp(&major) {
                    Ordering::Less => continue,
                    Ordering::Greater => {
                        break; // 大于则向后插入
                    }
                    Ordering::Equal => {
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
        return Ok(mkdev(major, baseminor));
    }

    /// @brief: 注销设备号
    /// @parameter: major: 主设备号，如果为0，动态分配
    ///             baseminor: 起始次设备号
    ///             minorct: 次设备号数量
    /// @return: 如果注销成功，返回()，否则，返回错误码
    fn __unregister_chardev_region(
        device_number: DeviceNumber,
        minorct: usize,
    ) -> Result<(), SystemError> {
        if let Some(items) = CHARDEVS
            .0
            .lock()
            .0
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
    pub fn cdev_add(cdev: Arc<dyn CharDevice>, id_table: IdTable, range: usize) {
        if Into::<usize>::into(id_table.device_number()) == 0 {
            kerror!("Device number can't be 0!\n");
        }
        DEVICE_MANAGER.add_device(id_table.clone(), cdev.clone());
        kobj_map(
            DEVMAP.clone(),
            id_table.device_number(),
            range,
            cdev.clone(),
        )
    }

    /// @brief: 字符设备注销
    /// @parameter: dev_t: 字符设备号
    ///             range: 次设备号范围
    /// @return: none
    #[allow(dead_code)]
    pub fn cdev_del(id_table: IdTable, range: usize) {
        DEVICE_MANAGER.remove_device(&id_table);
        kobj_unmap(DEVMAP.clone(), id_table.device_number(), range);
    }
}
