use crate::driver::tty::kthread::send_to_tty_refresh_thread;

#[allow(dead_code)]
pub const NUM_SCAN_CODES: u8 = 0x80;
#[allow(dead_code)]
pub const TYPE1_KEYCODE_MAP_TABLE_COLS: u8 = 2;

#[allow(dead_code)]
pub const TYPE1_KEYCODE_FLAG_BREAK: u8 = 0x80; // 用于判断按键是否被按下

/// 标志状态
#[repr(u8)]
#[derive(Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum KeyFlag {
    NoneFlag = 0 as u8,
    PauseBreak = 1 as u8,
    PrintScreenPress = 2 as u8,
    PrintScreenRelease = 4 as u8,
    OtherKey = 8 as u8, // 除了上面两个按键以外的功能按键（不包括下面的第三类按键）
}

/// @brief A FSM to parse type one keyboard scan code
#[derive(Debug)]
#[allow(dead_code)]
pub struct TypeOneFSM {
    status: ScanCodeStatus,
    current_state: TypeOneFSMState,
}

impl TypeOneFSM {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            status: ScanCodeStatus::new(),
            current_state: TypeOneFSMState::Start,
        }
    }

    /// @brief 解析扫描码
    #[allow(dead_code)]
    pub fn parse(&mut self, scancode: u8) -> TypeOneFSMState {
        self.current_state = self.current_state.parse(scancode, &mut self.status);
        self.current_state
    }
}

/// @brief 第一类扫描码状态机的状态
#[derive(Debug, Copy, Clone)]
pub enum TypeOneFSMState {
    /// 起始状态
    Start,
    /// PauseBreak 第n个扫描码
    PauseBreak(u8),
    /// 多扫描码功能键起始状态
    Func0,
    /// 第三类扫描码或字符
    Type3,

    PrtscPress(u8),
    PrtscRelease(u8),
}

impl TypeOneFSMState {
    /// @brief 状态机总控程序
    fn parse(&self, scancode: u8, scancode_status: &mut ScanCodeStatus) -> TypeOneFSMState {
        // kdebug!("the code is {:#x}\n", scancode);
        match self {
            TypeOneFSMState::Start => {
                return self.handle_start(scancode, scancode_status);
            }
            TypeOneFSMState::PauseBreak(n) => {
                return self.handle_pause_break(*n, scancode_status);
            }
            TypeOneFSMState::Func0 => {
                return self.handle_func0(scancode, scancode_status);
            }
            TypeOneFSMState::Type3 => {
                return self.handle_type3(scancode, scancode_status);
            }
            TypeOneFSMState::PrtscPress(n) => return self.handle_prtsc_press(*n, scancode_status),
            TypeOneFSMState::PrtscRelease(n) => {
                return self.handle_prtsc_release(*n, scancode_status)
            }
        }
    }

    /// @brief 处理起始状态
    fn handle_start(&self, scancode: u8, scancode_status: &mut ScanCodeStatus) -> TypeOneFSMState {
        //kdebug!("in handle_start the code is {:#x}\n",scancode);
        match scancode {
            0xe1 => {
                return TypeOneFSMState::PauseBreak(1);
            }
            0xe0 => {
                return TypeOneFSMState::Func0;
            }
            _ => {
                //kdebug!("in _d the code is {:#x}\n",scancode);
                return TypeOneFSMState::Type3.handle_type3(scancode, scancode_status);
            }
        }
    }

    /// @brief 处理PauseBreak状态
    fn handle_pause_break(
        &self,
        scancode: u8,
        scancode_status: &mut ScanCodeStatus,
    ) -> TypeOneFSMState {
        static PAUSE_BREAK_SCAN_CODE: [u8; 6] = [0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5];
        let i = match self {
            TypeOneFSMState::PauseBreak(i) => *i,
            _ => {
                return self.handle_type3(scancode, scancode_status);
            }
        };
        if scancode != PAUSE_BREAK_SCAN_CODE[i as usize] {
            return self.handle_type3(scancode, scancode_status);
        } else {
            if i == 5 {
                // 所有Pause Break扫描码都被清除
                return TypeOneFSMState::Start;
            } else {
                return TypeOneFSMState::PauseBreak(i + 1);
            }
        }
    }

