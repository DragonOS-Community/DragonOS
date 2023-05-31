use crate::{
    driver::uart::uart::{c_uart_send, c_uart_send_str, UartPort},
    include::bindings::bindings::video_frame_buffer_info,
    libs::spinlock::SpinLock,
    syscall::SystemError,
};
use alloc::{collections::LinkedList, string::ToString};
use alloc::{sync::Arc, vec::Vec};
use core::{fmt::Debug, intrinsics::unlikely, ptr::copy_nonoverlapping};

use thingbuf::mpsc::{
    self,
    errors::{TryRecvError, TrySendError},
};

use super::{
    screen_manager::{
        scm_register, ScmBufferInfo, ScmFramworkType, ScmUiFramework, ScmUiFrameworkMetadata,
    },
    textui_render::{no_init_textui_render_chromatic, renew_buf, textui_render_chromatic},
};
use lazy_static::lazy_static;

// 暂时初始化16080个初始字符对象以及67个虚拟行对象
// const INITIAL_CHARS_NUM: usize = 200;
// const INITIAL_VLINES_NUM: usize = 67;
// const CHARS_PER_VLINE: usize = 200;
lazy_static! {
// pub static ref INITIAL_WINDOW: SpinLock<TextuiWindow> = SpinLock::new(TextuiWindow::new()); // 初始窗口
pub static ref WINDOW_LIST: SpinLock<LinkedList<TextuiWindow>> = SpinLock::new(LinkedList::new());
}
//window标志位
bitflags! {
    pub struct WindowFlag: u8 {
        // 采用彩色字符
        const TEXTUI_IS_CHROMATIC = 1 << 0;
    }
}

// 每个字符的宽度和高度（像素）
const TEXTUI_CHAR_WIDTH: u32 = 8;
const TEXTUI_CHAR_HEIGHT: u32 = 16;

pub static mut TEST_IS_INIT: bool = false;
pub static mut TRUE_LINE_NUM: u32 = 0;
pub static mut CHAR_PER_LINE: u32 = 0;

lazy_static! {
    pub static ref TEXTUIFRAMEWORK: LockedTextUiFramework = LockedTextUiFramework::new();
}
lazy_static! {
    pub static ref TEXTUI_PRIVATE_INFO: SpinLock<TextuiPrivateInfo> =
        SpinLock::new(TextuiPrivateInfo::new());
}
lazy_static! {
    pub static ref WINDOW_MPSC: WindowMpsc = WindowMpsc::new();
}
// 利用mpsc实现当前窗口
pub struct WindowMpsc {
    window_r: mpsc::Receiver<TextuiWindow>,
    window_s: mpsc::Sender<TextuiWindow>,
}
pub const BUF_SIZE: usize = 4096;
impl WindowMpsc {
    fn new() -> Self {
        // let window = &TEXTUI_PRIVATE_INFO.lock().current_window;
        let (window_s, window_r) = mpsc::channel::<TextuiWindow>(BUF_SIZE);
        WindowMpsc { window_r, window_s }
    }
}
/**
 * @brief 黑白字符对象
 *
 */
#[derive(Clone, Debug)]
struct TextuiCharNormal {
    _c: u8,
}

/**
 * @brief 彩色字符对象
 *
 */
#[derive(Clone, Debug, Copy)]
pub struct TextuiCharChromatic {
    pub c: u8,

    // 前景色
    pub frcolor: usize, // rgb

    // 背景色
    pub bkcolor: usize, // rgb
}
impl TextuiCharChromatic {
    fn new() -> Self {
        TextuiCharChromatic {
            c: 0,
            frcolor: 0,
            bkcolor: 0,
        }
    }
}
// 注意！！！ 请保持vline结构体的大小、成员变量命名相等！
/**
 * @brief 单色显示的虚拟行结构体
 *
 */
#[derive(Clone, Debug, Default)]
pub struct TextuiVlineNormal {
    _chars: Vec<TextuiCharNormal>, // 字符对象数组
    _index: i16,                   // 当前操作的位置
}
/**
 * @brief 彩色显示的虚拟行结构体
 *
 */
