//use core::marker::PhantomData;

use crate::filesystem::vfs::ROOT_INODE;
use crate::kdebug;
//use crate::kdebug;
use crate::syscall::SystemError;
use crate::{filesystem, kerror, libs::rwlock::RwLock};
use lazy_static::lazy_static;

#[allow(dead_code)]
pub const NUM_SCAN_CODES: u8 = 0x80;
#[allow(dead_code)]
pub const MAP_COLS: u8 = 2;

#[allow(dead_code)]
pub const FLAG_BREAK: u8 = 0x80; // 用于判断按键是否被按下

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

#[allow(dead_code)]
#[no_mangle]
/// for test
pub extern "C" fn keyboard_handle(input:u8){
    //kdebug!("WHAT IS THE PROBELM?\n");
    let mut fsm=TypeOneFSM::new();
    fsm.parse(input).parse(input);
}

/// @brief A FSM to parse type one keyboard scan code
#[derive(Debug)]
#[allow(dead_code)]
struct TypeOneFSM {
    state: ScanCodeStatus,
    current_state: TypeOneFSMState,
}

impl TypeOneFSM {
    #[allow(dead_code)]
    fn new() -> Self {
        Self {
            state: ScanCodeStatus::new(),
            current_state: TypeOneFSMState::Start,
        }
    }

    /// @brief 解析扫描码
    #[allow(dead_code)]
    fn parse(&mut self, scancode: u8)->TypeOneFSMState {
        self.current_state = self.current_state.parse(scancode);
        self.current_state
    }
}

/// @brief 第一类扫描码状态机的状态
#[derive(Debug,Copy,Clone)]
enum TypeOneFSMState {
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
    fn parse(&self, scancode: u8) -> TypeOneFSMState {
        kdebug!("the code is {:#x}\n",scancode);
        match self {
            TypeOneFSMState::Start => {
                
                return self.handle_start(scancode);
            }
            TypeOneFSMState::PauseBreak(n) => {
                return self.handle_pause_break(*n);
            }
            TypeOneFSMState::Func0 => {
                return self.handle_func0(scancode);
            }
            TypeOneFSMState::Type3 => {
                return self.handle_type3(scancode);
            }
            TypeOneFSMState::PrtscPress(n) => return self.handle_prtsc_press(*n),
            TypeOneFSMState::PrtscRelease(n) => return self.handle_prtsc_release(*n),
        }
    }

