/// 特殊控制字符
#[derive(Clone, Copy, Debug)]
pub enum CC {
    // 文件结束字符
    VEOF = 0,
    // 换行字符
    VEOL = 1,
    // 第二换行字符
    VEOL2 = 2,
    // 删除字符
    VERASE = 3,
    // 单词删除字符
    VWERASE = 4,
    //  Kill字符
    VKILL = 5,
    // 重打印字符
    VREPRINT = 6,
    // 切换字符模式字符
    VSWTC = 7,
    // 中断字符
    VINTR = 8,
    // 退出字符
    VQUIT = 9,
    // 挂起字符
    VSUSP = 10,
    // 开始字符
    VSTART = 12,
    // 停止字符
    VSTOP = 13,
    // 下一行字符
    VLNEXT = 14,
    // 废弃字符
    VDISCARD = 15,
    // 最小字符输入
    VMIN = 16,
    // 时间字符输入
    VTIME = 17,
}
bitflags! {
    pub struct IFlag : u32 {
        const IXON = 0x0200;      // 启用输入时的XON/XOFF流控制
        const IXOFF = 0x0400;     // 关闭输入时的XON/XOFF流控制
        const IUCLC = 0x1000;     // 将输入的大写字母转换为小写字母
        const IMAXBEL = 0x2000;   // 当输入缓冲区溢出时，产生响铃音
        const IUTF8 = 0x4000;     // 输入为UTF-8字符
    }
    pub struct OFlag : u32 {
        const ONLCR = 0x00002;    // 输出时将换行符转换为回车换行
        const OLCUC = 0x00004;    // 输出时将小写字母转换为大写字母
        const NLDLY = 0x00300;    // 换行延迟掩码
        const NL0 = 0x00000;      // 换行延迟为0
        const NL1 = 0x00100;      // 换行延迟为1
        const NL2 = 0x00200;      // 换行延迟为2
        const NL3 = 0x00300;      // 换行延迟为3
        const TABDLY = 0x00c00;   // 制表符延迟掩码
        const TAB0 = 0x00000;     // 制表符延迟为0
        const TAB1 = 0x00400;     // 制表符延迟为1
        const TAB2 = 0x00800;     // 制表符延迟为2
        const TAB3 = 0x00c00;     // 制表符延迟为3
        const CRDLY = 0x03000;    // 回车延迟掩码
        const CR0 = 0x00000;      // 回车延迟为0
        const CR1 = 0x01000;      // 回车延迟为1
        const CR2 = 0x02000;      // 回车延迟为2
        const CR3 = 0x03000;      // 回车延迟为3
        const FFDLY = 0x04000;    // 换页延迟掩码
        const FF0 = 0x00000;      // 换页延迟为0
        const FF1 = 0x04000;      // 换页延迟为1
        const BSDLY = 0x08000;    // 退格延迟掩码
        const BS0 = 0x00000;      // 退格延迟为0
        const BS1 = 0x08000;      // 退格延迟为1
        const VTDLY = 0x10000;    // 垂直制表延迟掩码
        const VT0 = 0x00000;      // 垂直制表延迟为0
        const VT1 = 0x10000;      // 垂直制表延迟为1
    }
    pub struct CFlag : u32 {
        const CBAUD = 0x0000001f;    // 传输速率掩码
        const CBAUDEX = 0x00000000;  // 扩展传输速率掩码
        const BOTHER = 0x0000001f;   // 其他传输速率掩码
        const B57600 = 0x00000010;   // 57600 bps
        const B115200 = 0x00000011;  // 115200 bps
        const B230400 = 0x00000012;  // 230400 bps
        const B460800 = 0x00000013;  // 460800 bps
        const B500000 = 0x00000014;  // 500000 bps
        const B576000 = 0x00000015;  // 576000 bps
        const B921600 = 0x00000016;  // 921600 bps
        const B1000000 = 0x00000017; // 1000000 bps
        const B1152000 = 0x00000018; // 1152000 bps
        const B1500000 = 0x00000019; // 1500000 bps
        const B2000000 = 0x0000001a; // 2000000 bps
        const B2500000 = 0x0000001b; // 2500000 bps
        const B3000000 = 0x0000001c; // 3000000 bps
        const B3500000 = 0x0000001d; // 3500000 bps
        const B4000000 = 0x0000001e; // 4000000 bps
        const CSIZE = 0x00000300;    // 字符大小掩码
        const CS5 = 0x00000000;      // 5位字符大小
        const CS6 = 0x00000100;      // 6位字符大小
        const CS7 = 0x00000200;      // 7位字符大小
        const CS8 = 0x00000300;      // 8位字符大小
        const CSTOPB = 0x00000400;   // 设置两个停止位
        const CREAD = 0x00000800;    // 启用接收器
        const PARENB = 0x00001000;   // 启用奇偶校验
        const PARODD = 0x00002000;   // 使用奇校验而不是偶校验
        const HUPCL = 0x00004000;    // 关闭时挂起线路
        const CLOCAL = 0x00008000;   // 忽略调制解调器线路状态
        const CIBAUD = 0x001f0000;   // 输入波特率掩码
    }

    pub struct Lflag : u32 {
        const ISIG = 0o000001;     // 接收信号
        const ICANON = 0o000002;   // 规范模式
        const XCASE = 0o000004;    // 当输入中有大写字母时，将其转换为小写字母
        const ECHO = 0o000010;     // 回显输入
        const ECHOE = 0o000020;    // 擦除字符时回显特殊字符
        const ECHOK = 0o000040;    // 擦除整行时回显特殊字符
        const ECHONL = 0o000100;   // 在回显时将换行符转换为回车-换行序列
        const NOFLSH = 0o000200;   // 禁止刷新输出队列
        const TOSTOP = 0o000400;   // 向后台进程发送SIGTTOU信号以停止输出
        const ECHOCTL = 0o001000;  // 在回显时显示控制字符
        const ECHOPRT = 0o002000;  // 在回显时显示打印字符
        const ECHOKE = 0o004000;   // 在回显时擦除整行
        const FLUSHO = 0o010000;   // 输出时刷新队列
        const PENDIN = 0o040000;   // 有未读取的输入数据
        const IEXTEN = 0o100000;   // 启用输入处理扩展
        const EXTPROC = 0o200000;  // 启用外部处理
    }
}
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Termios {
    // 输入模式标志、输出模式标志、控制模式标志和本地模式标志。
    pub iflag: IFlag,
    pub oflag: OFlag,
    pub cflag: CFlag,
    pub lflag: Lflag,
    // 表示行规程(== c_cc[32])
    pub line: CC,
    // 用于存储特殊控制字符
    pub cc: [CC; 32],
    // 输入波特率和输出波特率
    pub ispeed: u32,
    pub ospeed: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Winsize {
    row: u16,    // 每行有多少字符
    col: u16,    // 每列有多少字符
    xpixel: u16, // 每行有多少像素
    ypixel: u16, // 每列有多少像素
}
impl Termios {
    pub fn new(
        iflag: IFlag,
        oflag: OFlag,
        cflag: CFlag,
        lflag: Lflag,
        ispeed: u32,
        ospeed: u32,
    ) -> Self {
        Self {
            iflag,
            oflag,
            cflag,
            lflag,
            line: CC::VEOF,
            cc: [CC::VEOF; 32],
            ispeed,
            ospeed,
        }
    }
}
impl Default for Termios {
    fn default() -> Self {
        Termios::new(
            IFlag::IMAXBEL | IFlag::IUTF8 | IFlag::IXON,
            OFlag::ONLCR,
            CFlag::CS8 | CFlag::CREAD | CFlag::CSTOPB,
            Lflag::ISIG | Lflag::ICANON | Lflag::ECHO | Lflag::ECHOE | Lflag::ECHOCTL,
            0,
            0,
        )
    }
}
impl Winsize {
    pub fn new(row: u16, col: u16, xpixel: u16, ypixel: u16) -> Self {
        Self {
            row,
            col,
            xpixel,
            ypixel,
        }
    }
}