    fn handle_func0(&self, scancode: u8, scancode_status: &mut ScanCodeStatus) -> TypeOneFSMState {
        //0xE0
        match scancode {
            0x2a => {
                return TypeOneFSMState::PrtscPress(2);
            }
            0xb7 => {
                return TypeOneFSMState::PrtscRelease(2);
            }
            0x1d => {
                // 按下右边的ctrl
                scancode_status.ctrl_r = true;
            }
            0x9d => {
                // 松开右边的ctrl
                scancode_status.ctrl_r = false;
            }
            0x38 => {
                // 按下右边的alt
                scancode_status.alt_r = true;
            }
            0xb8 => {
                // 松开右边的alt
                scancode_status.alt_r = false;
            }
            0x5b => {
                scancode_status.gui_l = true;
            }
            0xdb => {
                scancode_status.gui_l = false;
            }
            0x5c => {
                scancode_status.gui_r = true;
            }
            0xdc => {
                scancode_status.gui_r = false;
            }
            0x5d => {
                scancode_status.apps = true;
            }
            0xdd => {
                scancode_status.apps = false;
            }
            0x52 => {
                scancode_status.insert = true;
            }
            0xd2 => {
                scancode_status.insert = false;
            }
            0x47 => {
                scancode_status.home = true;
            }
            0xc7 => {
                scancode_status.home = false;
            }
            0x49 => {
                scancode_status.pgup = true;
            }
            0xc9 => {
                scancode_status.pgup = false;
            }
            0x53 => {
                scancode_status.del = true;
                Self::emit(127);
            }
            0xd3 => {
                scancode_status.del = false;
            }
            0x4f => {
                scancode_status.end = true;
            }
            0xcf => {
                scancode_status.end = false;
            }
            0x51 => {
                scancode_status.pgdn = true;
            }
            0xd1 => {
                scancode_status.pgdn = false;
            }
            0x48 => {
                scancode_status.arrow_u = true;
                Self::emit(224);
                Self::emit(72);
            }
            0xc8 => {
                scancode_status.arrow_u = false;
            }
            0x4b => {
                scancode_status.arrow_l = true;
                Self::emit(224);
                Self::emit(75);
            }
            0xcb => {
                scancode_status.arrow_l = false;
            }
            0x50 => {
                scancode_status.arrow_d = true;
                Self::emit(224);
                Self::emit(80);
            }
            0xd0 => {
                scancode_status.arrow_d = false;
            }
            0x4d => {
                scancode_status.arrow_r = true;
                Self::emit(224);
                Self::emit(77);
            }
            0xcd => {
                scancode_status.arrow_r = false;
            }

            0x35 => {
                // 数字小键盘的 / 符号
                scancode_status.kp_forward_slash = true;

                let ch = '/' as u8;
                Self::emit(ch);
            }
            0xb5 => {
                scancode_status.kp_forward_slash = false;
            }
            0x1c => {
                scancode_status.kp_enter = true;
                Self::emit('\n' as u8);
            }
            0x9c => {
                scancode_status.kp_enter = false;
            }
            _ => {
                return TypeOneFSMState::Start;
            }
        }
        return TypeOneFSMState::Start;
    }

