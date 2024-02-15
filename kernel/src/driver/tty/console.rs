use alloc::string::String;
use system_error::SystemError;

use super::virtual_terminal::virtual_console::{
    CursorOperation, ScrollDir, VirtualConsoleData, VirtualConsoleIntensity,
};

/// 终端切换相关的回调
pub trait ConsoleSwitch: Sync + Send {
    /// 初始化console
    fn con_startup(&self) -> Result<String, SystemError>;
    fn con_init(&self, vc_data: &mut VirtualConsoleData, init: bool) -> Result<(), SystemError>;
    fn con_deinit(&self) -> Result<(), SystemError>;
    fn con_clear(
        &self,
        vc_data: &mut VirtualConsoleData,
        sy: usize,
        sx: usize,
        height: usize,
        width: usize,
    ) -> Result<(), SystemError>;
    fn con_putc(
        &self,
        vc_data: &VirtualConsoleData,
        ch: u16,
        ypos: u32,
        xpos: u32,
    ) -> Result<(), SystemError>;
    fn con_putcs(
        &self,
        vc_data: &VirtualConsoleData,
        buf: &[u16],
        count: usize,
        ypos: u32,
        xpos: u32,
    ) -> Result<(), SystemError>;

    /// ## 根据pos计算出对应xy
    ///
    /// ### 返回值： （下一行的起始偏移,x，y）
    fn con_getxy(
        &self,
        _vc_data: &VirtualConsoleData,
        _pos: usize,
    ) -> Result<(usize, usize, usize), SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn con_cursor(&self, vc_data: &VirtualConsoleData, op: CursorOperation);

    fn con_build_attr(
        &self,
        _vc_data: &VirtualConsoleData,
        _color: u8,
        _intensity: VirtualConsoleIntensity,
        _blink: bool,
        _underline: bool,
        _reverse: bool,
        _italic: bool,
    ) -> Result<u8, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn con_set_palette(
        &self,
        vc_data: &VirtualConsoleData,
        color_table: &[u8],
    ) -> Result<(), SystemError>;

    /// ## 滚动
    /// ### 参数
    /// - top：滚动范围顶部
    /// - bottom： 滚动范围底部
    /// - dir： 滚动方向
    /// - nr： 滚动行数
    fn con_scroll(
        &self,
        vc_data: &mut VirtualConsoleData,
        top: usize,
        bottom: usize,
        dir: ScrollDir,
        nr: usize,
    ) -> bool;
}