    /// @brief 处理起始状态
    fn handle_start(&self, scancode: u8) -> TypeOneFSMState {
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
                return TypeOneFSMState::Type3;
            }
        }
    }

    /// @brief 处理PauseBreak状态
    fn handle_pause_break(&self, scancode: u8) -> TypeOneFSMState {
        static PAUSE_BREAK_SCAN_CODE: [u8; 6] = [0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5];
        let i = match self {
            TypeOneFSMState::PauseBreak(i) => *i,
            _ => {
                return self.handle_type3(scancode);
            }
        };
        if scancode != PAUSE_BREAK_SCAN_CODE[i as usize] {
            return self.handle_type3(scancode);
        } else {
            if i == 5 {
                // 所有Pause Break扫描码都被清除
                return TypeOneFSMState::Start;
            } else {
                return TypeOneFSMState::PauseBreak(i + 1);
            }
        }
    }


    fn handle_func0(&self, scancode: u8) -> TypeOneFSMState {
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
                SCAN_CODE_STATUS.write().ctrl_r = true;
            }
            0x9d => {
                // 松开右边的ctrl
                SCAN_CODE_STATUS.write().ctrl_r = false;
                // flag_make = false;
            }
            0x38 => {
                // 按下右边的alt
                SCAN_CODE_STATUS.write().alt_r = true;

            }
            0xb8 => {
                // 松开右边的alt
                SCAN_CODE_STATUS.write().alt_r = false;
                // flag_make = false;
            }
            0x5b => {
                SCAN_CODE_STATUS.write().gui_l = true;
                // flag_make = true;
            }
            0xdb => {
                SCAN_CODE_STATUS.write().gui_l = false;
                // flag_make = false;
            }
            0x5c => {
                SCAN_CODE_STATUS.write().gui_r = true;
                // flag_make = true;
            }
            0xdc => {
                SCAN_CODE_STATUS.write().gui_r = false;
                // flag_make = false;
            }
            0x5d => {
                SCAN_CODE_STATUS.write().apps = true;
                // flag_make = true;
            }
            0xdd => {
                SCAN_CODE_STATUS.write().apps = false;
                // flag_make = false;
            }
            0x52 => {
                SCAN_CODE_STATUS.write().insert = true;
                // flag_make = true;
            }
            0xd2 => {
                SCAN_CODE_STATUS.write().insert = false;
                // flag_make = false;
            }
            0x47 => {
                SCAN_CODE_STATUS.write().home = true;
                // flag_make = true;
            }
            0xc7 => {
                SCAN_CODE_STATUS.write().home = false;
                // flag_make = false;
            }
            0x49 => {
                SCAN_CODE_STATUS.write().pgup = true;
                // flag_make = true;
            }
            0xc9 => {
                SCAN_CODE_STATUS.write().pgup = false;
                // flag_make = false;
            }
            0x53 => {
                SCAN_CODE_STATUS.write().del = true;
                // flag_make = true;
            }
            0xd3 => {
                SCAN_CODE_STATUS.write().del = false;
                // flag_make = false;
            }
            0x4f => {
                SCAN_CODE_STATUS.write().end = true;
                // flag_make = true;
            }
            0xcf => {
                SCAN_CODE_STATUS.write().end = false;
                // flag_make = false;
            }
            0x51 => {
                SCAN_CODE_STATUS.write().pgdn = true;
                // flag_make = true;
            }
            0xd1 => {
                SCAN_CODE_STATUS.write().pgdn = false;
                // flag_make = false;
            }
            0x48 => {
                SCAN_CODE_STATUS.write().arrow_u = true;
                // flag_make = true;
            }
            0xc8 => {
                SCAN_CODE_STATUS.write().arrow_u = false;
                // flag_make = false;
                let ch = 0xc8;
                Self::emit(ch);
                // return (0, 0xc8);
            }
            0x4b => {
                SCAN_CODE_STATUS.write().arrow_l = true;
                // flag_make = true;
            }
            0xcb => {
                SCAN_CODE_STATUS.write().arrow_l = false;
                // flag_make = false;
            }
            0x50 => {
                SCAN_CODE_STATUS.write().arrow_d = true;
                let ch = 0x50;
                Self::emit(ch);
            }
            0xd0 => {
                SCAN_CODE_STATUS.write().arrow_d = false;
                // flag_make = false;
            }
            0x4d => {
                SCAN_CODE_STATUS.write().arrow_r = true;
            }
            0xcd => {
                SCAN_CODE_STATUS.write().arrow_r = false;
            }

            0x35 => {
                // 数字小键盘的 / 符号
                SCAN_CODE_STATUS.write().kp_forward_slash = true;
                // flag_make = true;
                let ch = '/' as u8;
                Self::emit(ch);
            }
            0xb5 => {
                SCAN_CODE_STATUS.write().kp_forward_slash = false;

            }
            0x1c => {
                SCAN_CODE_STATUS.write().kp_enter = true;
                let ch = '\n' as u8;
                Self::emit(ch);
            }
            0x9c => {
                SCAN_CODE_STATUS.write().kp_enter = false;
            }
            _ => {
                return TypeOneFSMState::Start;
            }
        }
        return TypeOneFSMState::Start;
        
    }

    fn handle_type3(&self, scancode: u8) -> TypeOneFSMState {
        // 判断按键是被按下还是抬起
        let flag_make = if (scancode & (FLAG_BREAK as u8)) > 0 {
            false
        } else {
            true
        };

        // 计算扫描码位于码表的第几行
        let mut col: usize = 0;
        let index = scancode & 0x7f;

        // shift被按下
        if SCAN_CODE_STATUS.read().shift_l || SCAN_CODE_STATUS.read().shift_r {
            col = 1;
        }

        let ch = KEY_CODE_MAPTABLE[col + 2 * index as usize];
        //kdebug!("in type3 ch is {:#x}\n",ch);
        let mut key = KeyFlag::OtherKey; // 可视字符

        match index {
            0x2a => {
                SCAN_CODE_STATUS.write().shift_l = flag_make;
                key = KeyFlag::NoneFlag;
            }
            0x36 => {
                SCAN_CODE_STATUS.write().shift_r = flag_make;
                key = KeyFlag::NoneFlag;
            }
            0x1d => {
                SCAN_CODE_STATUS.write().ctrl_l = flag_make;
                key = KeyFlag::NoneFlag;
            }
            0x38 => {
                SCAN_CODE_STATUS.write().ctrl_r = flag_make;
                key = KeyFlag::NoneFlag;
            }
            _ => {
                if flag_make == false {
                    //kdebug!("in type3 ch is {:#x}\n",ch);
                    key = KeyFlag::NoneFlag;
                }
            }

        }
        if key!=KeyFlag::NoneFlag{

            Self::emit(ch);
        }
        return TypeOneFSMState::Start;
    }

    fn emit(_ch:u8){
        //kdebug!("the character is {}",_ch);
    }

    /// @brief 处理Prtsc按下事件
    fn handle_prtsc_press(&self, scancode: u8) -> TypeOneFSMState {
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
            return self.handle_type3(scancode);
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

    fn handle_prtsc_release(&self, scancode: u8)->TypeOneFSMState{
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
            return self.handle_type3(scancode);
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
        }
    }
}

