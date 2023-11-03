use crate::{
    driver::{
        tty::serial::serial8250::send_to_default_serial8250_port, video::video_refresh_manager,
    },
    kdebug, kinfo,
    libs::{
        lib_ui::font::FONT_8x16,
        rwlock::RwLock,
        spinlock::{SpinLock, SpinLockGuard},
    },
    process::kthread::{KernelThreadClosure, KernelThreadMechanism},
    syscall::{Syscall, SystemError},
    time::Duration,
};
use alloc::{boxed::Box, collections::LinkedList, string::ToString};
use alloc::{sync::Arc, vec::Vec};
use core::{
    cell::RefCell,
    fmt::Debug,
    intrinsics::unlikely,
    ops::{Add, AddAssign, Sub},
    sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering},
};

use super::{
    screen_manager::{
        scm_register, ScmBuffer, ScmBufferInfo, ScmFramworkType, ScmUiFramework,
        ScmUiFrameworkMetadata,
    },
    termios::Winsize,
    textui_no_alloc::no_init_textui_putchar_window,
};

/// 声明全局的TEXTUI_FRAMEWORK
static mut __TEXTUI_FRAMEWORK: Option<Arc<TextUiFramework>> = None;

/// 每个字符的宽度和高度（像素）
pub const TEXTUI_CHAR_WIDTH: u32 = 8;

pub const TEXTUI_CHAR_HEIGHT: u32 = 16;

pub static mut TEXTUI_IS_INIT: bool = false;

pub static ENABLE_PUT_TO_WINDOW: AtomicBool = AtomicBool::new(true);

/// 获取TEXTUI_FRAMEWORK的可变实例
pub fn textui_framework() -> Arc<TextUiFramework> {
    unsafe {
        return __TEXTUI_FRAMEWORK
            .as_ref()
            .expect("Textui framework has not been initialized yet!")
            .clone();
    }
}

/// 初始化TEXTUI_FRAMEWORK
pub unsafe fn textui_framwork_init() {
    if __TEXTUI_FRAMEWORK.is_none() {
        kinfo!("textui framework init");
        let metadata = ScmUiFrameworkMetadata::new("TextUI".to_string(), ScmFramworkType::Text);
        kdebug!("textui metadata: {:?}", metadata);
        // 为textui框架生成第一个窗口
        let true_lines_num = (metadata.buf_info().height() / TEXTUI_CHAR_HEIGHT) as usize;

        let chars_num = (metadata.buf_info().width() / TEXTUI_CHAR_WIDTH) as usize;
        // 设定虚拟行是窗口真实行的3倍
        let initial_window = TextuiWindow::new(
            WindowFlag::TEXTUI_CHROMATIC,
            true_lines_num as i32,
            chars_num as i32,
            true_lines_num as i32 * 3,
        );

        let current_window: Arc<SpinLock<TextuiWindow>> = Arc::new(SpinLock::new(initial_window));

        let default_window = current_window.clone();

        // 生成窗口链表，并把上面窗口添加进textui框架的窗口链表中
        let window_list: Arc<SpinLock<LinkedList<Arc<SpinLock<TextuiWindow>>>>> =
            Arc::new(SpinLock::new(LinkedList::new()));
        window_list.lock().push_back(current_window.clone());

        __TEXTUI_FRAMEWORK = Some(Arc::new(TextUiFramework::new(
            metadata,
            window_list,
            current_window,
            default_window,
        )));

        scm_register(textui_framework()).expect("register textui framework failed");
        kdebug!("textui framework init success");

        send_to_default_serial8250_port("\ntext ui initialized\n\0".as_bytes());
        unsafe { TEXTUI_IS_INIT = true };
    } else {
        panic!("Try to init TEXTUI_FRAMEWORK twice!");
    }
}
// window标志位
bitflags! {
    pub struct WindowFlag: u8 {
        // 采用彩色字符
        const TEXTUI_CHROMATIC = 1 << 0;
    }
}

/**
 * @brief 黑白字符对象
 *
 */
#[derive(Clone, Debug)]
struct TextuiCharNormal {
    _data: u8,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Hash, Default)]
pub struct LineId(i32);
impl LineId {
    pub fn new(num: i32) -> Self {
        LineId(num)
    }

    pub fn check(&self, max: i32) -> bool {
        self.0 < max && self.0 >= 0
    }

    pub fn data(&self) -> i32 {
        self.0
    }
}
impl Add<i32> for LineId {
    type Output = LineId;
    fn add(self, rhs: i32) -> Self::Output {
        LineId::new(self.0 + rhs)
    }
}
impl Sub<i32> for LineId {
    type Output = LineId;

    fn sub(self, rhs: i32) -> Self::Output {
        LineId::new(self.0 - rhs)
    }
}

impl Into<i32> for LineId {
    fn into(self) -> i32 {
        self.0.clone()
    }
}
impl Into<u32> for LineId {
    fn into(self) -> u32 {
        self.0.clone() as u32
    }
}
impl Into<usize> for LineId {
    fn into(self) -> usize {
        self.0.clone() as usize
    }
}
impl Sub<LineId> for LineId {
    type Output = LineId;

    fn sub(mut self, rhs: LineId) -> Self::Output {
        self.0 -= rhs.0;
        return self;
    }
}
impl AddAssign<LineId> for LineId {
    fn add_assign(&mut self, rhs: LineId) {
        self.0 += rhs.0;
    }
}
#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Hash, Default)]
pub struct LineIndex(i32);
impl LineIndex {
    pub fn new(num: i32) -> Self {
        LineIndex(num)
    }
    pub fn check(&self, chars_per_line: i32) -> bool {
        self.0 < chars_per_line && self.0 >= 0
    }
}
impl Add<LineIndex> for LineIndex {
    type Output = LineIndex;

