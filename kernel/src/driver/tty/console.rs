use system_error::SystemError;

use super::virtual_terminal::virtual_console::{
    CursorOperation, ScrollDir, VirtualConsoleData, VirtualConsoleIntensity,
};

/// 终端切换相关的回调
pub trait ConsoleSwitch: Sync + Send {
    /// 初始化，会对vc_data进行一系列初始化操作
    fn con_init(&self, vc_data: &mut VirtualConsoleData, init: bool) -> Result<(), SystemError>;

    /// 进行释放等系列操作，目前未使用
    fn con_deinit(&self) -> Result<(), SystemError>;

    /// ## 清空console的一片区域
    /// 该函数的所有参数对应的都是以字符为单位
    /// ### 参数：
    /// - vc_data: 对应的ConsoleData
    /// - sy: 对应区域左上角的y轴
    /// - sx: 对应区域左上角的x轴
    /// - height: 区域高度
    /// - width: 区域宽度
    fn con_clear(
        &self,
        vc_data: &mut VirtualConsoleData,
        sy: usize,
        sx: usize,
        height: usize,
        width: usize,
    ) -> Result<(), SystemError>;

    /// ## 向console输出一个字符
    /// ### 参数：
    /// - vc_data: 对应的ConsoleData
    /// - ch: 数据
    /// - ypos: 起始y坐标
    /// - xpos: 起始x坐标
    fn con_putc(
        &self,
        vc_data: &VirtualConsoleData,
        ch: u16,
        ypos: u32,
        xpos: u32,
    ) -> Result<(), SystemError>;

    /// ## 向console输出一串字符
    /// ### 参数：
    /// - vc_data: 对应的ConsoleData
    /// - buf: 数据
    /// - count: 输出字符数量
    /// - ypos: 起始y坐标
    /// - xpos: 起始x坐标
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

    /// ## 对光标进行操作
    /// ### 参数：
    /// - vc_data: 对应的ConsoleData
    /// - op: 对光标的操作
    fn con_cursor(&self, vc_data: &VirtualConsoleData, op: CursorOperation);

    /// ## 根据参数构建出对应的属性
    /// ### 参数：
    /// - vc_data: 对应的ConsoleData
    /// - color: 颜色
    /// - intensity: 字符强度
    /// - blink: 是否闪烁
    /// - underline: 下划线
    /// - reverse: 颜色反转
    /// - italic: 斜体
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

    /// ## 设置调色板
    /// ### 参数：
    /// - vc_data: 对应的ConsoleData
    /// - color_table: 颜色表
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
