use alloc::{sync::Arc, vec::Vec};
use crate::{
    libs::spinlock::{SpinLock, SpinLockGuard},
    syscall::SystemError,
};
use super::{
    map::LockKObjMap, 
    device::{mkdev, Device, DeviceNumber}
};

lazy_static! {
    // 全局字符设备号管理实例
    pub static ref CHRDEVS: Arc<LockChrDevs> = Arc::new(LockChrDevs::default());

    // 全局字符设备管理实例
    pub static ref CDEVMAP: Arc<LockKObjMap> = Arc::new(LockKObjMap::default());
}

pub trait FileOperations {
    fn open(&self) -> Result<(), SystemError>;

    fn close(&self) -> Result<(), SystemError>;
}

pub trait CharDevice: Device {

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
        ChrDevs(vec![Vec::new(); 255])
    }
}

// 字符设备在系统中的实例，devfs通过该结构与实际字符设备进行联系
#[derive(Debug, Clone)]
pub struct CharDeviceStruct {
    dev_t: DeviceNumber, //起始设备号
    minorct: usize,      // 次设备号数量
    name: &'static str,  //字符设备名
    char: Option<Arc<dyn CharDevice>>, // 字符设备实例
}

impl CharDeviceStruct {
    /// @brief: 创建实例
    /// @parameter: dev_t: 设备号
    ///             minorct: 次设备号数量
    ///             name: 字符设备名
    ///             char: 字符设备实例
    /// @return: 实例
    pub fn new(
        dev_t: DeviceNumber,
        minorct: usize,
        name: &'static str,
        char: Option<Arc<dyn CharDevice>>,
    ) -> Self {
        Self {
            dev_t,
            minorct,
            name,
            char,
        }
    }

    /// @brief: 获取下一个次设备号
    /// @parameter: None
    /// @return: 下一个次设备号
    pub fn final_minor(&self) -> usize {
        self.dev_t.minor() + self.minorct
    }

    /// @brief: 获取主设备号
    /// @parameter: None
    /// @return: 主设备号
    pub fn major(&self) -> usize {
        self.dev_t.major
    }
}

/// @brief: 注册设备号，该函数需要指定主设备号
/// @parameter: major: 主设备号
///             count: 次设备号数量
///             name: 字符设备名
///             char: 字符设备实例
/// @return: 如果注册成功，返回设备号，否则，返回错误码
pub fn register_chrdev_region(
    major: usize,
    count: usize,
    name: &'static str,
    char: Option<Arc<dyn CharDevice>>,
) -> Result<DeviceNumber, SystemError> {
    if major == 0 {
        return alloc_chrdev_region(count, name, char);
    }
    let mut map: SpinLockGuard<ChrDevs> = CHRDEVS.0.lock();
    match map.0.get_mut(major % 255) {
        Some(value) => {
            if value.is_empty() {
                // 主设备号还未注册任何设备，创建次设备实例
                let chrdev: CharDeviceStruct =
                    CharDeviceStruct::new(mkdev(major, 0), count, name, char);
                value.push(chrdev);
                return Ok(mkdev(major, 0));
            } else {
                for (index, char) in value {
                    if char.major() > major { // 找到了主设备号大于major的主设备
                        // 获取属于major最后的次设备号
                        let minor = value[index - 1].final_minor();
                        // 创建管理字符设备实例
                        let chrdev: CharDeviceStruct =
                            CharDeviceStruct::new(mkdev(major, minor), count, name, char);
                        value.insert(index, chrdev);
                        return Ok(mkdev(major, minor));
                    }
                    if index == value.len() - 1 { // 该主设备号为最大
                        // 创建管理字符设备实例
                        let chrdev: CharDeviceStruct =
                            CharDeviceStruct::new(mkdev(major, 0), count, name, char);
                        value.push(chrdev);
                        return Ok(mkdev(major, 0));
                    }
                }
            }
        }
        None => {      
            return Err(SystemError::EPERM);
        }
    }
}

/// @brief: 注册设备号，该函数自动分配主设备号
/// @parameter: major: 主设备号
///             count: 次设备号数量
///             name: 字符设备名
///             char: 字符设备实例
/// @return: 如果注册成功，返回，否则，返回false
pub fn alloc_chrdev_region(
    count: usize,
    name: &'static str,
    char: Option<Arc<dyn CharDevice>>,
) -> Result<DeviceNumber, SystemError> {
    let mut map: SpinLockGuard<ChrDevs> = CHRDEVS.0.lock();
    for (index, array) in map.0.iter_mut().enumerate() {
        if array.is_empty() {
            // 不存在主设备号，创建实例
            let chrdev: CharDeviceStruct = CharDeviceStruct::new(mkdev(index, 0), count, name, char);
            array.push(chrdev);
            return Ok(mkdev(index, 0));
        }
    }
    // 主设备号已被用完
    return Err(SystemError::EPERM);
}

pub fn cdev_add(cdev: Arc<dyn CharDevice>, dev_t: DeviceNumber, count: usize) {

}