    fn add(self, rhs: LineIndex) -> Self::Output {
        LineIndex::new(self.0 + rhs.0)
    }
}
impl Add<i32> for LineIndex {
    // type Output = Self;
    type Output = LineIndex;

    fn add(self, rhs: i32) -> Self::Output {
        LineIndex::new(self.0 + rhs)
    }
}
impl Sub<i32> for LineIndex {
    type Output = LineIndex;

    fn sub(self, rhs: i32) -> Self::Output {
        LineIndex::new(self.0 - rhs)
    }
}

impl Into<i32> for LineIndex {
    fn into(self) -> i32 {
        self.0.clone()
    }
}
impl Into<u32> for LineIndex {
    fn into(self) -> u32 {
        self.0.clone() as u32
    }
}
impl Into<usize> for LineIndex {
    fn into(self) -> usize {
        self.0.clone() as usize
    }
}
#[derive(Copy, Clone, Debug)]
pub struct FontColor(u32);
#[allow(dead_code)]
impl FontColor {
    pub const BLUE: FontColor = FontColor::new(0, 0, 0xff);
    pub const RED: FontColor = FontColor::new(0xff, 0, 0);
    pub const GREEN: FontColor = FontColor::new(0, 0xff, 0);
    pub const WHITE: FontColor = FontColor::new(0xff, 0xff, 0xff);
    pub const BLACK: FontColor = FontColor::new(0, 0, 0);
    pub const YELLOW: FontColor = FontColor::new(0xff, 0xff, 0);
    pub const ORANGE: FontColor = FontColor::new(0xff, 0x80, 0);
    pub const INDIGO: FontColor = FontColor::new(0x00, 0xff, 0xff);
    pub const PURPLE: FontColor = FontColor::new(0x80, 0x00, 0xff);

    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        let val = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
        return FontColor(val & 0x00ffffff);
    }
}

impl From<u32> for FontColor {
    fn from(value: u32) -> Self {
        return Self(value & 0x00ffffff);
    }
}
impl Into<usize> for FontColor {
    fn into(self) -> usize {
        self.0.clone() as usize
    }
}
impl Into<u32> for FontColor {
    fn into(self) -> u32 {
        self.0.clone()
    }
}
impl Into<u16> for FontColor {
    fn into(self) -> u16 {
        self.0.clone() as u16
    }
}
impl Into<u64> for FontColor {
    fn into(self) -> u64 {
        self.0.clone() as u64
    }
}

/// 彩色字符对象

#[derive(Clone, Debug, Copy)]
pub struct TextuiCharChromatic {
    c: Option<char>,

    // 前景色
    frcolor: FontColor, // rgb

    // 背景色
    bkcolor: FontColor, // rgb
}

#[derive(Debug)]
pub struct TextuiBuf<'a> {
    buf: Option<&'a mut [u32]>,
    guard: Option<SpinLockGuard<'a, Box<[u32]>>>,
}