#[derive(Clone, Debug, Default)]
pub struct TextuiVlineChromatic {
    chars: Vec<TextuiCharChromatic>, // 字符对象数组
    index: i16,                      // 当前操作的位置
}
impl TextuiVlineChromatic {
    fn new() -> Self {
        TextuiVlineChromatic {
            chars: Vec::new(),
            index: 0,
        }
    }
    /**
     * @brief 初始化虚拟行对象
     *
     * @param vline 虚拟行对象指针
     * @param chars_ptr 字符对象数组指针
     */
    fn textui_init_vline(&mut self, num: usize) {
        self.index = 0;
        let value = TextuiCharChromatic::new();
        for _i in 0..num {
            self.chars.push(value);
        }
    }
    // fn clone_s(&self) -> TextuiVlineChromatic {
    //     TextuiVlineChromatic {
    //         chars: self.chars.clone(),
    //         index: self.index,
    //     }
    // }
}

#[derive(Clone, Debug)]
pub enum TextuiVline {
    Chromatic(TextuiVlineChromatic),
    _Normal(TextuiVlineNormal),
}

#[derive(Clone, Debug)]
pub struct TextuiWindow {
    // 虚拟行是个循环表，头和尾相接
    pub id: u32,
    // 虚拟行总数
    pub vline_sum: i16,
    // 当前已经使用了的虚拟行总数（即在已经输入到缓冲区（之后显示在屏幕上）的虚拟行数量）
    pub vlines_used: i16,
    // 位于最顶上的那一个虚拟行的行号
    pub top_vline: i16,
    // 储存虚拟行的数组
    pub vlines: Vec<TextuiVline>,
    // 正在操作的vline
    pub vline_operating: i16,
    // 每行最大容纳的字符数
    pub chars_per_line: i16,
    // 窗口flag
    pub flags: WindowFlag,
}

pub static mut CORRENT_WINDOW_ID: u32 = 0;
pub static mut ACTUAL_LINE: i16 = 0;
impl TextuiWindow {
    fn new() -> Self {
        TextuiWindow {
            id: 0,
            flags: WindowFlag::TEXTUI_IS_CHROMATIC,
            vline_sum: 0,
            vlines_used: 1,
            top_vline: 0,
            vlines: Vec::new(),
            vline_operating: 0,
            chars_per_line: 0,
        }
    }
    /**
     * @brief 初始化window对象
     *
     * @param window 窗口对象
     * @param flags 标志位
     * @param vlines_num 虚拟行的总数
     * @param vlines_ptr 虚拟行数组指针
     * @param cperline 每行最大的字符数
     */
    fn init_window(
        window: &mut TextuiWindow,
        id: u32,
        flags: WindowFlag,
        vlines_num: i16,
        vlines_ptr: Vec<TextuiVline>,
        cperline: i16,
    ) -> Result<i32, SystemError> {
        window.id = id;
        window.flags = flags;
        window.vline_sum = vlines_num;
        window.vlines_used = 1;
        window.top_vline = 0;
        window.vlines = vlines_ptr;
        window.vline_operating = 0;
        window.chars_per_line = cperline;

        WINDOW_LIST.lock().push_back(window.clone());

        return Ok(0);
    }

