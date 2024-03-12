use alloc::vec::Vec;

pub mod console;
pub mod kthread;
pub mod termios;
pub mod tty_core;
pub mod tty_device;
pub mod tty_driver;
pub mod tty_job_control;
pub mod tty_ldisc;
pub mod tty_port;
pub mod virtual_terminal;

// 下列结构体暂时放在这
/// 键盘/显示器"（Keyboard/Display）模式
#[allow(dead_code)]
#[derive(Debug, PartialEq, Clone)]
pub enum KDMode {
    KdText,
    KdGraphics,
    KdText0,
    KdText1,
    Undefined,
}

impl Default for KDMode {
    fn default() -> Self {
        Self::Undefined
    }
}

#[derive(Debug, Default, Clone)]
pub struct ConsoleFont {
    pub width: u32,
    pub height: u32,
    pub count: u32,
    pub data: Vec<u8>,
}