impl TextuiBuf<'_> {
    pub fn new(buf: &mut ScmBufferInfo) -> TextuiBuf {
        let len = buf.buf_size() / 4;

        match &buf.buf {
            ScmBuffer::DeviceBuffer(vaddr) => {
                return TextuiBuf {
                    buf: Some(unsafe {
                        core::slice::from_raw_parts_mut(vaddr.data() as *mut u32, len)
                    }),
                    guard: None,
                };
            }

            ScmBuffer::DoubleBuffer(double_buffer) => {
                let guard: SpinLockGuard<'_, Box<[u32]>> = double_buffer.lock();

                return TextuiBuf {
                    buf: None,
                    guard: Some(guard),
                };
            }
        }
    }

    pub fn buf_mut(&mut self) -> &mut [u32] {
        if let Some(buf) = &mut self.buf {
            return buf;
        } else {
            return self.guard.as_mut().unwrap().as_mut();
        }
    }
    pub fn put_color_in_pixel(&mut self, color: u32, index: usize) {
        let buf: &mut [u32] = self.buf_mut();
        buf[index] = color;
    }
    pub fn get_index_of_next_line(now_index: usize) -> usize {
        textui_framework().metadata.read().buf_info().width() as usize + now_index
    }
    pub fn get_index_by_x_y(x: usize, y: usize) -> usize {
        textui_framework().metadata.read().buf_info().width() as usize * y + x
    }
    pub fn get_start_index_by_lineid_lineindex(lineid: LineId, lineindex: LineIndex) -> usize {
        //   x 左上角列像素点位置
        //   y 左上角行像素点位置
        let index_x: u32 = lineindex.into();
        let x: u32 = index_x * TEXTUI_CHAR_WIDTH;

        let id_y: u32 = lineid.into();
        let y: u32 = id_y * TEXTUI_CHAR_HEIGHT;

        TextuiBuf::get_index_by_x_y(x as usize, y as usize)
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub struct Font([u8; 16]);
impl Font {
    #[inline]
    pub fn get_font(character: char) -> Font {
        let x = FONT_8x16.char_map(character);

        let mut data = [0u8; 16];
        data.copy_from_slice(x);
        return Font(data);
    }
    pub fn is_frcolor(&self, height: usize, width: usize) -> bool {
        let w = self.0[height];
        let testbit = 1 << (8 - width);
        w & testbit != 0
    }
}

impl TextuiCharChromatic {
    pub fn new(c: Option<char>, frcolor: FontColor, bkcolor: FontColor) -> Self {
        TextuiCharChromatic {
            c,
            frcolor,
            bkcolor,
        }
    }

    /// 将该字符对象输出到缓冲区
    /// ## 参数
    /// -line_id 要放入的真实行号
    /// -index 要放入的真实列号
    pub fn textui_refresh_character(
        &self,
        lineid: LineId,
        lineindex: LineIndex,
    ) -> Result<i32, SystemError> {
        // 找到要渲染的字符的像素点数据
        if self.c == Some('\n') {
            return Ok(0);
        }
        let font: Font = Font::get_font(self.c.unwrap_or(' '));

        let mut count = TextuiBuf::get_start_index_by_lineid_lineindex(lineid, lineindex);

        let mut _binding = textui_framework().metadata.read().buf_info();

        let mut buf = TextuiBuf::new(&mut _binding);

        // 在缓冲区画出一个字体，每个字体有TEXTUI_CHAR_HEIGHT行，TEXTUI_CHAR_WIDTH列个像素点
        for i in 0..TEXTUI_CHAR_HEIGHT {
            let start = count;
            for j in 0..TEXTUI_CHAR_WIDTH {
                if font.is_frcolor(i as usize, j as usize) {
                    // 字，显示前景色
                    buf.put_color_in_pixel(self.frcolor.into(), count);
                } else {
                    // 背景色
                    buf.put_color_in_pixel(self.bkcolor.into(), count);
                }
                count += 1;
            }
            count = TextuiBuf::get_index_of_next_line(start);
        }

        return Ok(0);
    }

    pub fn no_init_textui_render_chromatic(&self, lineid: LineId, lineindex: LineIndex) {
        // 找到要渲染的字符的像素点数据
        let font = Font::get_font(self.c.unwrap_or(' '));

        //   x 左上角列像素点位置
        //   y 左上角行像素点位置
        let index_x: u32 = lineindex.into();
        let x: u32 = index_x * TEXTUI_CHAR_WIDTH;

        let id_y: u32 = lineid.into();
        let y: u32 = id_y * TEXTUI_CHAR_HEIGHT;

        let buf_width = video_refresh_manager().device_buffer().width();
        // 找到输入缓冲区的起始地址位置
        let buf_start =
            if let ScmBuffer::DeviceBuffer(vaddr) = video_refresh_manager().device_buffer().buf {
                vaddr
            } else {
                panic!("device buffer is not init");
            };

        let mut testbit: u32; // 用来测试特定行的某列是背景还是字体本身

        // 在缓冲区画出一个字体，每个字体有TEXTUI_CHAR_HEIGHT行，TEXTUI_CHAR_WIDTH列个像素点
        for i in 0..TEXTUI_CHAR_HEIGHT {
            // 计算出帧缓冲区每一行打印的起始位置的地址（起始位置+（y+i）*缓冲区的宽度+x）

            let mut addr: *mut u32 =
                (buf_start + buf_width as usize * 4 * (y as usize + i as usize) + 4 * x as usize)
                    .data() as *mut u32;

            testbit = 1 << (TEXTUI_CHAR_WIDTH + 1);

            for _j in 0..TEXTUI_CHAR_WIDTH {
                //从左往右逐个测试相应位
                testbit >>= 1;
                if (font.0[i as usize] & testbit as u8) != 0 {
                    unsafe { *addr = self.frcolor.into() }; // 字，显示前景色
                } else {
                    unsafe { *addr = self.bkcolor.into() }; // 背景色
                }

                unsafe {
                    addr = (addr.offset(1)) as *mut u32;
                }
            }
        }
    }
}

/// 单色显示的虚拟行结构体

#[derive(Clone, Debug, Default)]
pub struct TextuiVlineNormal {
    _characters: Vec<TextuiCharNormal>, // 字符对象数组
    _index: i16,                        // 当前操作的位置
}
/// 彩色显示的虚拟行结构体

#[derive(Clone, Debug, Default)]
pub struct TextuiVlineChromatic {
    chars: Vec<TextuiCharChromatic>, // 字符对象数组
    end_index: LineIndex,            // 最后一个字符的位置
    is_empty: bool,                  // 判断该虚拟行有无储存字符
}
impl TextuiVlineChromatic {
    pub fn new(char_num: usize) -> Self {
        let mut r = TextuiVlineChromatic {
            chars: Vec::with_capacity(char_num),
            end_index: LineIndex::new(0),
            is_empty: true,
        };

        for _ in 0..char_num {
            r.chars.push(TextuiCharChromatic::new(
                None,
                FontColor::BLACK,
                FontColor::BLACK,
            ));
        }

        return r;
    }
    pub fn has_linefeed(&self) -> bool {
        if self.chars[self.end_index.0 as usize].c == Some('\n') {
            return true;
        } else {
            return false;
        }
    }
    pub fn get_chars(&self, start: LineIndex, end: LineIndex) -> Vec<TextuiCharChromatic> {
        let mut chars: Vec<TextuiCharChromatic> = vec![];
        for i in start.0..end.0 + 1 {
            chars.push(self.chars[i as usize].clone());
        }
        return chars;
    }
    pub fn get_char(&self, index: LineIndex) -> TextuiCharChromatic {
        return self.chars[index.0 as usize].clone();
    }
    pub fn set_chars(&mut self, chars: Vec<TextuiCharChromatic>, start: LineIndex, end: LineIndex) {
        for i in start.0..end.0 + 1 {
            self.chars[i as usize] = chars[i as usize].clone();
        }
    }
    pub fn set_char(&mut self, index: LineIndex, char: TextuiCharChromatic) {
        self.chars[index.0 as usize] = char;
    }
}

#[derive(Clone, Debug)]
pub enum TextuiVline {
    Chromatic(TextuiVlineChromatic),
    _Normal(TextuiVlineNormal),
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub struct WindowId(u32);

impl WindowId {
    pub fn new() -> Self {
        static MAX_ID: AtomicU32 = AtomicU32::new(0);
        return WindowId(MAX_ID.fetch_add(1, Ordering::SeqCst));
    }
}
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct Cursor {
    x: LineIndex,
    y: LineId,
}
impl Cursor {
    pub fn new(x: i32, y: i32) -> Self {
        let x = LineIndex::new(x);
        let y = LineId::new(y);
        return Cursor { x, y };
    }
    pub fn get_x(&self) -> LineIndex {
        return self.x;
    }
    pub fn get_y(&self) -> LineId {
        return self.y;
    }
    pub fn set_x(&mut self, x: i32) {
        self.x = LineIndex::new(x);
    }
    pub fn set_y(&mut self, y: i32) {
        self.y = LineId::new(y);
    }
    pub fn move_up(&mut self, n: i32) {
        self.y = self.y - n;
    }
    pub fn move_down(&mut self, n: i32) {
        self.y = self.y + n;
    }
    pub fn move_left(&mut self, n: i32) {
        self.x = self.x - n;
    }
    pub fn move_right(&mut self, n: i32) {
        self.x = self.x + n;
    }
    pub fn move_newline(&mut self, n: i32, max: i32) {
        let mut line = self.y.0 + n;
        if line < 0 {
            line = max + line;
        } else {
            line = line % max;
        }
        self.set_y(line);
        self.set_x(0);
    }
    pub fn reset(&mut self) {
        self.set_x(0);
        self.set_y(0);
    }
}
#[allow(dead_code)]
#[derive(Debug)]
pub struct TextuiWindow {
    // 虚拟行是个循环表，头和尾相接
    id: WindowId,
    // 当前已经使用了的虚拟行总数（即在已经输入到缓冲区（之后显示在屏幕上）的虚拟行数量）
    vlines_used: i32,
    // 位于最顶上的那一个虚拟行的行号
    top_vline: LineId,
    // 储存虚拟行的数组
    vlines: Vec<TextuiVline>,
    // 虚拟化数量
    vlines_num: i32,
    // 窗口flag
    flags: WindowFlag,
    // 显示窗口大小
    winsize: Winsize,
    // 光标位置（针对虚拟行）
    cursor: Cursor,
}

impl TextuiWindow {
    /// 使用参数初始化window对象
    /// ## 参数
    ///
    /// -flags 标志位
    /// -vlines_num 虚拟行的总数
    /// -chars_num 每行最大的字符数

    pub fn new(flags: WindowFlag, true_lines_num: i32, chars_num: i32, vlines_num: i32) -> Self {
        let mut initial_vlines = Vec::new();
        for _ in 0..vlines_num {
            let vline = TextuiVlineChromatic::new(chars_num as usize);

            initial_vlines.push(TextuiVline::Chromatic(vline));
        }
        TextuiWindow {
            id: WindowId::new(),
            flags,
            vlines_used: 1,
            top_vline: LineId::new(0),
            vlines: initial_vlines,
            winsize: Winsize::new(
                true_lines_num,
                chars_num,
                true_lines_num * TEXTUI_CHAR_HEIGHT as i32,
                chars_num * TEXTUI_CHAR_WIDTH as i32,
            ),
            cursor: Cursor::new(0, 0),
            vlines_num,
        }
    }

    /// 刷新某个窗口的缓冲区的某个虚拟行的连续n个字符对象
    /// ## 参数
    /// - vline_id 要刷新的虚拟行号
    /// - start 起始字符号
    /// - count 要刷新的字符数量

    fn textui_refresh_characters(
        &mut self,
        vline_id: LineId,
        start: LineIndex,
        count: i32,
    ) -> Result<(), SystemError> {
        // let actual_line_sum = textui_framework().actual_line.load(Ordering::SeqCst);

        // 判断虚拟行参数是否合法
        if unlikely(
            !vline_id.check(self.vlines_num)
                || (<LineIndex as Into<i32>>::into(start) + count) > self.winsize.col(),
        ) {
            return Err(SystemError::EINVAL);
        }
        // 计算虚拟行对应的真实行（即要渲染的行）
        // 为正说明虚拟行不在真实行显示的区域上面
        let mut actual_line_id = vline_id - self.top_vline;

        if <LineId as Into<i32>>::into(actual_line_id) < 0 {
            // 真实行数小于虚拟行数，则需要加上真实行数的位置，以便正确计算真实行
            actual_line_id = actual_line_id + self.winsize.row() as i32;
        }

        // 将此窗口的某个虚拟行的连续n个字符对象往缓存区写入
        if self.flags.contains(WindowFlag::TEXTUI_CHROMATIC) {
            let vline = &mut (self.vlines[<LineId as Into<usize>>::into(vline_id)]);
            let mut i = 0;
            let mut index = start;

            while i < count {
                if let TextuiVline::Chromatic(vline) = vline {
                    vline.chars[<LineIndex as Into<usize>>::into(index)]
                        .textui_refresh_character(actual_line_id, index)?;

                    index = index + 1;
                }
                i += 1;
            }
        }

        return Ok(());
    }

    /// 重新渲染某个窗口的某个虚拟行
    /// ## 参数

    /// - window 窗口结构体
    /// - vline_id 虚拟行号

    fn textui_refresh_vline(&mut self, vline_id: LineId) -> Result<(), SystemError> {
        if self.flags.contains(WindowFlag::TEXTUI_CHROMATIC) {
            return self.textui_refresh_characters(vline_id, LineIndex::new(0), self.winsize.col());
        } else {
            //todo支持纯文本字符()
            todo!();
        }
    }

    // 刷新某个窗口的start 到start + count行（即将这些行输入到缓冲区）
    fn textui_refresh_vlines(&mut self, start: LineId, count: i32) -> Result<i32, SystemError> {
        let mut refresh_count = count;
        for i in <LineId as Into<i32>>::into(start)
            ..(self.winsize.row()).min(<LineId as Into<i32>>::into(start) + count)
        {
            self.textui_refresh_vline(LineId::new(i))?;
            refresh_count -= 1;
        }
        // 因为虚拟行是循环表
        let mut refresh_start = 0;
        while refresh_count > 0 {
            self.textui_refresh_vline(LineId::new(refresh_start))?;
            refresh_start += 1;
            refresh_count -= 1;
        }
        return Ok(0);
    }

    /// 往窗口的某个虚拟行之后插入新的一行
    /// ## 参数
    /// - window 窗口结构体
    /// - vline 虚拟行号

    fn textui_new_line(&mut self, vline_id: LineId) -> Result<i32, SystemError> {
        // let actual_line_sum = textui_framework().actual_line.load(Ordering::SeqCst);
        let new_vline_id = vline_id + 1;
        // 将下面的虚拟行往下移
        let mut move_lines: Vec<TextuiVline> = vec![];
        let mut end_id: LineId = new_vline_id;
        let mut is_found = false;
        for i in <LineId as Into<i32>>::into(new_vline_id)..(self.winsize.row()) {
            if let TextuiVline::Chromatic(vline) = &self.vlines[i as usize] {
                if vline.is_empty {
                    is_found = true;
                    break;
                } else {
                    end_id = end_id + 1;
                    move_lines.push(self.vlines[i as usize].clone());
                }
            }
        }
        if !is_found {
            end_id = LineId::new(0);
            for i in 0..<LineId as Into<i32>>::into(new_vline_id) {
                if let TextuiVline::Chromatic(vline) = &self.vlines[i as usize] {
                    if vline.is_empty {
                        is_found = true;
                        break;
                    } else {
                        end_id = end_id + 1;
                        move_lines.push(self.vlines[i as usize].clone());
                    }
                }
            }
        }
        let mut j: usize = 0;
        if end_id > new_vline_id {
            for i in <LineId as Into<i32>>::into(new_vline_id + 1)
                ..<LineId as Into<i32>>::into(end_id + 1)
            {
                self.vlines[i as usize] = move_lines[j].clone();
                j += 1;
            }
        } else if end_id < new_vline_id {
            for i in <LineId as Into<i32>>::into(new_vline_id + 1)..self.winsize.row() + 1 {
                self.vlines[i as usize] = move_lines[j].clone();
                j += 1;
            }
            for i in 0..<LineId as Into<i32>>::into(end_id + 1) {
                self.vlines[i as usize] = move_lines[j].clone();
                j += 1;
            }
        } else if !is_found {
            for i in <LineId as Into<i32>>::into(new_vline_id + 1)..self.winsize.row() + 1 {
                self.vlines[i as usize] = move_lines[j].clone();
                j += 1;
            }
            for i in 0..<LineId as Into<i32>>::into(new_vline_id) {
                self.vlines[i as usize] = move_lines[j].clone();
                j += 1;
            }
        }

        // 增加新行
        let num = self.winsize.col();
        if let TextuiVline::Chromatic(vline) = self.get_mut_vline(new_vline_id) {
            vline.is_empty = true;
            for i in 0..num {
                if let Some(v_char) = vline.chars.get_mut(i as usize) {
                    v_char.c = None;
                    v_char.frcolor = FontColor::BLACK;
                    v_char.bkcolor = FontColor::BLACK;
                }
            }
            vline.end_index = LineIndex::new(0);
            self.cursor.set_x(0);
        }

        // 当已经使用的虚拟行总数等于真实行总数时，说明窗口中已经显示的文本行数已经达到了窗口的最大容量。这时，如果继续在窗口中添加新的文本，就会导致文本溢出窗口而无法显示。因此，需要往下滚动屏幕来显示更多的文本。
        if self.vlines_used == self.winsize.row() {
            self.top_vline = self.top_vline + 1;

            if !self.top_vline.check(self.vlines_num) {
                self.top_vline = LineId::new(0);
            }

            // 刷新所有行
            self.textui_refresh_vlines(self.top_vline, self.winsize.row())?;
        } else {
            // 换行说明上一行已经在缓冲区中，所以已经使用的虚拟行总数+1
            self.vlines_used += 1;
        }

        return Ok(0);
    }

    /// 真正向窗口的缓冲区上输入字符的函数(位置为window.cursor.get_y()，window.cursor.get_x())
    fn true_textui_putchar_window(
        &mut self,
        character: char,
        frcolor: FontColor,
        bkcolor: FontColor,
    ) -> Result<(), SystemError> {
        // 启用彩色字符
        if self.flags.contains(WindowFlag::TEXTUI_CHROMATIC) {
            let mut line_index = LineIndex::new(0); // 操作的列号
            let index = <LineIndex as Into<usize>>::into(self.cursor.get_x());

            if let TextuiVline::Chromatic(vline) = self.get_mut_vline(self.cursor.get_y()) {
                vline.is_empty = false;

                if let Some(v_char) = vline.chars.get_mut(index) {
                    v_char.c = Some(character);
                    v_char.frcolor = frcolor;
                    v_char.bkcolor = bkcolor;
                }
                line_index = vline.end_index;
                vline.end_index = vline.end_index + 1;
                self.cursor.move_right(1);
            }

            // 存储换行符后进行换行操作
            if character == '\n' {
                // 换行时还需要输出\r
                send_to_default_serial8250_port(&[b'\r']);
                self.textui_new_line(self.cursor.get_y())?;
                self.cursor.move_newline(1, self.winsize.row());
                return Ok(());
            }
            self.textui_refresh_characters(self.cursor.get_y(), line_index, 1)?;

            // 加入光标后，因为会识别光标，所以需超过该行最大字符数才能创建新行
            if !line_index.check(self.winsize.col()) {
                self.textui_new_line(self.cursor.get_y())?;
                self.cursor.move_newline(1, self.winsize.row());
            }
        } else {
            // todo: 支持纯文本字符
            todo!();
        }
        return Ok(());
    }
    /// 根据输入的一个字符在窗口上输出
    /// ## 参数

    /// - window 窗口
    /// - character 字符
    /// - FRcolor 前景色（RGB）
    /// - BKcolor 背景色（RGB）

    fn textui_putchar_window(
        &mut self,
        character: char,
        frcolor: FontColor,
        bkcolor: FontColor,
        is_enable_window: bool,
    ) -> Result<(), SystemError> {
        let actual_line_sum = textui_framework().actual_line.load(Ordering::SeqCst);

        //字符'\0'代表ASCII码表中的空字符,表示字符串的结尾
        if unlikely(character == '\0') {
            return Ok(());
        }

        if unlikely(character == '\r') {
            return Ok(());
        }

        // 暂不支持纯文本窗口
        if !self.flags.contains(WindowFlag::TEXTUI_CHROMATIC) {
            return Ok(());
        }

        //进行换行操作
        if character == '\n' {
            // 换行时还需要输出\r
            send_to_default_serial8250_port(&[b'\r']);
            if is_enable_window == true {
                self.textui_new_line(self.cursor.get_y())?;
                self.cursor.move_newline(1, self.winsize.row());
            }
            return Ok(());
        }
        // 输出制表符
        else if character == '\t' {
            if is_enable_window == true {
                if let TextuiVline::Chromatic(vline) = self.get_vline(self.cursor.get_y()) {
                    //打印的空格数（注意将每行分成一个个表格，每个表格为8个字符）
                    let mut space_to_print =
                        8 - <LineIndex as Into<usize>>::into(vline.end_index) % 8;
                    while space_to_print > 0 {
                        self.true_textui_putchar_window(' ', frcolor, bkcolor)?;
                        space_to_print -= 1;
                    }
                }
            }
        }
        // 字符 '\x08' 代表 ASCII 码中的退格字符。它在输出中的作用是将光标向左移动一个位置，并在该位置上输出后续的字符，从而实现字符的删除或替换。
        else if character == '\x08' {
            if is_enable_window == true {
                let mut tmp = LineIndex(0);
                if let TextuiVline::Chromatic(vline) = self.get_mut_vline(self.cursor.get_y()) {
                    vline.end_index = vline.end_index - 1;
                    tmp = vline.end_index;
                }
                self.cursor.move_left(1);
                if <LineIndex as Into<i32>>::into(tmp) >= 0 {
                    if let TextuiVline::Chromatic(vline) = self.get_mut_vline(self.cursor.get_y()) {
                        if let Some(v_char) =
                            vline.chars.get_mut(<LineIndex as Into<usize>>::into(tmp))
                        {
                            v_char.c = Some(' ');

                            v_char.bkcolor = bkcolor;
                        }
                    }
                    return self.textui_refresh_characters(self.cursor.get_y(), tmp, 1);
                }
                // 需要向上缩一行
                if <LineIndex as Into<i32>>::into(tmp) < 0 {
                    // 当前行为空,需要重新刷新
                    let num = self.winsize.col();
                    if let TextuiVline::Chromatic(vline) = self.get_mut_vline(self.cursor.get_y()) {
                        vline.end_index = LineIndex::new(0);
                        for i in 0..num {
                            if let Some(v_char) = vline.chars.get_mut(i as usize) {
                                v_char.c = None;
                                v_char.frcolor = FontColor::BLACK;
                                v_char.bkcolor = FontColor::BLACK;
                            }
                        }
                    }
                    self.cursor.set_x(0);

                    // 上缩一行
                    // self.vline_operating = self.vline_operating - 1;
                    self.cursor.move_newline(-1, self.winsize.row());

                    // 考虑是否向上滚动（在top_vline上退格）
                    if self.vlines_used > actual_line_sum {
                        self.top_vline = self.top_vline - 1;
                        if <LineId as Into<i32>>::into(self.top_vline) < 0 {
                            self.top_vline = LineId(self.winsize.row() - 1);
                        }
                    }
                    //因为上缩一行所以显示在屏幕中的虚拟行少一
                    self.vlines_used -= 1;
                    self.textui_refresh_vlines(self.top_vline, actual_line_sum)?;
                }
            }
        } else {
            // 输出其他字符

            send_to_default_serial8250_port(&[character as u8]);

            if is_enable_window == true {
                if let TextuiVline::Chromatic(vline) = self.get_vline(self.cursor.get_y()) {
                    if !vline.end_index.check(self.winsize.col()) {
                        self.textui_new_line(self.cursor.get_y())?;
                        self.cursor.move_newline(1, self.winsize.row());
                    }

                    return self.true_textui_putchar_window(character, frcolor, bkcolor);
                }
            }
        }

        return Ok(());
    }
    /// 窗口闪烁显示光标
    pub fn show_cursor_window(&mut self) {}

    /// 得到窗口某一虚拟行的不可变引用
    pub fn get_vline(&self, vline_id: LineId) -> &TextuiVline {
        let vline = &((self.vlines)[<LineId as Into<usize>>::into(vline_id)]);
        return vline;
    }

    /// 得到窗口某一虚拟行的可变引用
    pub fn get_mut_vline(&mut self, vline_id: LineId) -> &mut TextuiVline {
        &mut ((self.vlines)[<LineId as Into<usize>>::into(vline_id)])
    }

    /// 将某虚拟行的字符从start_index之后开始向左移一个字符,占据start_index位置
    pub fn chars_move_left(
        &mut self,
        vline_id: LineId,
        start_index: LineIndex,
    ) -> Result<(), SystemError> {
        let num = self.winsize.col();
        let mut need_move_next_vline = false;
        let mut chars: Vec<TextuiCharChromatic> = Vec::new();
        if let TextuiVline::Chromatic(vline) = self.get_mut_vline(vline_id) {
            chars = vline.get_chars(start_index + 1, vline.end_index);

            if !vline.end_index.check(num - 1) {
                if vline.has_linefeed() {
                    vline.set_chars(chars.clone(), start_index, vline.end_index - 1);
                    vline.set_char(
                        vline.end_index,
                        TextuiCharChromatic::new(Some(' '), FontColor::BLACK, FontColor::BLACK),
                    );
                    vline.end_index = vline.end_index - 1;
                    self.textui_refresh_vline(vline_id)?;
                } else {
                    need_move_next_vline = true;
                }
            } else {
                vline.end_index = vline.end_index - 1;
                vline.set_chars(chars.clone(), start_index, vline.end_index);
                self.textui_refresh_vline(vline_id)?;
            }
        }
        if need_move_next_vline {
            let mut move_chars = chars.clone();
            let mut is_empty = true;
            if let TextuiVline::Chromatic(next_vline) = self.get_vline(vline_id + 1) {
                if !next_vline.is_empty {
                    is_empty = false;
                    let char = next_vline.get_char(LineIndex::new(0));
                    move_chars.push(char);
                }
            }
            if !is_empty {
                self.chars_move_left(vline_id + 1, LineIndex::new(0))?;
                if let TextuiVline::Chromatic(vline) = self.get_mut_vline(vline_id) {
                    vline.set_chars(move_chars, start_index, vline.end_index);
                }
                self.textui_refresh_vlines(vline_id, 2)?;
            }
        }
        return Ok(());
    }

    /// 将某虚拟行的字符从start_index开始向右移一个字符,空出start_index位置
    pub fn chars_move_right(
        &mut self,
        vline_id: LineId,
        start_index: LineIndex,
    ) -> Result<(), SystemError> {
        let num = self.winsize.col();
        if let TextuiVline::Chromatic(vline) = self.get_mut_vline(vline_id) {
            let chars: Vec<TextuiCharChromatic> = vline.get_chars(start_index, vline.end_index);
            if !vline.end_index.check(num - 1) {
                let mut move_chars = chars.clone();
                let char = move_chars.pop().unwrap();
                if vline.has_linefeed() {
                    vline.set_chars(move_chars, start_index + 1, vline.end_index);
                    self.textui_new_line(vline_id)?;
                    if let TextuiVline::Chromatic(next_vline) = self.get_mut_vline(vline_id + 1) {
                        next_vline.is_empty = false;
                        next_vline.set_char(LineIndex::new(0), char);
                    }
                } else {
                    vline.set_chars(move_chars, start_index + 1, vline.end_index);
                    let mut is_empty = false;
                    if let TextuiVline::Chromatic(next_vline) = self.get_mut_vline(vline_id + 1) {
                        if next_vline.is_empty {
                            is_empty = true;
                            next_vline.is_empty = false;
                            next_vline.set_char(LineIndex::new(0), char);
                            next_vline.end_index = next_vline.end_index + 1;
                        }
                    }
                    if !is_empty {
                        self.chars_move_right(vline_id + 1, LineIndex::new(0))?;
                        if let TextuiVline::Chromatic(next_vline) = self.get_mut_vline(vline_id + 1)
                        {
                            next_vline.set_char(LineIndex::new(0), char);
                        }
                    }
                }
                self.textui_refresh_vlines(vline_id, 2)?;
            } else {
                vline.end_index = vline.end_index + 1;
                vline.set_chars(chars, start_index, vline.end_index);
                self.textui_refresh_vline(vline_id)?;
            }
        }
        return Ok(());
    }
    /// 用户使用该函数通过方向键在任意位置进行输入字符
    pub fn textui_putchar_window_for_user(
        &mut self,
        character: char,
        frcolor: FontColor,
        bkcolor: FontColor,
        is_enable_window: bool,
    ) -> Result<(), SystemError> {
        let u = char::from(72);
        let d = char::from(80);
        let l = char::from(75);
        let r = char::from(77);
        if character == u {
            self.chars_move_left(self.cursor.get_y(), self.cursor.get_x())?;

            self.chars_move_right(self.cursor.get_y() - 1, self.cursor.get_x())?;

            self.cursor.move_up(1);
        } else if character == d {
            self.chars_move_left(self.cursor.get_y(), self.cursor.get_x())?;

            self.chars_move_right(self.cursor.get_y() + 1, self.cursor.get_x())?;

            self.cursor.move_down(1);
        } else if character == l {
            self.chars_move_left(self.cursor.get_y(), self.cursor.get_x())?;

            self.chars_move_right(self.cursor.get_y(), self.cursor.get_x() - 1)?;

            self.cursor.move_left(1);
        } else if character == r {
            self.chars_move_left(self.cursor.get_y(), self.cursor.get_x())?;

            self.chars_move_right(self.cursor.get_y(), self.cursor.get_x() + 1)?;

            self.cursor.move_right(1);
        } else {
            self.textui_putchar_window(character, frcolor, bkcolor, is_enable_window)?;
        }
        return Ok(());
    }
}
impl Default for TextuiWindow {
    fn default() -> Self {
        TextuiWindow {
            id: WindowId(0),
            flags: WindowFlag::TEXTUI_CHROMATIC,
            vlines_used: 1,
            top_vline: LineId::new(0),
            vlines: Vec::new(),
            winsize: Winsize::new(0, 0, 0, 0),
            cursor: Cursor::new(0, 0),
            vlines_num: 0,
        }
    }
}
#[allow(dead_code)]
#[derive(Debug)]
pub struct TextUiFramework {
    metadata: RwLock<ScmUiFrameworkMetadata>,
    window_list: Arc<SpinLock<LinkedList<Arc<SpinLock<TextuiWindow>>>>>,
    actual_line: AtomicI32, // 真实行的数量（textui的帧缓冲区能容纳的内容的行数）
    current_window: Arc<SpinLock<TextuiWindow>>, // 当前的主窗口
    default_window: Arc<SpinLock<TextuiWindow>>, // 默认print到的窗口
}

impl TextUiFramework {
    pub fn new(
        metadata: ScmUiFrameworkMetadata,
        window_list: Arc<SpinLock<LinkedList<Arc<SpinLock<TextuiWindow>>>>>,
        current_window: Arc<SpinLock<TextuiWindow>>,
        default_window: Arc<SpinLock<TextuiWindow>>,
    ) -> Self {
        let actual_line =
            AtomicI32::new((&metadata.buf_info().height() / TEXTUI_CHAR_HEIGHT) as i32);
        let inner = TextUiFramework {
            metadata: RwLock::new(metadata),
            window_list,
            actual_line,
            current_window,
            default_window,
        };
        return inner;
    }
}

impl ScmUiFramework for TextUiFramework {
    // 安装ui框架的回调函数
    fn install(&self) -> Result<i32, SystemError> {
        send_to_default_serial8250_port("\ntextui_install_handler\n\0".as_bytes());
        return Ok(0);
    }
    // 卸载ui框架的回调函数
    fn uninstall(&self) -> Result<i32, SystemError> {
        return Ok(0);
    }
    // 启用ui框架的回调函数
    fn enable(&self) -> Result<i32, SystemError> {
        ENABLE_PUT_TO_WINDOW.store(true, Ordering::SeqCst);
        return Ok(0);
    }
    // 禁用ui框架的回调函数
    fn disable(&self) -> Result<i32, SystemError> {
        ENABLE_PUT_TO_WINDOW.store(false, Ordering::SeqCst);

        return Ok(0);
    }
    // 改变ui框架的帧缓冲区的回调函数
    fn change(&self, buf_info: ScmBufferInfo) -> Result<i32, SystemError> {
        let old_buf = textui_framework().metadata.read().buf_info();

        textui_framework().metadata.write().set_buf_info(buf_info);

        let mut new_buf = textui_framework().metadata.read().buf_info();

        new_buf.copy_from_nonoverlapping(&old_buf);
        kdebug!("textui change buf_info: old: {:?}", old_buf);
        kdebug!("textui change buf_info: new: {:?}", new_buf);

        return Ok(0);
    }
    ///  获取ScmUiFramework的元数据
    ///  ## 返回值
    ///
    ///  -成功：Ok(ScmUiFramework的元数据)
    ///  -失败：Err(错误码)
    fn metadata(&self) -> Result<ScmUiFrameworkMetadata, SystemError> {
        let metadata = self.metadata.read().clone();

        return Ok(metadata);
    }
}

/// Mapping from characters to glyph indices.
pub trait GlyphMapping: Sync {
    /// Maps a character to a glyph index.
    ///
    /// If `c` isn't included in the font the index of a suitable replacement glyph is returned.
    fn index(&self, c: char) -> usize;
}

impl<F> GlyphMapping for F
where
    F: Sync + Fn(char) -> usize,
{
    fn index(&self, c: char) -> usize {
        self(c)
    }
}

/// 在默认窗口上输出一个字符
/// ## 参数
/// - character 字符
/// - FRcolor 前景色（RGB）
/// - BKcolor 背景色（RGB）

#[no_mangle]
pub extern "C" fn rs_textui_putchar(character: u8, fr_color: u32, bk_color: u32) -> i32 {
    return textui_putchar(
        character as char,
        FontColor::from(fr_color),
        FontColor::from(bk_color),
    )
    .map(|_| 0)
    .unwrap_or_else(|e| e.to_posix_errno());
}

pub fn textui_putchar(
    character: char,
    fr_color: FontColor,
    bk_color: FontColor,
) -> Result<(), SystemError> {
    if unsafe { TEXTUI_IS_INIT } {
        return textui_framework()
            .current_window
            .lock()
            .textui_putchar_window(
                character,
                fr_color,
                bk_color,
                ENABLE_PUT_TO_WINDOW.load(Ordering::SeqCst),
            );
    } else {
        //未初始化暴力输出
        return no_init_textui_putchar_window(
            character,
            fr_color,
            bk_color,
            ENABLE_PUT_TO_WINDOW.load(Ordering::SeqCst),
        );
    }
}

/// 初始化text ui框架

#[no_mangle]
pub extern "C" fn rs_textui_init() -> i32 {
    let r = textui_init().unwrap_or_else(|e| e.to_posix_errno());
    if r.is_negative() {
        send_to_default_serial8250_port("textui init failed.\n\0".as_bytes());
    }
    return r;
}

fn textui_init() -> Result<i32, SystemError> {
    unsafe { textui_framwork_init() };

    return Ok(0);
}