    fn handle_type3(&self, scancode: u8, scancode_status: &mut ScanCodeStatus) -> TypeOneFSMState {
        // 判断按键是被按下还是抬起
        let flag_make = if (scancode & (TYPE1_KEYCODE_FLAG_BREAK as u8)) > 0 {
            false //up
        } else {
            true //down
        };

        // 计算扫描码位于码表的第几行
        let mut col: bool = false;
        let index = scancode & 0x7f;

        //kdebug!("in type3 ch is {:#x}\n",ch);
        let mut key = KeyFlag::OtherKey; // 可视字符

        match index {
            0x2a => {
                scancode_status.shift_l = flag_make;
                key = KeyFlag::NoneFlag;
            }
            0x36 => {
                scancode_status.shift_r = flag_make;
                key = KeyFlag::NoneFlag;
            }
            0x1d => {
                scancode_status.ctrl_l = flag_make;
                key = KeyFlag::NoneFlag;
            }
            0x38 => {
                scancode_status.alt_r = flag_make;
                key = KeyFlag::NoneFlag;
            }
            0x3A => {
                if scancode_status.caps_lock {
                    scancode_status.caps_lock = !flag_make;
                }
                //if caps_lock: true, flag_make: true => cap_lock: false
                else {
                    scancode_status.caps_lock = flag_make;
                } //else false => cap_lock: true
                key = KeyFlag::NoneFlag;
            }
            _ => {
                if flag_make == false {
                    // kdebug!("in type3 ch is {:#x}\n",ch);
                    key = KeyFlag::NoneFlag;
                }
            }
        }

        // shift被按下
        if scancode_status.shift_l || scancode_status.shift_r {
            col = true;
        }

        if scancode_status.caps_lock {
            if index >= 0x10 && index <= 0x19 {
                col = !col;
            } else if index >= 0x1e && index <= 0x26 {
                col = !col;
            } else if index >= 0x2c && index <= 0x32 {
                col = !col;
            }
        }

        let mut ch = TYPE1_KEY_CODE_MAPTABLE[col as usize + 2 * index as usize];
        if key != KeyFlag::NoneFlag {
            // kdebug!("EMIT: ch is '{}', keyflag is {:?}\n", ch as char, key);
            if scancode_status.ctrl_l || scancode_status.ctrl_r {
                ch = Self::to_ctrl(ch);
            }
            Self::emit(ch);
        }
        return TypeOneFSMState::Start;
    }

    #[inline]
    fn to_ctrl(ch: u8) -> u8 {
        return match ch as char {
            'a'..='z' => ch - 0x40,
            'A'..='Z' => ch - 0x40,
            '@'..='_' => ch - 0x40,
            _ => ch,
        };
    }

    #[inline(always)]
    fn emit(ch: u8) {
        // 发送到tty
        send_to_tty_refresh_thread(&[ch]);
    }

    /// @brief 处理Prtsc按下事件
    fn handle_prtsc_press(
        &self,
        scancode: u8,
        scancode_status: &mut ScanCodeStatus,
    ) -> TypeOneFSMState {
        static PRTSC_SCAN_CODE: [u8; 4] = [0xe0, 0x2a, 0xe0, 0x37];
        let i = match self {
            TypeOneFSMState::PrtscPress(i) => *i,
            _ => return TypeOneFSMState::Start, // 解析错误，返回起始状态
        };
        if i > 3 {
            // 解析错误，返回起始状态
            return TypeOneFSMState::Start;
        }
        if scancode != PRTSC_SCAN_CODE[i as usize] {
            return self.handle_type3(scancode, scancode_status);
        } else {
            if i == 3 {
                // 成功解析出PrtscPress
                return TypeOneFSMState::Start;
            } else {
                // 继续解析
                return TypeOneFSMState::PrtscPress(i + 1);
            }
        }
    }

    fn handle_prtsc_release(
        &self,
        scancode: u8,
        scancode_status: &mut ScanCodeStatus,
    ) -> TypeOneFSMState {
        static PRTSC_SCAN_CODE: [u8; 4] = [0xe0, 0xb7, 0xe0, 0xaa];
        let i = match self {
            TypeOneFSMState::PrtscRelease(i) => *i,
            _ => return TypeOneFSMState::Start, // 解析错误，返回起始状态
        };
        if i > 3 {
            // 解析错误，返回起始状态
            return TypeOneFSMState::Start;
        }
        if scancode != PRTSC_SCAN_CODE[i as usize] {
            return self.handle_type3(scancode, scancode_status);
        } else {
            if i == 3 {
                // 成功解析出PrtscRelease
                return TypeOneFSMState::Start;
            } else {
                // 继续解析
                return TypeOneFSMState::PrtscRelease(i + 1);
            }
        }
    }
}

/// 按键状态
#[derive(Debug)]
#[allow(dead_code)]
pub struct ScanCodeStatus {
    // Shift 按键
    shift_l: bool,
    shift_r: bool,
    // Ctrl 按键
    ctrl_l: bool,
    ctrl_r: bool,
    // Alt 按键
    alt_l: bool,
    alt_r: bool,
    //
    gui_l: bool,
    gui_r: bool,
    //
    apps: bool,
    insert: bool,
    // page up/down
    pgup: bool,
    pgdn: bool,
    del: bool,
    home: bool,
    end: bool,
    arrow_u: bool,
    arrow_l: bool,
    arrow_d: bool,
    arrow_r: bool,
    // 斜杠
    kp_forward_slash: bool,
    // 回车
    kp_enter: bool,
    caps_lock: bool,
}

