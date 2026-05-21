use core::hint::spin_loop;

/// # 功能
///
/// 执行系统重启操作。该函数会尝试使用不同的方法来重启系统，直到成功为止。
pub(crate) fn machine_restart(_cmd: Option<&str>) -> ! {
    todo!();
}

/// # 功能
///
/// 执行系统停止操作
pub(crate) fn machine_halt() -> ! {
    todo!();
}

/// # Functionality
///
/// Perform system power off operation.
pub(crate) fn machine_power_off() -> ! {
    log::warn!("riscv64 machine_power_off is not implemented, spin here.");
    loop {
        spin_loop();
    }
}

pub(crate) fn migrate_to_reboot_cpu() {}
