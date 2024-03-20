use super::tty_ldisc::LineDisciplineType;

/// ## 窗口大小
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowSize {
    /// 行
    pub row: u16,
    /// 列
    pub col: u16,
    /// x方向像素数
    pub xpixel: u16,
    /// y方向像素数
    pub ypixel: u16,
}

#[derive(Debug, Clone, Copy)]
pub struct Termios {
    pub input_mode: InputMode,
    pub output_mode: OutputMode,
    pub control_mode: ControlMode,
    pub local_mode: LocalMode,
    pub control_characters: [u8; CONTORL_CHARACTER_NUM],
    pub line: LineDisciplineType,
    pub input_speed: u32,
    pub output_speed: u32,
}

#[derive(Clone, Copy, Default)]
pub struct PosixTermios {
    pub c_iflag: u32,
    pub c_oflag: u32,
    pub c_cflag: u32,
    pub c_lflag: u32,
    pub c_cc: [u8; CONTORL_CHARACTER_NUM],
    pub c_line: u8,
    pub c_ispeed: u32,
    pub c_ospeed: u32,
}

impl PosixTermios {
    pub fn from_kernel_termios(termios: Termios) -> Self {
        Self {
            c_iflag: termios.input_mode.bits,
            c_oflag: termios.output_mode.bits,
            c_cflag: termios.control_mode.bits,
            c_lflag: termios.local_mode.bits,
            c_cc: termios.control_characters.clone(),
            c_line: termios.line as u8,
            c_ispeed: termios.input_speed,
            c_ospeed: termios.output_speed,
        }
    }

    #[allow(dead_code)]
    pub fn to_kernel_termios(&self) -> Termios {
        // TODO：这里没有考虑非规范模式
        Termios {
            input_mode: InputMode::from_bits_truncate(self.c_iflag),
            output_mode: OutputMode::from_bits_truncate(self.c_oflag),
            control_mode: ControlMode::from_bits_truncate(self.c_cflag),
            local_mode: LocalMode::from_bits_truncate(self.c_lflag),
            control_characters: self.c_cc.clone(),
            line: LineDisciplineType::from_line(self.c_line),
            input_speed: self.c_ispeed,
            output_speed: self.c_ospeed,
        }
    }
}

pub const INIT_CONTORL_CHARACTERS: [u8; CONTORL_CHARACTER_NUM] = [
    b'C' - 0x40,  // VINTR
    b'\\' - 0x40, // VQUIT
    0o177,        // VERASE
    b'U' - 0x40,  // VKILL
    b'D' - 0x40,  // VEOF
    1,            // VMIN
    0,            // VEOL
    0,            // VTIME
    0,            // VEOL2
    0,            // VSWTC
    b'W' - 0x40,  // VWERASE
    b'R' - 0x40,  // VREPRINT
    b'Z' - 0x40,  // VSUSP
    b'Q' - 0x40,  // VSTART
    b'S' - 0x40,  // VSTOP
    b'V' - 0x40,  // VLNEXT
    b'O' - 0x40,  // VDISCARD
    0,
    0,
];

// pub const INIT_CONTORL_CHARACTERS: [u8; CONTORL_CHARACTER_NUM] = {
//     let mut chs: [u8; CONTORL_CHARACTER_NUM] = Default::default();
//     chs[ControlCharIndex::VINTR] = b'C' - 0x40;
//     chs[ControlCharIndex::VQUIT] = b'\\' - 0x40;
//     chs[ControlCharIndex::VERASE] = 0o177;
//     chs[ControlCharIndex::VKILL] = b'U' - 0x40;
//     chs[ControlCharIndex::VEOF] = b'D' - 0x40;
//     chs[ControlCharIndex::VSTART] = b'Q' - 0x40;
//     chs[ControlCharIndex::VSTOP] = b'S' - 0x40;
//     chs[ControlCharIndex::VSUSP] = b'Z' - 0x40;
//     chs[ControlCharIndex::VREPRINT] = b'R' - 0x40;
//     chs[ControlCharIndex::VDISCARD] = b'O' - 0x40;
//     chs[ControlCharIndex::VWERASE] = b'W' - 0x40;
//     chs[ControlCharIndex::VLNEXT] = b'V' - 0x40;
//     // chs[ContorlCharIndex::VDSUSP] = b'Y'  - 0x40;
//     chs[ControlCharIndex::VMIN] = 1;
//     return chs;
// };