    /**
     * @brief 刷新某个窗口的缓冲区的某个虚拟行的连续n个字符对象
     *
     * @param window 窗口结构体
     * @param vline_id 要刷新的虚拟行号
     * @param start 起始字符号
     * @param count 要刷新的字符数量
     * @return int 错误码
     */
    fn textui_refresh_characters(
        &self,
        vline_id: i16,
        start: i16,
        count: i16,
    ) -> Result<i32, SystemError> {
        // 要刷新的窗口正在使用，则退出刷新
        let window_id = unsafe { CORRENT_WINDOW_ID };
        let actual_line = unsafe { ACTUAL_LINE };
        if self.id != window_id {
            return Ok(0);
        }
        // 判断虚拟行参数是否合法
        if unlikely(vline_id >= self.vline_sum || (start + count) > self.chars_per_line) {
            return Err(SystemError::EINVAL);
        }
        // 计算虚拟行对应的真实行（即要渲染的行）
        let mut actual_line_id = vline_id - self.top_vline; //为正说明虚拟行不在真实行显示的区域上面

        if actual_line_id < 0 {
            //真实行数小于虚拟行数，则需要加上真实行数的位置，以便正确计算真实行
            actual_line_id += actual_line;
        }
        // 判断真实行id是否合理
        if unlikely(actual_line_id < 0 || actual_line_id >= actual_line) {
            return Ok(0);
        }

        // 将此窗口的某个虚拟行的连续n个字符对象往缓存区写入
        if self.flags.contains(WindowFlag::TEXTUI_IS_CHROMATIC) {
            let vline = &self.vlines[vline_id as usize];
            let mut i = 0;
            while i < count {
                if let TextuiVline::Chromatic(vline) = vline {
                    if self.vline_operating == 1 && vline.chars[8].c == b' ' && vline.index == 9 {
                        loop {}
                    }
                    textui_render_chromatic(
                        actual_line_id as u16,
                        (start + i) as u16,
                        &vline.chars[(start + i) as usize],
                    );
                    // if self.vline_operating == 1 && vline.chars[8].c == b' ' && vline.index == 9 {
                    //     loop {}
                    // }
                    // if self.vline_operating == 1&&vline.index==9 {
                    //     loop {}
                    // }
                }

                i += 1;
            }
        }
        return Ok(0);
    }

    /**
     * @brief 重新渲染某个窗口的某个虚拟行
     *
     * @param window 窗口结构体
     * @param vline_id 虚拟行号
     * @return int 错误码
     */
    fn textui_refresh_vline(&self, vline_id: i16) -> Result<i32, SystemError> {
        if self.flags.contains(WindowFlag::TEXTUI_IS_CHROMATIC) {
            return self.textui_refresh_characters(vline_id as i16, 0, self.chars_per_line);
        } else {
            //todo支持纯文本字符
            todo!();
        }
    }

    // 刷新某个窗口的start 到start + count行（即将这些行输入到缓冲区）
    fn textui_refresh_vlines(&self, start: u16, count: u16) -> i32 {
        let mut refresh_count = count;
        for i in start..(self.vline_sum as u16).min(start + count) {
            let _ = self.textui_refresh_vline(i as i16);
            refresh_count -= 1;
        }
        //因为虚拟行是循环表
        let mut refresh_start = 0;
        while refresh_count > 0 {
            let _ = self.textui_refresh_vline(refresh_start);
            refresh_start += 1;
            refresh_count -= 1;
        }
        return 0;
    }

    /**
     * @brief 往某个窗口的缓冲区的某个虚拟行插入换行
     *
     * @param window 窗口结构体
     * @param vline_id 虚拟行号
     * @return int
     */
    fn textui_new_line(&mut self) -> i32 {
        // todo: 支持在两个虚拟行之间插入一个新行

        self.vline_operating += 1;
        //如果已经到了最大行数，则重新从0开始
        if self.vline_operating == self.vline_sum {
            self.vline_operating = 0;
        }

        if let TextuiVline::Chromatic(vline) = &mut (self.vlines[self.vline_operating as usize]) {
            for i in 0..self.chars_per_line {
                if let Some(v_char) = vline.chars.get_mut(i as usize) {
                    v_char.c = 0;
                    v_char.frcolor = 0;
                    v_char.bkcolor = 0;
                }
            }
            vline.index = 0;
        }
        // 当已经使用的虚拟行总数等于真实行总数时，说明窗口中已经显示的文本行数已经达到了窗口的最大容量。这时，如果继续在窗口中添加新的文本，就会导致文本溢出窗口而无法显示。因此，需要往下滚动屏幕来显示更多的文本。

        if self.vlines_used == unsafe { ACTUAL_LINE } {
            self.top_vline += 1;

            if self.top_vline >= self.vline_sum {
                self.top_vline = 0;
            }

            // 刷新所有行
            self.textui_refresh_vlines(self.top_vline as u16, self.vline_sum as u16);
        } else {
            //换行说明上一行已经在缓冲区中，所以已经使用的虚拟行总数+1
            self.vlines_used += 1;
        }

        return 0;
    }

