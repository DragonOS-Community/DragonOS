use crate::time::TimeArch;

use super::{
    driver::tsc::TSCManager, setup::setup_arch, syscall::init_syscall_64, CurrentTimeArch,
};

#[no_mangle]
unsafe extern "C" fn rs_setup_arch() -> i32 {
    return setup_arch()
        .map(|_| 0)
        .unwrap_or_else(|e| e.to_posix_errno());
}

/// 获取当前的时间戳
#[no_mangle]
unsafe extern "C" fn rs_get_cycles() -> u64 {
    return CurrentTimeArch::get_cycles() as u64;
}

#[no_mangle]
unsafe extern "C" fn rs_tsc_get_cpu_khz() -> u64 {
    return TSCManager::cpu_khz();
}

/// syscall指令初始化
#[no_mangle]
pub unsafe extern "C" fn rs_init_syscall_64() {
    init_syscall_64();
}
