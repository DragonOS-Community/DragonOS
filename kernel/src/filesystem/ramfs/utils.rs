use core::cmp::Ordering;

use alloc::{string::String, sync::{Arc, Weak}};

use super::LockedEntry;


#[derive(Debug)]
pub struct Keyer(Weak<LockedEntry>, Option<String>);

impl Keyer {
    pub fn from_str(key: &str) -> Self {
        Keyer(Weak::new(), Some(String::from(key)))
    }

    pub fn from_entry(entry: &Arc<LockedEntry>) -> Self {
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
            kdebug!("Compare itself!");
            return true;
        }
        if self.1.is_none() && other.1.is_none() {
            // cmp between wrapper
            let opt1 = self.0.upgrade();
            let opt2 = other.0.upgrade();
            if opt1.is_none() && opt2.is_none() {
                kerror!("Empty Both none!");
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
                kwarn!("depecated");
                return false;
            }

            return &opt.unwrap().0.lock().name == other.1.as_ref().unwrap();
        } else {
            let opt = other.0.upgrade();
            if opt.is_none() {
                kwarn!("depecated");
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
        if self.0.ptr_eq(&other.0) {
            kdebug!("Compare itself!");
            return Some(Ordering::Equal);
        }
        if self.1.is_none() && other.1.is_none() {
            // cmp between wrapper
            let opt1 = self.0.upgrade();
            let opt2 = other.0.upgrade();
            if opt1.is_none() && opt2.is_none() {
                kerror!("Empty Both none!");
                panic!("All Keys None, compare error!");
            }
            if opt1.is_some() && opt2.is_some() {
                return Some(
                    opt1.unwrap()
                        .0
                        .lock()
                        .name
                        .cmp(&opt2.unwrap().0.lock().name),
                );
            } else {
                kwarn!("depecated");
                panic!("Empty Key!");
            }
        } else {
            if self.1.is_none() {
                let opt = self.0.upgrade();
                if opt.is_none() {
                    kwarn!("depecated");
                    panic!("Empty Key!");
                }
                return Some(opt.unwrap().0.lock().name.cmp(other.1.as_ref().unwrap()));
            } else {
                let opt = other.0.upgrade();
                if opt.is_none() {
                    kwarn!("depecated");
                    panic!("Empty Key!");
                }

                return Some(opt.unwrap().0.lock().name.cmp(self.1.as_ref().unwrap()));
            }
        }
    }
}

impl Ord for Keyer {
    fn cmp(&self, other: &Self) -> Ordering {
        // let mut ret: Ordering = Ordering::Equal;
        if self.0.ptr_eq(&other.0) {
            kdebug!("Compare itself!");
            return Ordering::Equal;
        }
        if self.1.is_none() && other.1.is_none() {
            // cmp between wrapper
            let opt1 = self.0.upgrade();
            let opt2 = other.0.upgrade();
            if opt1.is_none() && opt2.is_none() {
                kerror!("Both None!");
                panic!("All Keys None, compare error!");
            }
            if opt1.is_some() && opt2.is_some() {
                return opt1
                    .unwrap()
                    .0
                    .lock()
                    .name
                    .cmp(&opt2.unwrap().0.lock().name);
            } else {
                kwarn!("depecated");
                panic!("Empty Key!");
            }
        } else {
            if self.1.is_none() {
                let opt = self.0.upgrade();
                if opt.is_none() {
                    kwarn!("depecated");
                    panic!("Empty Key!");
                }
                return opt.unwrap().0.lock().name.cmp(other.1.as_ref().unwrap());
            } else {
                let opt = other.0.upgrade();
                if opt.is_none() {
                    kwarn!("depecated");
                    panic!("Empty Key!");
                }

                return self.1.as_ref().unwrap().cmp(&opt.unwrap().0.lock().name);
            }
        }
    }
}