lazy_static! {
    pub static ref TTY_STD_TERMIOS: Termios = {
        Termios {
            input_mode: InputMode::ICRNL | InputMode::IXON,
            output_mode: OutputMode::OPOST | OutputMode::ONLCR,
            control_mode: ControlMode::B38400
                | ControlMode::CREAD
                | ControlMode::HUPCL
                | ControlMode::CS8,
            local_mode: LocalMode::ISIG
                | LocalMode::ICANON
                | LocalMode::ECHO
                | LocalMode::ECHOE
                | LocalMode::ECHOK
                | LocalMode::ECHOCTL
                | LocalMode::ECHOKE
                | LocalMode::IEXTEN,
            control_characters: INIT_CONTORL_CHARACTERS.clone(),
            line: LineDisciplineType::NTty,
            input_speed: 38400,
            output_speed: 38400,
        }
    };
}

pub const CONTORL_CHARACTER_NUM: usize = 19;

bitflags! {
    /// termios输入特性
    pub struct InputMode: u32 {
        /// 如果设置了该标志，表示启用软件流控制。
        const IXON = 0x0400;
        /// 如果设置了该标志，表示启用输入流控制。
        const IXOFF = 0x1000;
        /// Map Uppercase to Lowercase on Input 将大写转换为小写
        /// 表示不区分大小写
        const IUCLC = 0x0200;
        /// 如果设置了该标志，表示当输入队列满时，产生一个响铃信号。
        const IMAXBEL = 0x2000;
        /// 如果设置了该标志，表示输入数据被视为 UTF-8 编码。
        const IUTF8 = 0x4000;

        /// 忽略中断信号
        const IGNBRK	= 0x001;
        /// 检测到中断信号时生成中断（产生中断信号）
        const BRKINT	= 0x002;
        /// 忽略具有奇偶校验错误的字符
        const IGNPAR	= 0x004;
        /// 在检测到奇偶校验错误或帧错误时，将字符以 \377 标记
        const PARMRK	= 0x008;
        /// 启用输入奇偶校验检查
        const INPCK	= 0x010;
        /// 从输入字符中剥离第 8 位，即只保留低 7 位
        const ISTRIP	= 0x020;
        /// 表示将输入的换行符 (\n) 映射为回车符 (\r)
        const INLCR	= 0x040;
        /// 表示忽略回车符 (\r)
        const IGNCR	= 0x080;
        /// 表示将输入的回车符 (\r) 映射为换行符 (\n)
        const ICRNL	= 0x100;
        /// 表示在输入被停止（Ctrl-S）后，任何字符的输入都将重新启动输入
        const IXANY	= 0x800;
    }

    /// termios输出特性
    pub struct OutputMode: u32 {
        /// 在输出时将换行符替换\r\n
        const ONLCR	= 0x00004;
        /// Map Lowercase to Uppercase on Output 输出字符时将小写字母映射为大写字母
        const OLCUC	= 0x00002;

        /// 与NL协同 配置换行符的处理方式
        const NLDLY	= 0x00100;
        const   NL0	= 0x00000;  // 不延迟换行
        const   NL1	= 0x00100;  // 延迟换行（输出回车后等待一段时间再输出换行）

        /// 配置水平制表符的处理方式
        const TABDLY = 0x01800;
        const  TAB0 = 0x00000;  // 不延迟水平制表符
        const  TAB1 = 0x00800;  // 在输出水平制表符时，延迟到下一个设置的水平制表符位置
        const  TAB2 = 0x01000;  // 在输出水平制表符时，延迟到下一个设置的 8 的倍数的位置
        const  TAB3 = 0x01800;  // TAB3 和 XTABS（与 TAB3 等效）保留，暂未使用
        const XTABS = 0x01800;

        /// 配置回车符的处理方式
        const CRDLY	= 0x00600;
        const   CR0	= 0x00000;  // 不延迟回车
        const   CR1	= 0x02000;  //  延迟回车（输出回车后等待一段时间再输出换行）
        const   CR2	= 0x04000;  // CR2 和 CR3保留，暂未使用
        const   CR3	= 0x06000;

        /// 配置换页符（form feed）的处理方式
        const FFDLY	= 0x08000;
        const   FF0	= 0x00000;  // 不延迟换页
        const   FF1	= 0x08000;  // 延迟换页

        /// 配置退格符（backspace）的处理方式
        const BSDLY	= 0x02000;
        const   BS0	= 0x00000;  // 不延迟退格
        const   BS1	= 0x02000;  // 延迟退格

        /// 配置垂直制表符（vertical tab）的处理方式
        const VTDLY	= 0x04000;
        const   VT0	= 0x00000;  // 不延迟垂直制表符
        const   VT1	= 0x04000;  // 延迟垂直制表符

        /// 表示执行输出处理，即启用输出处理函数
        const OPOST	= 0x01;
        /// 表示将输出的回车符 (\r) 映射为换行符 (\n)
        const OCRNL	= 0x08;
        /// 表示在输出时，如果光标在第 0 列，则不输出回车符 (\r)
        const ONOCR	= 0x10;
        /// 表示将回车符 (\r) 映射为换行符 (\n)
        const ONLRET	= 0x20;
        /// 表示使用填充字符进行延迟。这个填充字符的默认值是空格。
        const OFILL	= 0x40;
        /// 表示使用删除字符 (DEL, \177) 作为填充字符
        const OFDEL	= 0x80;
    }

    /// 配置终端设备的基本特性和控制参数
    pub struct ControlMode: u32 {
        /// Baud Rate Mask 指定波特率的掩码
        const CBAUD		= 0x0000100f;
        /// Extra Baud Bits 指定更高的波特率位
        const CBAUDEX	= 0x00001000;
        /// Custom Baud Rate 指定自定义波特率 如果设置了 BOTHER，则通过以下位来设置自定义的波特率值
        const BOTHER	= 0x00001000;

        /* Common CBAUD rates */
        const     B0	= 0x00000000;	/* hang up */
        const    B50	= 0x00000001;
        const    B75	= 0x00000002;
        const   B110	= 0x00000003;
        const   B134	= 0x00000004;
        const   B150	= 0x00000005;
        const   B200	= 0x00000006;
        const   B300	= 0x00000007;
        const   B600	= 0x00000008;
        const  B1200	= 0x00000009;
        const  B1800	= 0x0000000a;
        const  B2400	= 0x0000000b;
        const  B4800	= 0x0000000c;
        const  B9600	= 0x0000000d;
        const B19200	= 0x0000000e;
        const B38400	= 0x0000000f;

        const     B57600 = 0x00001001;
        const    B115200 = 0x00001002;
        const    B230400 = 0x00001003;
        const    B460800 = 0x00001004;
        const    B500000 = 0x00001005;
        const    B576000 = 0x00001006;
        const    B921600 = 0x00001007;
        const   B1000000 = 0x00001008;
        const   B1152000 = 0x00001009;
        const   B1500000 = 0x0000100a;
        const   B2000000 = 0x0000100b;
        const   B2500000 = 0x0000100c;
        const   B3000000 = 0x0000100d;
        const   B3500000 = 0x0000100e;
        const   B4000000 = 0x0000100f;

        /// 指定字符大小的掩码 以下位为特定字符大小
        const CSIZE		= 0x00000030;
        const   CS5		= 0x00000000;
        const   CS6		= 0x00000010;
        const   CS7		= 0x00000020;
        const   CS8		= 0x00000030;

        /// Stop Bit Select 表示使用两个停止位；否则，表示使用一个停止位
        const CSTOPB	= 0x00000040;
        /// 表示启用接收器。如果未设置，则禁用接收器。
        const CREAD		= 0x00000080;
        /// 表示启用奇偶校验。如果未设置，则禁用奇偶校验。
        const PARENB	= 0x00000100;
        /// 表示启用奇校验。如果未设置，则表示启用偶校验。
        const PARODD	= 0x00000200;
        /// 表示在终端设备被关闭时挂断线路（执行挂断操作）
        const HUPCL		= 0x00000400;
        /// 表示忽略调制解调器的状态（DCD、DSR、CTS 等）
        const CLOCAL	= 0x00000800;
        /// 指定输入波特率的掩码
        const CIBAUD	= 0x100f0000;

        const ADDRB = 0x20000000;
    }

    /// 配置终端设备的本地模式（local mode）或控制输入处理的行为
    pub struct LocalMode: u32 {
        /// 启用中断字符（Ctrl-C、Ctrl-Z）
        const ISIG	 = 0x00001;
        /// 表示启用规范模式，即启用行缓冲和回显。在规范模式下，输入被缓冲，并且只有在输入回车符时才会传递给应用程序。
        const ICANON = 0x00002;
        /// 表示启用大写模式，即输入输出都将被转换为大写。
        const XCASE	 = 0x00004;
        /// 表示启用回显（显示用户输入的字符）
        const ECHO	 = 0x00008;
        /// 表示在回显时将擦除的字符用 backspace 和空格字符显示。
        const ECHOE	 = 0x00010;
        /// 表示在回显时将换行符后的字符用空格字符显示。
        const ECHOK	 = 0x00020;
        /// 表示在回显时将换行符显示为换行和回车符。
        const ECHONL = 0x00040;
        /// 表示在收到中断（Ctrl-C）和退出（Ctrl-\）字符后，不清空输入和输出缓冲区。
        const NOFLSH = 0x00080;
        /// 表示在后台进程尝试写入终端时，发送停止信号（Ctrl-S）
        const TOSTOP = 0x00100;
        /// 表示在回显时，显示控制字符为 ^ 加字符。
        const ECHOCTL= 0x00200;
        /// 表示在回显时显示带有 # 的换行符（为了与 echo -n 命令兼容）。
        const ECHOPRT= 0x00400;
        /// 表示在回显时将 KILL 字符（Ctrl-U）用空格字符显示。
        const ECHOKE = 0x00800;
        /// 表示输出正在被冲刷（flush），通常是由于输入/输出流的状态变化。
        const FLUSHO = 0x01000;
        /// 表示在规范模式下，存在需要重新打印的字符。
        const PENDIN = 0x04000;
        /// 表示启用实现定义的输入处理。
        const IEXTEN = 0x08000;
        /// 表示启用扩展的处理函数
        const EXTPROC= 0x10000;
    }

    pub struct TtySetTermiosOpt: u8 {
        const TERMIOS_FLUSH	=1;
        const TERMIOS_WAIT	=2;
        const TERMIOS_TERMIO	=4;
        const TERMIOS_OLD	=8;
    }
}

