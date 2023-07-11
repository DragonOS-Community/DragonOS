use super::device::{mkdev, DeviceNumber, KObject};
use crate::libs::spinlock::SpinLock;
use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

const KOBJMAP_HASH_SIZE: usize = 255;

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
