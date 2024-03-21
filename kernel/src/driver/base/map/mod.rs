use core::ops::{Deref, DerefMut};

use super::{
    device::device_number::{DeviceNumber, Major},
    kobject::KObject,
};
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

const KOBJMAP_HASH_SIZE: usize = 255;
pub(crate) const DEV_MAJOR_HASH_SIZE: u32 = 255;
pub(crate) const DEV_MAJOR_MAX: Major = Major::new(512);

/* Marks the bottom of the first segment of free char majors */
pub(crate) const DEV_MAJOR_DYN_END: Major = Major::new(234);
/* Marks the top and bottom of the second segment of free char majors */
pub(crate) const DEV_MAJOR_DYN_EXT_START: Major = Major::new(511);
pub(crate) const DEV_MAJOR_DYN_EXT_END: Major = Major::new(384);

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
    if let Some(map) = domain
        .0
        .lock()
        .0
        .get_mut((dev_t.major().data() % 255) as usize)
    {
        for i in 0..range {
            map.insert(
                DeviceNumber::new(dev_t.major(), dev_t.minor() + i as u32),
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
    if let Some(map) = domain
        .0
        .lock()
        .0
        .get_mut((dev_t.major().data() % 255) as usize)
    {
        for i in 0..range {
            let rm_dev_t = &DeviceNumber::new(dev_t.major(), dev_t.minor() + i as u32);
            if map.get(rm_dev_t).is_some() {
                map.remove(rm_dev_t);
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
    if let Some(map) = domain.0.lock().0.get((dev_t.major().data() % 255) as usize) {
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

impl LockedDevsMap {
    #[inline(always)]
    pub fn lock(&self) -> SpinLockGuard<DevsMap> {
        self.0.lock()
    }
}

// 管理字符设备号的map
#[derive(Debug)]
pub struct DevsMap(Vec<Vec<DeviceStruct>>);

impl Default for DevsMap {
    fn default() -> Self {
        DevsMap(vec![Vec::new(); DEV_MAJOR_HASH_SIZE as usize])
    }
}

impl Deref for DevsMap {
    type Target = Vec<Vec<DeviceStruct>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for DevsMap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

// 字符设备在系统中的实例，devfs通过该结构与实际字符设备进行联系
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DeviceStruct {
    dev_t: DeviceNumber, //起始设备号
    minorct: u32,        // 次设备号数量
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
    pub fn new(dev_t: DeviceNumber, minorct: u32, name: &'static str) -> Self {
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
    pub fn base_minor(&self) -> u32 {
        self.dev_t.minor()
    }

    /// @brief: 获取次设备号数量
    /// @parameter: None
    /// @return: 次设备号数量
    #[allow(dead_code)]
    pub fn minorct(&self) -> u32 {
        self.minorct
    }
}