/// 对应termios中控制字符的索引
pub struct ControlCharIndex;
#[allow(dead_code)]
impl ControlCharIndex {
    pub const DISABLE_CHAR: u8 = b'\0';
    /// 中断信号
    pub const VINTR: usize = 0;
    /// 退出信号
    pub const VQUIT: usize = 1;
    /// 退格
    pub const VERASE: usize = 2;
    /// 终止输入信号
    pub const VKILL: usize = 3;
    /// 文件结束信号 \0?
    pub const VEOF: usize = 4;
    /// 指定非规范模式下的最小字符数
    pub const VMIN: usize = 5;
    /// 换行符
    pub const VEOL: usize = 6;
    /// 指定非规范模式下的超时时间
    pub const VTIME: usize = 7;
    /// 换行符
    pub const VEOL2: usize = 8;
    /// 未使用，保留
    pub const VSWTC: usize = 9;
    /// 擦除前一个单词
    pub const VWERASE: usize = 10;
    /// 重新打印整行
    pub const VREPRINT: usize = 11;
    /// 挂起信号
    pub const VSUSP: usize = 12;
    /// 启动输出信号
    pub const VSTART: usize = 13;
    /// 停止输出信号
    pub const VSTOP: usize = 14;
    /// 将下一个字符视为字面值，而不是特殊字符
    pub const VLNEXT: usize = 15;
    /// 对应于字符丢弃信号，用于丢弃当前输入的行
    pub const VDISCARD: usize = 16;
}
