use crate::debug::kprobe::args::KprobeInfo;
use crate::libs::rwlock::RwLock;
use crate::libs::spinlock::SpinLock;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use kprobe::{Kprobe, KprobeBuilder, KprobeOps, KprobePoint};
use system_error::SystemError;

pub mod args;
#[cfg(feature = "kprobe_test")]
mod test;

pub type LockKprobe = Arc<RwLock<Kprobe>>;
pub static KPROBE_MANAGER: SpinLock<KprobeManager> = SpinLock::new(KprobeManager::new());
static KPROBE_POINT_LIST: SpinLock<BTreeMap<usize, Arc<KprobePoint>>> =
    SpinLock::new(BTreeMap::new());

/// 管理所有的kprobe探测点
#[derive(Debug, Default)]
pub struct KprobeManager {
    break_list: BTreeMap<usize, Vec<LockKprobe>>,
    debug_list: BTreeMap<usize, Vec<LockKprobe>>,
}

impl KprobeManager {
    pub const fn new() -> Self {
        KprobeManager {
            break_list: BTreeMap::new(),
            debug_list: BTreeMap::new(),
        }
    }
    /// # 插入一个kprobe
    ///
    /// ## 参数
    /// - `kprobe`: kprobe的实例
    pub fn insert_kprobe(&mut self, kprobe: LockKprobe) {
        let probe_point = kprobe.read().probe_point().clone();
        self.insert_break_point(probe_point.break_address(), kprobe.clone());
        self.insert_debug_point(probe_point.debug_address(), kprobe);
    }

    /// # 向break_list中插入一个kprobe
    ///
    /// ## 参数
    /// - `address`: kprobe的地址, 由`KprobePoint::break_address()`或者`KprobeBuilder::probe_addr()`返回
    /// - `kprobe`: kprobe的实例
    fn insert_break_point(&mut self, address: usize, kprobe: LockKprobe) {
        let list = self.break_list.entry(address).or_default();
        list.push(kprobe);
    }

    /// # 向debug_list中插入一个kprobe
    ///
    /// ## 参数
    /// - `address`: kprobe的单步执行地址，由`KprobePoint::debug_address()`返回
    /// - `kprobe`: kprobe的实例
    fn insert_debug_point(&mut self, address: usize, kprobe: LockKprobe) {
        let list = self.debug_list.entry(address).or_default();
        list.push(kprobe);
    }

    pub fn get_break_list(&self, address: usize) -> Option<&Vec<LockKprobe>> {
        self.break_list.get(&address)
    }

    pub fn get_debug_list(&self, address: usize) -> Option<&Vec<LockKprobe>> {
        self.debug_list.get(&address)
    }

    /// # 返回一个地址上注册的kprobe数量
    ///
    /// ## 参数
    /// - `address`: kprobe的地址, 由`KprobePoint::break_address()`或者`KprobeBuilder::probe_addr()`返回
    pub fn kprobe_num(&self, address: usize) -> usize {
        self.break_list_len(address)
    }

    #[inline]
    fn break_list_len(&self, address: usize) -> usize {
        self.break_list
            .get(&address)
            .map(|list| list.len())
            .unwrap_or(0)
    }
    #[inline]
    fn debug_list_len(&self, address: usize) -> usize {
        self.debug_list
            .get(&address)
            .map(|list| list.len())
            .unwrap_or(0)
    }

    /// # 移除一个kprobe
    ///
    /// ## 参数
    /// - `kprobe`: kprobe的实例
    pub fn remove_kprobe(&mut self, kprobe: &LockKprobe) {
        let probe_point = kprobe.read().probe_point().clone();
        self.remove_one_break(probe_point.break_address(), kprobe);
        self.remove_one_debug(probe_point.debug_address(), kprobe);
    }

    /// # 从break_list中移除一个kprobe
    ///
    /// 如果没有其他kprobe注册在这个地址上，则删除列表
    ///
    /// ## 参数
    /// - `address`: kprobe的地址, 由`KprobePoint::break_address()`或者`KprobeBuilder::probe_addr()`返回
    /// - `kprobe`: kprobe的实例
    fn remove_one_break(&mut self, address: usize, kprobe: &LockKprobe) {
        if let Some(list) = self.break_list.get_mut(&address) {
            list.retain(|x| !Arc::ptr_eq(x, kprobe));
        }
        if self.break_list_len(address) == 0 {
            self.break_list.remove(&address);
        }
    }

    /// # 从debug_list中移除一个kprobe
    ///
    /// 如果没有其他kprobe注册在这个地址上，则删除列表
    ///
    /// ## 参数
    /// - `address`: kprobe的单步执行地址，由`KprobePoint::debug_address()`返回
    /// - `kprobe`: kprobe的实例
    fn remove_one_debug(&mut self, address: usize, kprobe: &LockKprobe) {
        if let Some(list) = self.debug_list.get_mut(&address) {
            list.retain(|x| !Arc::ptr_eq(x, kprobe));
        }
        if self.debug_list_len(address) == 0 {
            self.debug_list.remove(&address);
        }
    }
}

#[cfg(feature = "kprobe_test")]
#[allow(unused)]
/// This function is only used for testing kprobe
pub fn kprobe_test() {
    test::kprobe_test();
}

/// # 注册一个kprobe
///
/// 该函数会根据`symbol`查找对应的函数地址，如果找不到则返回错误。
///
/// ## 参数
/// - `kprobe_info`: kprobe的信息
pub fn register_kprobe(kprobe_info: KprobeInfo) -> Result<LockKprobe, SystemError> {
    let kprobe_builder = KprobeBuilder::try_from(kprobe_info)?;
    let address = kprobe_builder.probe_addr();
    let existed_point = KPROBE_POINT_LIST.lock().get(&address).map(Clone::clone);
    let kprobe = match existed_point {
        Some(existed_point) => {
            kprobe_builder
                .with_probe_point(existed_point.clone())
                .install()
                .0
        }
        None => {
            let (kprobe, probe_point) = kprobe_builder.install();
            KPROBE_POINT_LIST.lock().insert(address, probe_point);
            kprobe
        }
    };
    let kprobe = Arc::new(RwLock::new(kprobe));
    KPROBE_MANAGER.lock().insert_kprobe(kprobe.clone());
    Ok(kprobe)
}

/// # 注销一个kprobe
///
/// ## 参数
/// - `kprobe`: 已安装的kprobe
pub fn unregister_kprobe(kprobe: LockKprobe) {
    let kprobe_addr = kprobe.read().probe_point().break_address();
    KPROBE_MANAGER.lock().remove_kprobe(&kprobe);
    // 如果没有其他kprobe注册在这个地址上，则删除探测点
    if KPROBE_MANAGER.lock().kprobe_num(kprobe_addr) == 0 {
        KPROBE_POINT_LIST.lock().remove(&kprobe_addr);
    }
}
