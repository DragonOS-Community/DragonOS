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
    pub fault_handler: fn(&dyn ProbeArgs),
    pub symbol: &'a str,
    pub offset: usize,
}

extern "C" {
    fn addr_from_symbol(symbol: *const u8) -> usize;
}

/// 注册一个kprobe
pub fn register_kprobe(kprobe: KprobeInfo) -> Result<Arc<Kprobe>, SystemError> {
    let mut symbol_sting = kprobe.symbol.to_string();
    if !symbol_sting.ends_with("\0") {
        symbol_sting.push('\0');
    }
    let symbol = symbol_sting.as_ptr();
    let func_addr = unsafe { addr_from_symbol(symbol) };
    if func_addr == 0 {
        warn!("register_kprobe: the symbol: {} not found", kprobe.symbol);
        return Err(SystemError::ENXIO);
    }
    let kprobe = KprobeBuilder::new()
        .symbol(kprobe.symbol.to_string())
        .symbol_addr(func_addr)
        .offset(kprobe.offset)
        .pre_handler(kprobe.pre_handler)
        .post_handler(kprobe.post_handler)
        .fault_handler(kprobe.fault_handler)
        .build()
        .install();
    let kprobe = Arc::new(kprobe);
    BREAK_KPROBE_LIST.lock().insert(func_addr, kprobe.clone());
    let debug_address = kprobe.debug_address();
    DEBUG_KPROBE_LIST
        .lock()
        .insert(debug_address, kprobe.clone());
    Ok(kprobe)
}

/// 注销一个kprobe
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
