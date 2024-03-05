use crate::{sched::SchedArch, time::TimeArch};

use super::{driver::tsc::TSCManager, syscall::init_syscall_64, CurrentSchedArch, CurrentTimeArch};

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

#[no_mangle]
unsafe extern "C" fn rs_init_current_core_sched() {
    CurrentSchedArch::initial_setup_sched_local();
}
