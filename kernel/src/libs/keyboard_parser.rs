use crate::filesystem::vfs::ROOT_INODE;
use crate::kdebug;
use crate::{driver::keyboard, filesystem, kerror, libs::rwlock::RwLock};
use lazy_static::lazy_static;

pub const NUM_SCAN_CODES: u8 = 0x80;
pub const MAP_COLS: u8 = 2;

#[repr(u8)]
#[derive(Debug, PartialEq, Eq)]
pub enum KeyFlag {
    NoneFlag = 0 as u8,
    PauseBreak = 1 as u8,
    PrintScreenUp = 2 as u8,
    PrintScreenDown = 4 as u8,
    OtherKey = 8 as u8, // 除了上面两个按键以外的功能按键（不包括下面的第三类按键）
}

pub const FLAG_BREAK: u8 = 0x80; // 用于判断按键是否被按下

/// 按键状态机
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

const PAUSE_BREAK_SCAN_CODE: [u8; 6] = [0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5];

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
pub fn keyboard_get_keycode() -> (u8, KeyFlag) {
    let mut ch: u8 = u8::MAX;
    let mut key = KeyFlag::NoneFlag;
    let mut flag_make; // 按下/抬起
    let c = keyboard_get_scancode();

    // 循环队列为空
    if c.is_err() {
        kdebug!("keyboard find error.");
        return (0, KeyFlag::NoneFlag);
    }

    let c: i32 = c.unwrap();
    let mut scancode = c as u8;

    if scancode == 0xE1 {
        // Pause Break
        key = KeyFlag::PauseBreak;
        // 清除缓冲区中剩下的扫描码
        for i in 1..6 {
            if keyboard_get_scancode() != Ok(PAUSE_BREAK_SCAN_CODE[i] as i32) {
                break;
            }
        }
    } else if scancode == 0xE0 {
        // 功能键, 有多个扫描码
        scancode = match keyboard_get_scancode() {
            // 获取下一个扫描码
            Err(_) => {
                kerror!("keyboard_get_scancode error.");
                return (0, KeyFlag::NoneFlag);
            }
            Ok(code) => code as u8,
        };

        match scancode {
            0x2a => {
                // print screen 按键被按下
                if Ok(0xe0) == keyboard_get_scancode() {
                    if Ok(0x37) == keyboard_get_scancode() {
                        key = KeyFlag::PrintScreenUp;
                        flag_make = true;
                    }
                }
            }

            0xb7 => {
                // print screen 按键被松开
                if keyboard_get_scancode() == Ok(0xe0) {
                    if keyboard_get_scancode() == Ok(0xaa) {
                        key = KeyFlag::PrintScreenDown;
                        flag_make = false;
                    }
                }
            }
            0x1d => {
                // 按下右边的ctrl
                SCAN_CODE_STATUS.write().ctrl_r = true;
                key = KeyFlag::OtherKey;
                flag_make = true;
            }
            0x9d => {
                // 松开右边的ctrl
                SCAN_CODE_STATUS.write().ctrl_r = false;
                flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x38 => {
                // 按下右边的alt
                SCAN_CODE_STATUS.write().alt_r = true;
                key = KeyFlag::OtherKey;
                flag_make = true;
            }
            0xb8 => {
                // 松开右边的alt
                SCAN_CODE_STATUS.write().alt_r = false;
                flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x5b => {
                SCAN_CODE_STATUS.write().gui_l = true;
                key = KeyFlag::OtherKey;
                flag_make = true;
            }
            0xdb => {
                SCAN_CODE_STATUS.write().gui_l = false;
                flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x5c => {
                SCAN_CODE_STATUS.write().gui_r = true;
                key = KeyFlag::OtherKey;
                flag_make = true;
            }
            0xdc => {
                SCAN_CODE_STATUS.write().gui_r = false;
                flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x5d => {
                SCAN_CODE_STATUS.write().apps = true;
                key = KeyFlag::OtherKey;
                flag_make = true;
            }
            0xdd => {
                SCAN_CODE_STATUS.write().apps = false;
                flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x52 => {
                SCAN_CODE_STATUS.write().insert = true;
                key = KeyFlag::OtherKey;
                flag_make = true;
            }
            0xd2 => {
                SCAN_CODE_STATUS.write().insert = false;
                flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x47 => {
                SCAN_CODE_STATUS.write().home = true;
                key = KeyFlag::OtherKey;
                flag_make = true;
            }
            0xc7 => {
                SCAN_CODE_STATUS.write().home = false;
                flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x49 => {
                SCAN_CODE_STATUS.write().pgup = true;
                key = KeyFlag::OtherKey;
                flag_make = true;
            }
            0xc9 => {
                SCAN_CODE_STATUS.write().pgup = false;
                flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x53 => {
                SCAN_CODE_STATUS.write().del = true;
                key = KeyFlag::OtherKey;
                flag_make = true;
            }
            0xd3 => {
                SCAN_CODE_STATUS.write().del = false;
                flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x4f => {
                SCAN_CODE_STATUS.write().end = true;
                key = KeyFlag::OtherKey;
                flag_make = true;
            }
            0xcf => {
                SCAN_CODE_STATUS.write().end = false;
                flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x51 => {
                SCAN_CODE_STATUS.write().pgdn = true;
                key = KeyFlag::OtherKey;
                flag_make = true;
            }
            0xd1 => {
                SCAN_CODE_STATUS.write().pgdn = false;
                flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x48 => {
                SCAN_CODE_STATUS.write().arrow_u = true;
                key = KeyFlag::OtherKey;
                flag_make = true;
            }
            0xc8 => {
                SCAN_CODE_STATUS.write().arrow_u = false;
                flag_make = false;
                key = KeyFlag::OtherKey;
                ch = 0xc8;
                // return (0, 0xc8);
            }
            0x4b => {
                SCAN_CODE_STATUS.write().arrow_l = true;
                key = KeyFlag::OtherKey;
                flag_make = true;
            }
            0xcb => {
                SCAN_CODE_STATUS.write().arrow_l = false;
                flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x50 => {
                SCAN_CODE_STATUS.write().arrow_d = true;
                key = KeyFlag::OtherKey;
                flag_make = true;
                ch = 0x50;
                // return (0, 0x50);
            }
            0xd0 => {
                SCAN_CODE_STATUS.write().arrow_d = false;
                flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x4d => {
                SCAN_CODE_STATUS.write().arrow_r = true;
                key = KeyFlag::OtherKey;
                flag_make = true;
            }
            0xcd => {
                SCAN_CODE_STATUS.write().arrow_r = false;
                flag_make = false;
                key = KeyFlag::OtherKey;
            }

            0x35 => {
                // 数字小键盘的 / 符号
                SCAN_CODE_STATUS.write().kp_forward_slash = true;
                key = KeyFlag::OtherKey;
                flag_make = true;
                ch = '/' as u8;
            }
            0xb5 => {
                SCAN_CODE_STATUS.write().kp_forward_slash = false;
                flag_make = false;
                key = KeyFlag::OtherKey;
            }
            0x1c => {
                SCAN_CODE_STATUS.write().kp_enter = true;
                ch = '\n' as u8;
                flag_make = true;
                key = KeyFlag::OtherKey;
            }
            0x9c => {
                SCAN_CODE_STATUS.write().kp_enter = false;
                flag_make = false;
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

    return (ch, key);
}

/// @brief 从键盘设备文件中获取键盘扫描码
fn keyboard_get_scancode() -> Result<i32, i32> {
    // 如果后面对性能有要求，可以把这个inode存下来当作static变量
    let keyboard = ROOT_INODE().lookup("/dev/char/ps2_keyboard");

    if keyboard.is_err() {
        kerror!("Failed in finding ps2_keyboard ");
        return Err(-1);
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
    }
    return Ok(buf[0] as i32);
}
