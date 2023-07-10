use super::{
    device::{mkdev, DeviceNumber, KObject},
    map::{kobj_map, kobj_unmap, LockKObjMap},
};
use crate::{filesystem::vfs::IndexNode, kerror, libs::spinlock::SpinLock, syscall::SystemError};
use alloc::{sync::Arc, vec::Vec};
use core::cmp::Ordering;

const CHRDEV_MAJOR_HASH_SIZE: usize = 255;
const CHRDEV_MAJOR_MAX: usize = 512;
const MINOR_BITS: usize = 20;
const MINOR_MASK: usize = 1 << MINOR_BITS - 1;
/* Marks the bottom of the first segment of free char majors */
const CHRDEV_MAJOR_DYN_END: usize = 234;
/* Marks the top and bottom of the second segment of free char majors */
const CHRDEV_MAJOR_DYN_EXT_START: usize = 511;
const CHRDEV_MAJOR_DYN_EXT_END: usize = 384;

lazy_static! {
    // 全局字符设备号管理实例
    pub static ref CHRDEVS: Arc<LockChrDevs> = Arc::new(LockChrDevs::default());

    // 全局字符设备管理实例
    pub static ref CDEVMAP: Arc<LockKObjMap> = Arc::new(LockKObjMap::default());
}

pub trait CharDevice: KObject {
    /// @brief: 打开设备
    /// @parameter: file: devfs inode
    /// @return: 打开成功，返回OK(())，失败，返回错误代码
    fn open(&self, file: Arc<dyn IndexNode>) -> Result<(), SystemError>;

    /// @brief: 关闭设备
    /// @parameter: file: devfs inode
    /// @return: 关闭成功，返回OK(())，失败，返回错误代码
    fn close(&self, file: Arc<dyn IndexNode>) -> Result<(), SystemError>;
}

// 管理字符设备号的map(加锁)
pub struct LockChrDevs(SpinLock<ChrDevs>);

impl Default for LockChrDevs {
    fn default() -> Self {
        LockChrDevs(SpinLock::new(ChrDevs::default()))
    }
}

// 管理字符设备号的map
#[derive(Debug)]
struct ChrDevs(Vec<Vec<CharDeviceStruct>>);

impl Default for ChrDevs {
    fn default() -> Self {
        ChrDevs(vec![Vec::new(); CHRDEV_MAJOR_HASH_SIZE])
    }
}

// 字符设备在系统中的实例，devfs通过该结构与实际字符设备进行联系
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct CharDeviceStruct {
    dev_t: DeviceNumber, //起始设备号
    minorct: usize,      // 次设备号数量
    name: &'static str,  //字符设备名
}

impl CharDeviceStruct {
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

/// @brief: 主设备号转下标
/// @parameter: major: 主设备号
/// @return: 返回下标
#[allow(dead_code)]
fn major_to_index(major: usize) -> usize {
    return major % CHRDEV_MAJOR_HASH_SIZE;
}

/// @brief: 动态获取主设备号
/// @parameter: None
/// @return: 如果成功，返回主设备号，否则，返回错误码 
#[allow(dead_code)]
fn find_dynamic_major() -> Result<usize, SystemError> {
    let chrdevs = CHRDEVS.0.lock();
    // 寻找主设备号为234～255的设备
    for index in (CHRDEV_MAJOR_DYN_END..CHRDEV_MAJOR_HASH_SIZE).rev() {
        if let Some(item) = chrdevs.0.get(index) {
            if item.is_empty() {
                return Ok(index); // 返回可用的主设备号
            }
        }
    }
    // 寻找主设备号在384～511的设备
    for index in (CHRDEV_MAJOR_DYN_EXT_END + 1..CHRDEV_MAJOR_DYN_EXT_START + 1).rev() {
        if let Some(chrdevss) = chrdevs.0.get(major_to_index(index)) {
            let mut flag = true;
            for item in chrdevss {
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
pub fn register_chrdev_region(
    from: DeviceNumber,
    count: usize,
    name: &'static str,
) -> Result<DeviceNumber, SystemError> {
    __register_chrdev_region(from, count, name)
}

/// @brief: 注册设备号，该函数自动分配主设备号
/// @parameter: baseminor: 主设备号
///             count: 次设备号数量
///             name: 字符设备名
/// @return: 如果注册成功，返回，否则，返回false 
#[allow(dead_code)]
pub fn alloc_chrdev_region(
    baseminor: usize,
    count: usize,
    name: &'static str,
) -> Result<DeviceNumber, SystemError> {
    __register_chrdev_region(mkdev(0, baseminor), count, name)
}

/// @brief: 注册设备号
/// @parameter: device_number: 设备号，主设备号如果为0，则动态分配
///             minorct: 次设备号数量
///             name: 字符设备名
/// @return: 如果注册成功，返回设备号，否则，返回错误码
pub fn __register_chrdev_region(
    device_number: DeviceNumber,
    minorct: usize,
    name: &'static str,
) -> Result<DeviceNumber, SystemError> {
    let mut major = device_number.major();
    let baseminor = device_number.minor();
    if major >= CHRDEV_MAJOR_MAX {
        kerror!(
            "CHRDEV {} major requested {} is greater than the maximum {}\n",
            name,
            major,
            CHRDEV_MAJOR_MAX - 1
        );
    }
    if minorct > MINOR_MASK + 1 - baseminor {
        kerror!("CHRDEV {} minor range requested ({}-{}) is out of range of maximum range ({}-{}) for a single major\n",
			name, baseminor, baseminor + minorct - 1, 0, MINOR_MASK);
    }
    let chrdev = CharDeviceStruct::new(mkdev(major, baseminor), minorct, name);
    if major == 0 {
        // 如果主设备号为0,则自动分配主设备号
        major = find_dynamic_major().expect("Find synamic major error.\n");
    }
    if let Some(items) = CHRDEVS.0.lock().0.get_mut(major_to_index(major)) {
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
        items.insert(insert_index, chrdev);
    }
    return Ok(mkdev(major, baseminor));
}

/// @brief: 注销设备号
/// @parameter: major: 主设备号，如果为0，动态分配
///             baseminor: 起始次设备号
///             minorct: 次设备号数量
/// @return: 如果注销成功，返回()，否则，返回错误码
pub fn __unregister_chrdev_region(
    device_number: DeviceNumber,
    minorct: usize,
) -> Result<(), SystemError> {
    if let Some(items) = CHRDEVS
        .0
        .lock()
        .0
        .get_mut(major_to_index(device_number.major()))
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
pub fn cdev_add(cdev: Arc<dyn CharDevice>, dev_t: DeviceNumber, range: usize) {
    if Into::<usize>::into(dev_t) == 0 {
        kerror!("Device number can't be 0!\n");
    }
    kobj_map(CDEVMAP.clone(), dev_t, range, cdev);
}

/// @brief: 字符设备注销
/// @parameter: dev_t: 字符设备号
///             range: 次设备号范围
/// @return: none
#[allow(dead_code)]
pub fn cdev_del(dev_t: DeviceNumber, range: usize) {
    kobj_unmap(CDEVMAP.clone(), dev_t, range);
}