lazy_static! {
    static ref SCAN_CODE_STATUS: RwLock<ScanCodeStatus> = RwLock::new(ScanCodeStatus::new());
}

#[allow(dead_code)]
const KEY_CODE_MAPTABLE: [u8; 256] = [
    /*0x00*/ 0, 0, /*0x01*/ 0, 0, // ESC
    /*0x02*/ '1' as u8, '!' as u8, /*0x03*/ '2' as u8, '@' as u8,
    /*0x04*/ '3' as u8, '#' as u8, /*0x05*/ '4' as u8, '$' as u8,
    /*0x06*/ '5' as u8, '%' as u8, /*0x07*/ '6' as u8, '^' as u8,
    /*0x08*/ '7' as u8, '&' as u8, /*0x09*/ '8' as u8, '*' as u8,
    /*0x0a*/ '9' as u8, '(' as u8, /*0x0b*/ '0' as u8, ')' as u8,
    /*0x0c*/ '-' as u8, '_' as u8, /*0x0d*/ '=' as u8, '+' as u8,
    /*0x0e*/ 0x0e as u8, 0x0e as u8, // BACKSPACE
    /*0x0f*/ '\t' as u8, '\t' as u8, // TAB
    /*0x10*/ 'q' as u8, 'Q' as u8, /*0x11*/ 'w' as u8, 'W' as u8,
    /*0x12*/ 'e' as u8, 'E' as u8, /*0x13*/ 'r' as u8, 'R' as u8,
    /*0x14*/ 't' as u8, 'T' as u8, /*0x15*/ 'y' as u8, 'Y' as u8,
    /*0x16*/ 'u' as u8, 'U' as u8, /*0x17*/ 'i' as u8, 'I' as u8,
    /*0x18*/ 'o' as u8, 'O' as u8, /*0x19*/ 'p' as u8, 'P' as u8,
    /*0x1a*/ '[' as u8, '{' as u8, /*0x1b*/ ']' as u8, '}' as u8,
    /*0x1c*/ '\n' as u8, '\n' as u8, // ENTER
    /*0x1d*/ 0x1d, 0x1d, // CTRL Left
    /*0x1e*/ 'a' as u8, 'A' as u8, /*0x1f*/ 's' as u8, 'S' as u8,
    /*0x20*/ 'd' as u8, 'D' as u8, /*0x21*/ 'f' as u8, 'F' as u8,
    /*0x22*/ 'g' as u8, 'G' as u8, /*0x23*/ 'h' as u8, 'H' as u8,
    /*0x24*/ 'j' as u8, 'J' as u8, /*0x25*/ 'k' as u8, 'K' as u8,
    /*0x26*/ 'l' as u8, 'L' as u8, /*0x27*/ ';' as u8, ':' as u8,
    /*0x28*/ '\'' as u8, '"' as u8, /*0x29*/ '`' as u8, '~' as u8, /*0x2a*/ 0x2a,
    0x2a, // SHIFT Left
    /*0x2b*/ '\\' as u8, '|' as u8, /*0x2c*/ 'z' as u8, 'Z' as u8,
    /*0x2d*/ 'x' as u8, 'X' as u8, /*0x2e*/ 'c' as u8, 'C' as u8,
    /*0x2f*/ 'v' as u8, 'V' as u8, /*0x30*/ 'b' as u8, 'B' as u8,
    /*0x31*/ 'n' as u8, 'N' as u8, /*0x32*/ 'm' as u8, 'M' as u8,
    /*0x33*/ ',' as u8, '<' as u8, /*0x34*/ '.' as u8, '>' as u8,
    /*0x35*/ '/' as u8, '?' as u8, /*0x36*/ 0x36, 0x36, // SHIFT Right
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

/// @brief 键盘扫描码有三种：
///
///     0xE1开头的PauseBreak键;   
///     0xE0开头的功能键;   
///     1byte的普通按键;   
///
/// @return (ch, key_flag)
///
///     ch 如果是功能符，返回 255
///        如果是可显示字符，则返回 按下/抬起 的字符的ASCII码
///     key_flag key的类型，目前只有printScreen和pausebreak两个功能键，其他都是otherkey
#[allow(dead_code)]
pub fn keyboard_get_keycode() -> Result<(u8, KeyFlag), SystemError> {
    static PAUSE_BREAK_SCAN_CODE: [u8; 6] = [0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5];
    let mut ch: u8 = u8::MAX;
    let mut key = KeyFlag::NoneFlag;
    let flag_make; // (按下/抬起)
    let c = keyboard_get_scancode()?;

    let mut scancode = c as u8;

    if scancode == 0xE1 {
        // Pause Break
        key = KeyFlag::PauseBreak;
        // 清除缓冲区中剩下的扫描码
        for i in 1..6 {
            if keyboard_get_scancode()? != PAUSE_BREAK_SCAN_CODE[i] as i32 {
                break;
            }
        }
    } else if scancode == 0xE0 {
        // 功能键, 有多个扫描码
        scancode = keyboard_get_scancode()? as u8;

        match scancode {
            0x2a => {
                // print screen 按键被按下
                if let Ok(0xe0) = keyboard_get_scancode() {
                    if let Ok(0x37) = keyboard_get_scancode() {
                        key = KeyFlag::PrintScreenPress;
                        // flag_make = Release;
                    }
                }
            }

            0xb7 => {
                // print screen 按键被松开
                if let Ok(0xe0) = keyboard_get_scancode() {
                    if let Ok(0xaa) = keyboard_get_scancode() {
                        key = KeyFlag::PrintScreenRelease;
                        // flag_make = false;
                    }
                }
            }
            0x1d => {
                // 按下右边的ctrl
                SCAN_CODE_STATUS.write().ctrl_r = true;
                key = KeyFlag::OtherKey;
                // flag_make = true;
            }
            0x9d => {
                // 松开右边的ctrl
                SCAN_CODE_STATUS.write().ctrl_r = false;
                // flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x38 => {
                // 按下右边的alt
                SCAN_CODE_STATUS.write().alt_r = true;
                key = KeyFlag::OtherKey;
                // flag_make = true;
            }
            0xb8 => {
                // 松开右边的alt
                SCAN_CODE_STATUS.write().alt_r = false;
                // flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x5b => {
                SCAN_CODE_STATUS.write().gui_l = true;
                key = KeyFlag::OtherKey;
                // flag_make = true;
            }
            0xdb => {
                SCAN_CODE_STATUS.write().gui_l = false;
                // flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x5c => {
                SCAN_CODE_STATUS.write().gui_r = true;
                key = KeyFlag::OtherKey;
                // flag_make = true;
            }
            0xdc => {
                SCAN_CODE_STATUS.write().gui_r = false;
                // flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x5d => {
                SCAN_CODE_STATUS.write().apps = true;
                key = KeyFlag::OtherKey;
                // flag_make = true;
            }
            0xdd => {
                SCAN_CODE_STATUS.write().apps = false;
                // flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x52 => {
                SCAN_CODE_STATUS.write().insert = true;
                key = KeyFlag::OtherKey;
                // flag_make = true;
            }
            0xd2 => {
                SCAN_CODE_STATUS.write().insert = false;
                // flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x47 => {
                SCAN_CODE_STATUS.write().home = true;
                key = KeyFlag::OtherKey;
                // flag_make = true;
            }
            0xc7 => {
                SCAN_CODE_STATUS.write().home = false;
                // flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x49 => {
                SCAN_CODE_STATUS.write().pgup = true;
                key = KeyFlag::OtherKey;
                // flag_make = true;
            }
            0xc9 => {
                SCAN_CODE_STATUS.write().pgup = false;
                // flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x53 => {
                SCAN_CODE_STATUS.write().del = true;
                key = KeyFlag::OtherKey;
                // flag_make = true;
            }
            0xd3 => {
                SCAN_CODE_STATUS.write().del = false;
                // flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x4f => {
                SCAN_CODE_STATUS.write().end = true;
                key = KeyFlag::OtherKey;
                // flag_make = true;
            }
            0xcf => {
                SCAN_CODE_STATUS.write().end = false;
                // flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x51 => {
                SCAN_CODE_STATUS.write().pgdn = true;
                key = KeyFlag::OtherKey;
                // flag_make = true;
            }
            0xd1 => {
                SCAN_CODE_STATUS.write().pgdn = false;
                // flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x48 => {
                SCAN_CODE_STATUS.write().arrow_u = true;
                key = KeyFlag::OtherKey;
                // flag_make = true;
            }
            0xc8 => {
                SCAN_CODE_STATUS.write().arrow_u = false;
                // flag_make = false;
                key = KeyFlag::OtherKey;
                ch = 0xc8;
                // return (0, 0xc8);
            }
            0x4b => {
                SCAN_CODE_STATUS.write().arrow_l = true;
                key = KeyFlag::OtherKey;
                // flag_make = true;
            }
            0xcb => {
                SCAN_CODE_STATUS.write().arrow_l = false;
                // flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x50 => {
                SCAN_CODE_STATUS.write().arrow_d = true;
                key = KeyFlag::OtherKey;
                // flag_make = true;
                ch = 0x50;
                // return (0, 0x50);
            }
            0xd0 => {
                SCAN_CODE_STATUS.write().arrow_d = false;
                // flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x4d => {
                SCAN_CODE_STATUS.write().arrow_r = true;
                key = KeyFlag::OtherKey;
                // flag_make = true;
            }
            0xcd => {
                SCAN_CODE_STATUS.write().arrow_r = false;
                // flag_make = false;
                key = KeyFlag::OtherKey;
            }

            0x35 => {
                // 数字小键盘的 / 符号
                SCAN_CODE_STATUS.write().kp_forward_slash = true;
                key = KeyFlag::OtherKey;
                // flag_make = true;
                ch = '/' as u8;
            }
            0xb5 => {
                SCAN_CODE_STATUS.write().kp_forward_slash = false;
                // flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x1c => {
                SCAN_CODE_STATUS.write().kp_enter = true;
                ch = '\n' as u8;
                // flag_make = true;
                key = KeyFlag::OtherKey;
            }
            0x9c => {
                SCAN_CODE_STATUS.write().kp_enter = false;
                // flag_make = false;
                key = KeyFlag::OtherKey;
            }

            _ => {
                key = KeyFlag::OtherKey;
            }
        }
    }

    if key == KeyFlag::NoneFlag
    // 属于第三类扫描码
    {
        // 判断按键是被按下还是抬起
        flag_make = if (scancode & (FLAG_BREAK as u8)) > 0 {
            false
        } else {
            true
        };

        // 计算扫描码位于码表的第几行
        let mut col: usize = 0;
        let index = scancode & 0x7f;

        // shift被按下
        if SCAN_CODE_STATUS.read().shift_l || SCAN_CODE_STATUS.read().shift_r {
            col = 1;
        }

        ch = KEY_CODE_MAPTABLE[col + 2 * index as usize];
        key = KeyFlag::OtherKey; // 可视字符

        match index {
            0x2a => {
                SCAN_CODE_STATUS.write().shift_l = flag_make;
                key = KeyFlag::NoneFlag;
            }
            0x36 => {
                SCAN_CODE_STATUS.write().shift_r = flag_make;
                key = KeyFlag::NoneFlag;
            }
            0x1d => {
                SCAN_CODE_STATUS.write().ctrl_l = flag_make;
                key = KeyFlag::NoneFlag;
            }
            0x38 => {
                SCAN_CODE_STATUS.write().ctrl_r = flag_make;
                key = KeyFlag::NoneFlag;
            }
            _ => {
                if flag_make == false {
                    key = KeyFlag::NoneFlag;
                }
            }
        }
    }

    return Ok((ch, key));
}

/// @brief 从键盘设备文件中获取键盘扫描码
fn keyboard_get_scancode() -> Result<i32, SystemError> {
    // 如果后面对性能有要求，可以把这个inode存下来当作static变量
    let keyboard = ROOT_INODE().lookup("/dev/char/ps2_keyboard");

    if keyboard.is_err() {
        kerror!("Failed in finding ps2_keyboard ");
        return Err(SystemError::ENXIO);
    }

    let keyboard = keyboard.unwrap();

    let mut buf = [0 as u8];
    if keyboard
        .read_at(
            0,
            1,
            &mut buf,
            &mut filesystem::vfs::FilePrivateData::Unused,
        )
        .is_err()
    {
        kerror!("Read Ps2_keyboard Error");
        return Err(SystemError::EIO);
    }
    return Ok(buf[0] as i32);
}




