use crate::libs::spinlock::SpinLock;
use alloc::collections::BTreeMap;
use alloc::string::ToString;
use alloc::sync::Arc;
use kprobe::{Kprobe, KprobeBuilder, KprobeOps, ProbeArgs};
use log::warn;
use system_error::SystemError;

mod test;

pub static BREAK_KPROBE_LIST: SpinLock<BTreeMap<usize, Arc<Kprobe>>> =
    SpinLock::new(BTreeMap::new());
pub static DEBUG_KPROBE_LIST: SpinLock<BTreeMap<usize, Arc<Kprobe>>> =
    SpinLock::new(BTreeMap::new());

pub fn kprobe_init() {}

pub struct KprobeInfo<'a> {
    pub pre_handler: fn(&dyn ProbeArgs),
    pub post_handler: fn(&dyn ProbeArgs),
    pub fault_handler: Option<fn(&dyn ProbeArgs)>,
    pub symbol: &'a str,
    pub offset: usize,
}

extern "C" {
    fn addr_from_symbol(symbol: *const u8) -> usize;
}

/// # 注册一个kprobe
///
/// 该函数会根据`symbol`查找对应的函数地址，如果找不到则返回错误。
///
/// ## 参数
/// - `kprobe_info`: kprobe的信息
pub fn register_kprobe(kprobe_info: KprobeInfo) -> Result<Arc<Kprobe>, SystemError> {
    let mut symbol_sting = kprobe_info.symbol.to_string();
    if !symbol_sting.ends_with("\0") {
        symbol_sting.push('\0');
    }
    let symbol = symbol_sting.as_ptr();
    let func_addr = unsafe { addr_from_symbol(symbol) };
    if func_addr == 0 {
        warn!(
            "register_kprobe: the symbol: {} not found",
            kprobe_info.symbol
        );
        return Err(SystemError::ENXIO);
    }
    let mut kprobe_builder = KprobeBuilder::new(
        kprobe_info.symbol.to_string(),
        func_addr,
        kprobe_info.offset,
        kprobe_info.pre_handler,
        kprobe_info.post_handler,
    );
    if kprobe_info.fault_handler.is_some() {
        kprobe_builder = kprobe_builder.fault_handler(kprobe_info.fault_handler.unwrap());
    }
    let kprobe = kprobe_builder.build().install();

    let kprobe = Arc::new(kprobe);
    let kprobe_addr = kprobe.kprobe_address();
    BREAK_KPROBE_LIST.lock().insert(kprobe_addr, kprobe.clone());
    let debug_address = kprobe.debug_address();
    DEBUG_KPROBE_LIST
        .lock()
        .insert(debug_address, kprobe.clone());
    Ok(kprobe)
}

/// # 注销一个kprobe
///
/// ## 参数
/// - `kprobe`: 已安装的kprobe
pub fn unregister_kprobe(kprobe: Arc<Kprobe>) -> Result<(), SystemError> {
    let debug_address = kprobe.debug_address();
    let kprobe_addr = kprobe.kprobe_address();
    BREAK_KPROBE_LIST.lock().remove(&kprobe_addr);
    DEBUG_KPROBE_LIST.lock().remove(&debug_address);
    Ok(())
}

#[cfg(feature = "kprobe_test")]
pub fn kprobe_test() {
    test::kprobe_test();
}
