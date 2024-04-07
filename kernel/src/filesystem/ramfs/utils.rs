//! 用于展示如何在保留比较的同时支持从当前inode原地取出目录名
use super::LockedRamfsEntry;
use alloc::{
    string::String,
    sync::{Arc, Weak},
};
use core::cmp::Ordering;

#[derive(Debug)]
pub struct Keyer(Weak<LockedRamfsEntry>, Option<String>);

impl Keyer {
    pub fn from_str(key: &str) -> Self {
        Keyer(Weak::new(), Some(String::from(key)))
    }

    pub fn from_entry(entry: &Arc<LockedRamfsEntry>) -> Self {
        Keyer(Arc::downgrade(entry), None)
    }

    /// 获取name
    pub fn get(&self) -> Option<String> {
        if self.1.is_some() {
            return self.1.clone();
        }
        Some(self.0.upgrade()?.0.lock().name.clone())
    }
}

// For Btree insertion
impl PartialEq for Keyer {
    fn eq(&self, other: &Self) -> bool {
        if self.0.ptr_eq(&other.0) {
            return true;
        }
        if self.1.is_none() && other.1.is_none() {
            // cmp between wrapper
            let opt1 = self.0.upgrade();
            let opt2 = other.0.upgrade();
            if opt1.is_none() && opt2.is_none() {
                panic!("Empty of both");
            }
            if opt1.is_none() || opt2.is_none() {
                return false;
            }
            return opt1.unwrap().0.lock().name == opt2.unwrap().0.lock().name;
        }

        if self.1.is_none() {
            let opt = self.0.upgrade();
            if opt.is_none() {
                // kwarn!("depecated");
                return false;
            }

            return &opt.unwrap().0.lock().name == other.1.as_ref().unwrap();
        } else {
            let opt = other.0.upgrade();
            if opt.is_none() {
                // kwarn!("depecated");
                return false;
            }

            return &opt.unwrap().0.lock().name == self.1.as_ref().unwrap();
        }
    }
}

impl Eq for Keyer {}

// Uncheck Stable
impl PartialOrd for Keyer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Keyer {
    fn cmp(&self, other: &Self) -> Ordering {
        // let mut ret: Ordering = Ordering::Equal;
        if self.0.ptr_eq(&other.0) {
            return Ordering::Equal;
        }
        if self.1.is_none() && other.1.is_none() {
            // cmp between wrapper
            let opt1 = self.0.upgrade();
            let opt2 = other.0.upgrade();
            if opt1.is_none() && opt2.is_none() {
                panic!("All Keys None, compare error!");
            }
            if let Some(o1) = opt1 {
                if let Some(o2) = opt2 {
                    return o1.0.lock().name.cmp(&o2.0.lock().name);
                }
            }
            panic!("Empty Key!");
        } else if self.1.is_none() {
            let opt = self.0.upgrade();
            if opt.is_none() {
                panic!("Empty Key!");
            }
            return opt.unwrap().0.lock().name.cmp(other.1.as_ref().unwrap());
        } else {
            let opt = other.0.upgrade();
            if opt.is_none() {
                panic!("Empty Key!");
            }

            return self.1.as_ref().unwrap().cmp(&opt.unwrap().0.lock().name);
        }
    }
}
