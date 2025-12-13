// SPDX-License-Identifier: GPL-2.0
//! Loop 设备模块
//!
//! 该模块实现了 Linux 风格的 loop 设备，允许将普通文件作为块设备使用。
//!
//! # 模块结构
//!
//! - `constants`: 常量和枚举类型定义
//! - `loop_device`: Loop 设备实现
//! - `loop_control`: Loop-control 控制设备实现
//! - `manager`: Loop 设备管理器
//! - `driver`: Loop 设备驱动

mod constants;
mod driver;
mod loop_control;
#[allow(clippy::module_inception)]
mod loop_device;
mod manager;

use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    driver::base::device::device_register, filesystem::devfs::devfs_register,
    init::initcall::INITCALL_DEVICE,
};
use unified_init::macros::unified_init;

// 重新导出公共接口
pub use constants::LOOP_CONTROL_BASENAME;
pub use driver::LoopDeviceDriver;
pub use loop_control::LoopControlDevice;
pub use loop_device::{LoopDevice, LoopPrivateData};
pub use manager::LoopManager;

/// 初始化 loop 设备子系统
#[unified_init(INITCALL_DEVICE)]
pub fn loop_init() -> Result<(), SystemError> {
    let loop_mgr = Arc::new(LoopManager::new());
    let driver = LoopDeviceDriver::new();
    let loop_ctl = LoopControlDevice::new(loop_mgr.clone());

    device_register(loop_ctl.clone())?;
    log::info!("Loop control device registered.");
    devfs_register(LOOP_CONTROL_BASENAME, loop_ctl.clone())?;
    log::info!("Loop control device initialized.");
    loop_mgr.loop_init(driver)?;
    Ok(())
}