    /**
     * @brief 真正向窗口的缓冲区上输入字符的函数(位置为window.vline_operating，window.vline_operating.index)
     *
     * @param window
     * @param character
     * @return int
     */
    fn ture_textui_putchar_window(
        &mut self,
        character: u8,
        frcolor: usize,
        bkcolor: usize,
    ) -> Result<i32, SystemError> {
        // 启用彩色字符
        if self.flags.contains(WindowFlag::TEXTUI_IS_CHROMATIC) {
            let mut s_vline = 0; //操作的列号
            if let TextuiVline::Chromatic(vline) = &mut (self.vlines[self.vline_operating as usize])
            {
                let index = vline.index as usize;
                if vline.chars.is_empty() {
                    c_uart_send_str(UartPort::COM1.to_u16(), "putcharkkkkk\n\0".as_ptr());
                }
                if let Some(v_char) = vline.chars.get_mut(index) {
                    v_char.c = character;
                    v_char.frcolor = frcolor & 0xffffff;
                    v_char.bkcolor = bkcolor & 0xffffff;
                }
                vline.index += 1;
                s_vline = vline.index - 1;
                // if self.vline_operating == 1 && vline.chars[9].c == b'3' && vline.index == 10&&character==b'3' {
                //     loop {}
                // }
                // if self.vline_operating == 1 && vline.chars[8].c == b' ' && vline.index == 9&&character==b' ' {
                //     loop {}
                // }
            }

            self.textui_refresh_characters(self.vline_operating, s_vline, 1)?;

            // 加入光标后，因为会识别光标，所以需超过该行最大字符数才能创建新行
            if s_vline + 1 > self.chars_per_line {

                self.textui_new_line();
            }
        } else {
            // todo: 支持纯文本字符
            todo!();
        }
        return Ok(0);
    }
    /**
     * @brief 根据输入的一个字符在窗口上输出
     *
     * @param window 窗口
     * @param character 字符
     * @param FRcolor 前景色（RGB）
     * @param BKcolor 背景色（RGB）
     * @return int
     */
    fn textui_putchar_window(
        &mut self,
        character: u8,
        frcolor: u32,
        bkcolor: u32,
    ) -> Result<i32, SystemError> {
        //字符'\0'代表ASCII码表中的空字符,表示字符串的结尾
        if unlikely(character == b'\0') {
            return Ok(0);
        }
        // 暂不支持纯文本窗口
        if !self.flags.contains(WindowFlag::TEXTUI_IS_CHROMATIC) {
            return Ok(0);
        }
        c_uart_send(UartPort::COM1.to_u16(), character);
        //进行换行操作
        if unlikely(character == b'\n') {
            // 换行时还需要输出\r
            c_uart_send(UartPort::COM1.to_u16(), b'\r');
            self.textui_new_line();

            return Ok(0);
        }
        // 输出制表符
        else if character == b'\t' {
            if let TextuiVline::Chromatic(vline) = &self.vlines[self.vline_operating as usize] {
                //打印的空格数（注意将每行分成一个个表格，每个表格为8个字符）
                let mut space_to_print = 8 - vline.index % 8;
                while space_to_print > 0 {
                    self.ture_textui_putchar_window(b' ', frcolor as usize, bkcolor as usize)?;
                    space_to_print -= 1;
                }
            }
        }  
        // else if character == b' ' {
        //     self.ture_textui_putchar_window(b' ', frcolor as usize, bkcolor as usize)?;
        // }
        // 字符 '\x08' 代表 ASCII 码中的退格字符。它在输出中的作用是将光标向左移动一个位置，并在该位置上输出后续的字符，从而实现字符的删除或替换。
        else if character == b'\x08' {
            let mut tmp = 0;
            if let TextuiVline::Chromatic(vline) = &mut self.vlines[self.vline_operating as usize] {
                // if self.vline_operating == 1&&vline.index==9 {
                //     loop {}
                // }
                vline.index -= 1;
                tmp = vline.index;
            }
            if tmp >= 0 {
                if let TextuiVline::Chromatic(vline) =
                    &mut self.vlines[self.vline_operating as usize]
                {
                    if let Some(v_char) = vline.chars.get_mut(tmp as usize) {
                        v_char.c = b' ';

                        v_char.bkcolor = bkcolor as usize & 0xffffff;
                    }
                }
                return self.textui_refresh_characters(self.vline_operating, tmp, 1);
            }
            // 需要向上缩一行
            if tmp < 0 {
                // 当前行为空,需要重新刷新
                if let TextuiVline::Chromatic(vline) =
                    &mut self.vlines[self.vline_operating as usize]
                {
                    vline.index = 0;
                    for i in 0..self.chars_per_line {
                        if let Some(v_char) = vline.chars.get_mut(i as usize) {
                            v_char.c = 0;
                            v_char.frcolor = 0;
                            v_char.bkcolor = 0;
                        }
                    }
                }
                // 上缩一行
                self.vline_operating -= 1;
                if self.vline_operating < 0 {
                    self.vline_operating = self.vline_sum - 1;
                }

                // 考虑是否向上滚动（在top_vline上退格）
                if self.vlines_used > unsafe { ACTUAL_LINE } {
                    self.top_vline -= 1;
                    if self.top_vline < 0 {
                        self.top_vline = self.vline_sum - 1;
                    }
                }
                //因为上缩一行所以显示在屏幕中的虚拟行少一
                self.vlines_used -= 1;
                self.textui_refresh_vlines(self.top_vline as u16, unsafe { ACTUAL_LINE } as u16);
            }
        } else {
            // 输出其他字符
            if let TextuiVline::Chromatic(vline) = &self.vlines[self.vline_operating as usize] {
                // if self.vline_operating == 1&&vline.index==9 {
                //     loop {}
                // }
                // if self.vline_operating == 1&&vline.chars[8].c==b' '&&vline.index==9{
                //     loop{}
                // }
                if vline.index == self.chars_per_line {
                    self.textui_new_line();
                }

                return self.ture_textui_putchar_window(
                    character,
                    frcolor as usize,
                    bkcolor as usize,
                );
            }
        }

        return Ok(0);
    }
}
impl Default for TextuiWindow {
    fn default() -> Self {
        TextuiWindow {
            id: 0,
            flags: WindowFlag::TEXTUI_IS_CHROMATIC,
            vline_sum: 0,
            vlines_used: 1,
            top_vline: 0,
            vlines: Vec::new(),
            vline_operating: 0,
            chars_per_line: 0,
        }
    }
}
#[derive(Clone, Debug)]
pub struct TextuiPrivateInfo {
    pub actual_line: i16, // 真实行的数量（textui的帧缓冲区能容纳的内容的行数）
    pub current_window: TextuiWindow, // 当前的主窗口
    pub default_window: TextuiWindow, // 默认print到的窗口
}
impl TextuiPrivateInfo {
    pub fn new() -> Self {
        let p = TextuiPrivateInfo {
            actual_line: 0,
            current_window: TextuiWindow::new(),
            default_window: TextuiWindow::new(),
        };

        return p;
    }
}
#[derive(Debug)]
pub struct TextUiFramework {
    // self_ref: Weak<LockedTextUiFramework>,
    pub metadata: ScmUiFrameworkMetadata,
}
#[derive(Debug)]
pub struct LockedTextUiFramework(pub SpinLock<TextUiFramework>);
impl LockedTextUiFramework {
    pub fn new() -> Self {
        let inner = TextUiFramework {
            metadata: ScmUiFrameworkMetadata::new(ScmFramworkType::Text),
        };
        let result = Self(SpinLock::new(inner));

        return result;
    }
}
impl ScmUiFramework for TextUiFramework {
    // 安装ui框架的回调函数
    fn install(&self, _buf: ScmBufferInfo) -> Result<i32, SystemError> {
        c_uart_send_str(
            UartPort::COM1.to_u16(),
            "textui_install_handler\n\0".as_ptr(),
        );
        return Ok(0);
    }
    // 卸载ui框架的回调函数
    fn uninstall(&self) -> Result<i32, SystemError> {
        return Ok(0);
    }
    // 启用ui框架的回调函数
    fn enable(&self) -> Result<i32, SystemError> {
        c_uart_send_str(
            UartPort::COM1.to_u16(),
            "textui_enable_handler\n\0".as_ptr(),
        );
        return Ok(0);
    }
    // 禁用ui框架的回调函数
    fn disable(&self) -> Result<i32, SystemError> {
        return Ok(0);
    }
    // 改变ui框架的帧缓冲区的回调函数
    fn change(&self, buf: ScmBufferInfo) -> Result<i32, SystemError> {
        let f = TEXTUIFRAMEWORK.0.lock();
        let src = f.metadata.buf.vaddr as *const u8;
        let dst = buf.vaddr as *mut u8;
        let count = f.metadata.buf.size as usize;
        unsafe { copy_nonoverlapping(src, dst, count) };
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    /// @brief 获取ScmUiFramework的元数据
    ///
    /// @return 成功：Ok(ScmUiFramework的元数据)
    ///         失败：Err(错误码)
    fn metadata(&self) -> Result<ScmUiFrameworkMetadata, SystemError> {
        return Ok(self.metadata.clone());
    }
}
//textui 未初始化时直接向缓冲区写，不使用虚拟行
pub static mut NO_INIT_OPERATIONS_LINE: i16 = 0;
pub static mut NO_INIT_OPERATIONS_CHAR: i16 = 0;

pub fn no_init_textui_putchar_window(
    character: u8,
    frcolor: usize,
    bkcolor: usize,
) -> Result<i32, SystemError> {
    if unsafe { NO_INIT_OPERATIONS_LINE > TRUE_LINE_NUM as i16 } {
        // let fb = unsafe { video_frame_buffer_info.vaddr };
        // // let mut src: *mut u32 =
        // //     (fb as u32 + unsafe { video_frame_buffer_info.width } * TEXTUI_CHAR_HEIGHT) as *mut u32;
        // let dst: *mut u32 = (fb as u32) as *mut u32;
        // unsafe {
        //     write_bytes(
        //         dst,
        //         0,
        //         (video_frame_buffer_info.width * 4 * TEXTUI_CHAR_HEIGHT * TRUE_LINE_NUM) as usize,
        //     )
        // };
        unsafe { NO_INIT_OPERATIONS_LINE = 0 };
    }
    //字符'\0'代表ASCII码表中的空字符,表示字符串的结尾
    if unlikely(character == b'\0') {
        return Ok(0);
    }

    c_uart_send(UartPort::COM1.to_u16(), character);

    //进行换行操作
    if unlikely(character == b'\n') {
        // 换行时还需要输出\r
        c_uart_send(UartPort::COM1.to_u16(), b'\r');
        unsafe { NO_INIT_OPERATIONS_LINE += 1 };
        unsafe { NO_INIT_OPERATIONS_CHAR = 0 };

        return Ok(0);
    }
    // 输出制表符
    else if character == b'\t' {
        let char = TextuiCharChromatic {
            c: 1,
            frcolor,
            bkcolor,
        };

        //打印的空格数（注意将每行分成一个个表格，每个表格为8个字符）
        let mut space_to_print = 8 - unsafe { NO_INIT_OPERATIONS_CHAR } % 8;
        while space_to_print > 0 {
            no_init_textui_render_chromatic(
                unsafe { NO_INIT_OPERATIONS_LINE } as u16,
                unsafe { NO_INIT_OPERATIONS_CHAR } as u16,
                &char,
            );
            unsafe { NO_INIT_OPERATIONS_CHAR += 1 };
            space_to_print -= 1;
        }
        return Ok(0);
    }
    // 字符 '\x08' 代表 ASCII 码中的退格字符。它在输出中的作用是将光标向左移动一个位置，并在该位置上输出后续的字符，从而实现字符的删除或替换。
    else if character == b'\x08' {
        unsafe { NO_INIT_OPERATIONS_CHAR -= 1 };
        let op_char = unsafe { NO_INIT_OPERATIONS_CHAR };
        if op_char >= 0 {
            let char = TextuiCharChromatic {
                c: b' ',
                frcolor,
                bkcolor,
            };
            no_init_textui_render_chromatic(
                unsafe { NO_INIT_OPERATIONS_LINE } as u16,
                unsafe { NO_INIT_OPERATIONS_CHAR } as u16,
                &char,
            );
            unsafe { NO_INIT_OPERATIONS_CHAR += 1 };
        }
        // 需要向上缩一行
        if op_char < 0 {
            // 上缩一行
            unsafe { NO_INIT_OPERATIONS_LINE -= 1 };
            unsafe { NO_INIT_OPERATIONS_CHAR = 0 };
            if unsafe { NO_INIT_OPERATIONS_LINE < 0 } {
                unsafe { NO_INIT_OPERATIONS_LINE = 0 }
            }
        }
    } else {
        // 输出其他字符
        let char = TextuiCharChromatic {
            c: character,
            frcolor,
            bkcolor,
        };

        if unsafe { NO_INIT_OPERATIONS_CHAR } == unsafe { CHAR_PER_LINE } as i16 {
            unsafe { NO_INIT_OPERATIONS_CHAR = 0 };
            unsafe { NO_INIT_OPERATIONS_LINE += 1 };
        }
        no_init_textui_render_chromatic(
            unsafe { NO_INIT_OPERATIONS_LINE } as u16,
            unsafe { NO_INIT_OPERATIONS_CHAR } as u16,
            &char,
        );

        unsafe { NO_INIT_OPERATIONS_CHAR += 1 };
    }

    return Ok(0);
}

/**
 * @brief 在默认窗口上输出一个字符
 *
 * @param character 字符
 * @param FRcolor 前景色（RGB）
 * @param BKcolor 背景色（RGB）
 * @return int
 */
#[no_mangle]
pub extern "C" fn textui_putchar(character: u8, fr_color: u32, bk_color: u32) -> i32 {
    let r = true_textui_window(character, fr_color, bk_color)
        .map_err(|e| e.to_posix_errno())
        .unwrap();
    if r.is_negative() {
        c_uart_send_str(
            UartPort::COM1.to_u16(),
            "textui putchar failed.\n\0".as_ptr(),
        );
    }
    return r;
}
lazy_static! {
    pub static ref CURRENT_WINDOW: TextuiWindow = TEXTUI_PRIVATE_INFO.lock().current_window.clone();
}
fn true_textui_window(character: u8, fr_color: u32, bk_color: u32) -> Result<i32, SystemError> {
    if unsafe { TEST_IS_INIT } {
        let val = WINDOW_MPSC.window_r.try_recv_ref();

        let mut window: TextuiWindow;
        if let Err(err) = val {
            match err {
                TryRecvError::Empty => {
                    window = CURRENT_WINDOW.clone();
                }
                _ => todo!(),
            }
        } else {
            let r = val.unwrap();
            window = r.clone();
            // window = TextuiWindow {
            //     id: r.id,
            //     vline_sum: r.vline_sum,
            //     vlines_used: r.vlines_used,
            //     top_vline: r.top_vline,
            //     vlines: r.vlines.clone(),
            //     vline_operating: r.vline_operating,
            //     chars_per_line: r.chars_per_line,
            //     flags: r.flags,
            // };
        }
        window.textui_putchar_window(character, fr_color, bk_color)?;
        if let TextuiVline::Chromatic(vline) = &window.vlines[window.vline_operating as usize] {
            // if self.vline_operating == 1&&vline.index==9 {
            //     loop {}
            // }
            if window.vline_operating == 1
                && vline.chars[9].c == b'3'
                && vline.index == 10
                && character == b'3'
            {
                loop {}
            }
        }
        let mut window_s = WINDOW_MPSC.window_s.try_send_ref();

        loop {
            if let Err(err) = window_s {
                match err {
                    TrySendError::Full(_) => {
                        window_s = WINDOW_MPSC.window_s.try_send_ref();
                        // c_uart_send_str(
                        //     UartPort::COM1.to_u16(),
                        //     "textui init failed66666666.\n\0".as_ptr(),
                        // );
                    }
                    _ => todo!(),
                }
            } else {
                break;
            }
        }
        *window_s.unwrap() = window;

        // let mut privateinfo=TEXTUI_PRIVATE_INFO.lock();
        // privateinfo.current_window.textui_putchar_window(character, fr_color, bk_color)?;
        // drop(privateinfo);
        // c_uart_send_str(UartPort::COM1.to_u16(), "textui init failed555555555555\n\0".as_ptr());
    } else {
        //未初始化暴力输出
        unsafe { TRUE_LINE_NUM = video_frame_buffer_info.height / TEXTUI_CHAR_HEIGHT };
        unsafe { CHAR_PER_LINE = video_frame_buffer_info.width / TEXTUI_CHAR_WIDTH };
        return no_init_textui_putchar_window(character, fr_color as usize, bk_color as usize);
    }
    return Ok(0);
}
/**
 * @brief 初始化text ui框架
 *
 * @return int
 */
#[no_mangle]
pub extern "C" fn rs_textui_init() -> i32 {
    let r = textui_init().map_err(|e| e.to_posix_errno()).unwrap();
    if r.is_negative() {
        c_uart_send_str(UartPort::COM1.to_u16(), "textui init failed.\n\0".as_ptr());
    }
    return r;
}

fn textui_init() -> Result<i32, SystemError> {
    let name: &str = "textui";
    let framework = &mut TEXTUIFRAMEWORK.0.lock();

    framework.metadata.name = name.to_string();
    framework.metadata.f_type = ScmFramworkType::Text;
    let private_info = &mut TEXTUI_PRIVATE_INFO.lock();
    private_info.actual_line = (framework.metadata.buf.height / TEXTUI_CHAR_HEIGHT) as i16;

    // 注册框架到屏幕管理器
    let textui = TextUiFramework {
        metadata: framework.metadata.clone(),
    };
    let retval = scm_register(Arc::new(textui));
    if retval.is_err() {
        c_uart_send_str(UartPort::COM1.to_u16(), "text ui init failed\n\0".as_ptr());
    }
    // 初始化虚拟行
    let vlines_num = (framework.metadata.buf.height / TEXTUI_CHAR_HEIGHT) as usize;
    let chars_num = (framework.metadata.buf.width / TEXTUI_CHAR_WIDTH) as usize;
    // if vlines_num <= 0 {
    //     c_uart_send_str(
    //         UartPort::COM1.to_u16(),
    //         "text ui initialized666666666\n\0".as_ptr(),
    //     );
    // }
    let initial_vlines_num = vlines_num;
    let mut initial_vlines = Vec::new();
    for _i in 0..initial_vlines_num {
        let mut vline = TextuiVlineChromatic::new();
        vline.textui_init_vline(chars_num);
        initial_vlines.push(TextuiVline::Chromatic(vline));
    }

    // 初始化窗口
    let mut initial_window = TextuiWindow::new();

    TextuiWindow::init_window(
        &mut initial_window,
        framework.metadata.window_max_id,
        WindowFlag::TEXTUI_IS_CHROMATIC,
        initial_vlines_num as i16,
        initial_vlines,
        chars_num as i16,
    )?;

    framework.metadata.window_max_id += 1;
    // if let TextuiVline::Chromatic(vline) = &initial_window.vlines[0] {
    //     if vline.chars.is_empty() {
    //         c_uart_send_str(
    //             UartPort::COM1.to_u16(),
    //             "text ui initializedeeeeeeeeeeeeeee\n\0".as_ptr(),
    //         );
    //     }
    // }
    let num = framework.metadata.buf.width * framework.metadata.buf.height;
    renew_buf(framework.metadata.buf.vaddr, num);

    private_info.current_window = initial_window.clone();

    private_info.default_window = initial_window;
    // private_info.current_window.textui_refresh_vlines(0, vlines_num as u16);
    // loop{}
    unsafe { CORRENT_WINDOW_ID = private_info.current_window.id };

    unsafe { ACTUAL_LINE = private_info.actual_line };
    unsafe { TEST_IS_INIT = true };
    // private_info.current_window.textui_putchar_window(b']', 0x00ffffff, 0);
    // loop{}
    c_uart_send_str(UartPort::COM1.to_u16(), "text ui initialized\n\0".as_ptr());
    return Ok(0);
}
