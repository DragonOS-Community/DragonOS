use alloc::{sync::Arc, vec::Vec};
use crate::libs::spinlock::SpinLock;
use super::device::{DeviceNumber, KObject};

struct LockProbe(SpinLock<Probe>);

impl LockProbe {
    pub fn new(dev_t: DeviceNumber, range: usize, data: Option<Arc<dyn KObject>>) -> Self {
        SpinLock::new(Probe {
            dev_t,
            range,
            data,
        })
    }
}

struct Probe {
    dev_t: DeviceNumber,
    range: usize,
    data: Option<Arc<dyn KObject>>,
}

impl Probe {
    pub fn new(dev_t: DeviceNumber, range: usize, data: Option<Arc<dyn KObject>>) -> Self {
        Self {
            dev_t,
            range,
            data,
        }
    }
}

pub struct LockKObjMap(SpinLock<KObjMap>);

impl Default for LockKObjMap {
    fn default() -> Self {
        Self(SpinLock::new(KObjMap::default()))
    }
}

struct KObjMap(Vec<Vec<LockProbe>>);

impl Default for KObjMap {
    fn default() -> Self {
        Self (vec![Vec::new(), 255])
    }
}
