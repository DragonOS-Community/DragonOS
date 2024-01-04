use system_error::SystemError;

use crate::driver::video::fbdev::vesafb::vesafb_early_init;

/// 在内存管理初始化之前，初始化视频驱动（架构相关）
pub fn arch_video_early_init() -> Result<(), SystemError> {
    vesafb_early_init()?;
    return Ok(());
}
