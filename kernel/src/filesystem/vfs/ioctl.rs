#![allow(dead_code)]
// pub const TCGETS: usize = 0x5401;   // 获取终端属性
// pub const TCSETS: usize = 0x5402;   // 设置终端属性
// pub const TIOCGPGRP: usize = 0x540F;   // 获取前台进程组ID
// pub const TIOCSPGRP: usize = 0x5410;   // 设置前台进程组ID
// pub const TIOCGWINSZ: usize = 0x5413;   // 获取终端窗口大小
// pub const TIOCSWINSZ: usize = 0x5414;   // 设置终端窗口大小
// pub const FIONCLEX: usize = 0x5450;   // 关闭文件描述符的关闭-on-exec标志
// pub const FIOCLEX: usize = 0x5451;   // 设置文件描述符的关闭-on-exec标志
// pub const FIONBIO: usize = 0x5421;   // 设置文件描述符的非阻塞模式
// pub const GETWINSZ: u32 = 1;
// pub const SETWINSZ: u32 = 2;
// pub const ENABLEECHO: u32 = 3;
// pub const DISABLEECHO: u32 = 4;
pub enum IoctlCmd {
    GETWINSZ = 0,
    SETWINSZ = 1,
    ENABLEECHO = 2,
    DISABLEECHO = 3,
}


impl From<u32> for IoctlCmd {
    fn from(raw: u32) -> Self {
        match raw {
            0 => IoctlCmd::GETWINSZ,
            1 => IoctlCmd::SETWINSZ,
            2 => IoctlCmd::ENABLEECHO,
            3 => IoctlCmd::DISABLEECHO,
            _ => panic!("unknown ioctl cmd"),
        }
    }
}
impl Into<u32> for IoctlCmd {

    fn into(self) -> u32 {
        self as u32
    }
}