impl ScanCodeStatus {
    fn new() -> Self {
        ScanCodeStatus {
            shift_l: false,
            shift_r: false,
            ctrl_l: false,
            ctrl_r: false,
            alt_l: false,
            alt_r: false,
            gui_l: false,
            gui_r: false,
            apps: false,
            insert: false,
            pgup: false,
            pgdn: false,
            del: false,
            home: false,
            end: false,
            arrow_u: false,
            arrow_l: false,
            arrow_d: false,
            arrow_r: false,
            kp_forward_slash: false,
            kp_enter: false,
            caps_lock: false,
        }
    }
}

const TYPE1_KEY_CODE_MAPTABLE: [u8; 256] = [
    /*0x00*/ 0, 0, /*0x01*/ 0, 0, // ESC
    /*0x02*/ '1' as u8, '!' as u8, /*0x03*/ '2' as u8, '@' as u8,
    /*0x04*/ '3' as u8, '#' as u8, /*0x05*/ '4' as u8, '$' as u8,
    /*0x06*/ '5' as u8, '%' as u8, /*0x07*/ '6' as u8, '^' as u8,
    /*0x08*/ '7' as u8, '&' as u8, /*0x09*/ '8' as u8, '*' as u8,
    /*0x0a*/ '9' as u8, '(' as u8, /*0x0b*/ '0' as u8, ')' as u8,
    /*0x0c*/ '-' as u8, '_' as u8, /*0x0d*/ '=' as u8, '+' as u8,
    /*0x0e  \b */ 8 as u8, 8 as u8, // BACKSPACE
    /*0x0f*/ '\t' as u8, '\t' as u8, // TAB
    ////////////////////////character///////////////////////////
    /*0x10*/ 'q' as u8,
    'Q' as u8, /*0x11*/ 'w' as u8, 'W' as u8, /*0x12*/ 'e' as u8, 'E' as u8,
    /*0x13*/ 'r' as u8, 'R' as u8, /*0x14*/ 't' as u8, 'T' as u8,
    /*0x15*/ 'y' as u8, 'Y' as u8, /*0x16*/ 'u' as u8, 'U' as u8,
    /*0x17*/ 'i' as u8, 'I' as u8, /*0x18*/ 'o' as u8, 'O' as u8,
    /*0x19*/ 'p' as u8, 'P' as u8,
    ////////////////////////character///////////////////////////

    /*0x1a*/ '[' as u8,
    '{' as u8, /*0x1b*/ ']' as u8, '}' as u8, /*0x1c*/ '\n' as u8,
    '\n' as u8, // ENTER
    /*0x1d*/ 0x1d, 0x1d, // CTRL Left
    ////////////////////////character///////////////////////////
    /*0x1e*/ 'a' as u8,
    'A' as u8, /*0x1f*/ 's' as u8, 'S' as u8, /*0x20*/ 'd' as u8, 'D' as u8,
    /*0x21*/ 'f' as u8, 'F' as u8, /*0x22*/ 'g' as u8, 'G' as u8,
    /*0x23*/ 'h' as u8, 'H' as u8, /*0x24*/ 'j' as u8, 'J' as u8,
    /*0x25*/ 'k' as u8, 'K' as u8, /*0x26*/ 'l' as u8, 'L' as u8,
    ////////////////////////character///////////////////////////

    /*0x27*/ ';' as u8,
    ':' as u8, /*0x28*/ '\'' as u8, '"' as u8, /*0x29*/ '`' as u8, '~' as u8,
    /*0x2a*/ 0x2a, 0x2a, // SHIFT Left
    /*0x2b*/ '\\' as u8, '|' as u8,
    ////////////////////////character///////////////////////////
    /*0x2c*/ 'z' as u8,
    'Z' as u8, /*0x2d*/ 'x' as u8, 'X' as u8, /*0x2e*/ 'c' as u8, 'C' as u8,
    /*0x2f*/ 'v' as u8, 'V' as u8, /*0x30*/ 'b' as u8, 'B' as u8,
    /*0x31*/ 'n' as u8, 'N' as u8, /*0x32*/ 'm' as u8, 'M' as u8,
    ////////////////////////character///////////////////////////

    /*0x33*/ ',' as u8,
    '<' as u8, /*0x34*/ '.' as u8, '>' as u8, /*0x35*/ '/' as u8, '?' as u8,
    /*0x36*/ 0x36, 0x36, // SHIFT Right
    /*0x37*/ '*' as u8, '*' as u8, /*0x38*/ 0x38, 0x38, // ALT Left
    /*0x39*/ ' ' as u8, ' ' as u8, /*0x3a*/ 0, 0, // CAPS LOCK
    /*0x3b*/ 0, 0, // F1
    /*0x3c*/ 0, 0, // F2
    /*0x3d*/ 0, 0, // F3
    /*0x3e*/ 0, 0, // F4
    /*0x3f*/ 0, 0, // F5
    /*0x40*/ 0, 0, // F6
    /*0x41*/ 0, 0, // F7
    /*0x42*/ 0, 0, // F8
    /*0x43*/ 0, 0, // F9
    /*0x44*/ 0, 0, // F10
    /*0x45*/ 0, 0, // NUM LOCK
    /*0x46*/ 0, 0, // SCROLL LOCK
    /*0x47*/ '7' as u8, 0, /*PAD HONE*/
    /*0x48*/ '8' as u8, 0, /*PAD UP*/
    /*0x49*/ '9' as u8, 0, /*PAD PAGEUP*/
    /*0x4a*/ '-' as u8, 0, /*PAD MINUS*/
    /*0x4b*/ '4' as u8, 0, /*PAD LEFT*/
    /*0x4c*/ '5' as u8, 0, /*PAD MID*/
    /*0x4d*/ '6' as u8, 0, /*PAD RIGHT*/
    /*0x4e*/ '+' as u8, 0, /*PAD PLUS*/
    /*0x4f*/ '1' as u8, 0, /*PAD END*/
    /*0x50*/ '2' as u8, 0, /*PAD DOWN*/
    /*0x51*/ '3' as u8, 0, /*PAD PAGEDOWN*/
    /*0x52*/ '0' as u8, 0, /*PAD INS*/
    /*0x53*/ '.' as u8, 0, /*PAD DOT*/
    /*0x54*/ 0, 0, /*0x55*/ 0, 0, /*0x56*/ 0, 0, /*0x57*/ 0, 0, // F11
    /*0x58*/ 0, 0, // F12
    /*0x59*/ 0, 0, /*0x5a*/ 0, 0, /*0x5b*/ 0, 0, /*0x5c*/ 0, 0,
    /*0x5d*/ 0, 0, /*0x5e*/ 0, 0, /*0x5f*/ 0, 0, /*0x60*/ 0, 0,
    /*0x61*/ 0, 0, /*0x62*/ 0, 0, /*0x63*/ 0, 0, /*0x64*/ 0, 0,
    /*0x65*/ 0, 0, /*0x66*/ 0, 0, /*0x67*/ 0, 0, /*0x68*/ 0, 0,
    /*0x69*/ 0, 0, /*0x6a*/ 0, 0, /*0x6b*/ 0, 0, /*0x6c*/ 0, 0,
    /*0x6d*/ 0, 0, /*0x6e*/ 0, 0, /*0x6f*/ 0, 0, /*0x70*/ 0, 0,
    /*0x71*/ 0, 0, /*0x72*/ 0, 0, /*0x73*/ 0, 0, /*0x74*/ 0, 0,
    /*0x75*/ 0, 0, /*0x76*/ 0, 0, /*0x77*/ 0, 0, /*0x78*/ 0, 0,
    /*0x79*/ 0, 0, /*0x7a*/ 0, 0, /*0x7b*/ 0, 0, /*0x7c*/ 0, 0,
    /*0x7d*/ 0, 0, /*0x7e*/ 0, 0, /*0x7f*/ 0, 0,
];
