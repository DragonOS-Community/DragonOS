use crate::{
    driver::{
        serial::serial8250::send_to_default_serial8250_port,
        tty::{tty_port::TTY_PORTS, virtual_terminal::virtual_console::CURRENT_VCNUM},
        video::video_refresh_manager,
    },
    kdebug, kinfo,
    libs::{
        lib_ui::font::FONT_8x16,
        rwlock::RwLock,
        spinlock::{SpinLock, SpinLockGuard},
    },
};
use alloc::{boxed::Box, collections::LinkedList, string::ToString};
use alloc::{sync::Arc, vec::Vec};
use core::{
    fmt::Debug,
    intrinsics::unlikely,
    ops::{Add, AddAssign, Sub},
    sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering},
};
use system_error::SystemError;

use super::{
    screen_manager::{
        scm_register, ScmBuffer, ScmBufferInfo, ScmFramworkType, ScmUiFramework,
        ScmUiFrameworkMetadata,
    },
    textui_no_alloc::no_init_textui_putchar_window,
};

/// 声明全局的TEXTUI_FRAMEWORK
static mut __TEXTUI_FRAMEWORK: Option<Arc<TextUiFramework>> = None;

/// 每个字符的宽度和高度（像素）
pub const TEXTUI_CHAR_WIDTH: u32 = 8;

pub const TEXTUI_CHAR_HEIGHT: u32 = 16;

pub static mut TEXTUI_IS_INIT: bool = false;

static ENABLE_PUT_TO_WINDOW: AtomicBool = AtomicBool::new(false);

/// 启用将文本输出到窗口的功能。
pub fn textui_enable_put_to_window() {
    ENABLE_PUT_TO_WINDOW.store(true, Ordering::SeqCst);
}

/// 禁用将文本输出到窗口的功能。
pub fn textui_disable_put_to_window() {
    ENABLE_PUT_TO_WINDOW.store(false, Ordering::SeqCst);
}

/// 检查是否启用了将文本输出到窗口的功能。
///
/// # 返回
/// 如果启用了将文本输出到窗口的功能，则返回 `true`，否则返回 `false`。
pub fn textui_is_enable_put_to_window() -> bool {
    ENABLE_PUT_TO_WINDOW.load(Ordering::SeqCst)
}

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
fn textui_framwork_init() {
    if unsafe { __TEXTUI_FRAMEWORK.is_none() } {
        kinfo!("textuiframework init");
        let metadata = ScmUiFrameworkMetadata::new("TextUI".to_string(), ScmFramworkType::Text);
        kdebug!("textui metadata: {:?}", metadata);
        // 为textui框架生成第一个窗口
        let vlines_num = (metadata.buf_info().height() / TEXTUI_CHAR_HEIGHT) as usize;

        let chars_num = (metadata.buf_info().width() / TEXTUI_CHAR_WIDTH) as usize;

        let initial_window = TextuiWindow::new(
            WindowFlag::TEXTUI_CHROMATIC,
            vlines_num as i32,
            chars_num as i32,
        );

        let current_window: Arc<SpinLock<TextuiWindow>> = Arc::new(SpinLock::new(initial_window));

        let default_window = current_window.clone();

        // 生成窗口链表，并把上面窗口添加进textui框架的窗口链表中
        let window_list: Arc<SpinLock<LinkedList<Arc<SpinLock<TextuiWindow>>>>> =
            Arc::new(SpinLock::new(LinkedList::new()));
        window_list.lock().push_back(current_window.clone());

        unsafe {
            __TEXTUI_FRAMEWORK = Some(Arc::new(TextUiFramework::new(
                metadata,
                window_list,
                current_window,
                default_window,
            )))
        };

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
    index: LineIndex,                // 当前操作的位置
}
impl TextuiVlineChromatic {
    pub fn new(char_num: usize) -> Self {
        let mut r = TextuiVlineChromatic {
            chars: Vec::with_capacity(char_num),
            index: LineIndex::new(0),
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
pub struct TextuiWindow {
    // 虚拟行是个循环表，头和尾相接
    id: WindowId,
    // 虚拟行总数
    vline_sum: i32,
    // 当前已经使用了的虚拟行总数（即在已经输入到缓冲区（之后显示在屏幕上）的虚拟行数量）
    vlines_used: i32,
    // 位于最顶上的那一个虚拟行的行号
    top_vline: LineId,
    // 储存虚拟行的数组
    vlines: Vec<TextuiVline>,
    // 正在操作的vline
    vline_operating: LineId,
    // 每行最大容纳的字符数
    chars_per_line: i32,
    // 窗口flag
    flags: WindowFlag,
}

impl TextuiWindow {
    /// 使用参数初始化window对象
    /// ## 参数
    ///
    /// -flags 标志位
    /// -vlines_num 虚拟行的总数
    /// -chars_num 每行最大的字符数

    pub fn new(flags: WindowFlag, vlines_num: i32, chars_num: i32) -> Self {
        let mut initial_vlines = Vec::new();

        for _ in 0..vlines_num {
            let vline = TextuiVlineChromatic::new(chars_num as usize);

            initial_vlines.push(TextuiVline::Chromatic(vline));
        }
        TextuiWindow {
            id: WindowId::new(),
            flags,
            vline_sum: vlines_num,
            vlines_used: 1,
            top_vline: LineId::new(0),
            vlines: initial_vlines,
            vline_operating: LineId::new(0),
            chars_per_line: chars_num,
        }
    }

    /// 刷新某个窗口的缓冲区的某个虚拟行的连续n个字符对象
    /// ## 参数
    /// - window 窗口结构体
    /// - vline_id 要刷新的虚拟行号
    /// - start 起始字符号
    /// - count 要刷新的字符数量

    fn textui_refresh_characters(
        &mut self,
        vline_id: LineId,
        start: LineIndex,
        count: i32,
    ) -> Result<(), SystemError> {
        let actual_line_sum = textui_framework().actual_line.load(Ordering::SeqCst);

        // 判断虚拟行参数是否合法
        if unlikely(
            !vline_id.check(self.vline_sum)
                || (<LineIndex as Into<i32>>::into(start) + count) > self.chars_per_line,
        ) {
            return Err(SystemError::EINVAL);
        }
        // 计算虚拟行对应的真实行（即要渲染的行）
        let mut actual_line_id = vline_id - self.top_vline; //为正说明虚拟行不在真实行显示的区域上面

        if <LineId as Into<i32>>::into(actual_line_id) < 0 {
            //真实行数小于虚拟行数，则需要加上真实行数的位置，以便正确计算真实行
            actual_line_id = actual_line_id + actual_line_sum;
        }

        // 将此窗口的某个虚拟行的连续n个字符对象往缓存区写入
        if self.flags.contains(WindowFlag::TEXTUI_CHROMATIC) {
            let vline = &mut self.vlines[<LineId as Into<usize>>::into(vline_id)];
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
            return self.textui_refresh_characters(
                vline_id,
                LineIndex::new(0),
                self.chars_per_line,
            );
        } else {
            //todo支持纯文本字符()
            todo!();
        }
    }

    // 刷新某个窗口的start 到start + count行（即将这些行输入到缓冲区）
    fn textui_refresh_vlines(&mut self, start: LineId, count: i32) -> Result<i32, SystemError> {
        let mut refresh_count = count;
        for i in <LineId as Into<i32>>::into(start)
            ..(self.vline_sum).min(<LineId as Into<i32>>::into(start) + count)
        {
            self.textui_refresh_vline(LineId::new(i))?;
            refresh_count -= 1;
        }
        //因为虚拟行是循环表
        let mut refresh_start = 0;
        while refresh_count > 0 {
            self.textui_refresh_vline(LineId::new(refresh_start))?;
            refresh_start += 1;
            refresh_count -= 1;
        }
        return Ok(0);
    }

    /// 往某个窗口的缓冲区的某个虚拟行插入换行
    /// ## 参数
    /// - window 窗口结构体
    /// - vline_id 虚拟行号

    fn textui_new_line(&mut self) -> Result<i32, SystemError> {
        // todo: 支持在两个虚拟行之间插入一个新行
        let actual_line_sum = textui_framework().actual_line.load(Ordering::SeqCst);
        self.vline_operating = self.vline_operating + 1;
        //如果已经到了最大行数，则重新从0开始
        if !self.vline_operating.check(self.vline_sum) {
            self.vline_operating = LineId::new(0);
        }

        if let TextuiVline::Chromatic(vline) =
            &mut (self.vlines[<LineId as Into<usize>>::into(self.vline_operating)])
        {
            for i in 0..self.chars_per_line {
                if let Some(v_char) = vline.chars.get_mut(i as usize) {
                    v_char.c = None;
                    v_char.frcolor = FontColor::BLACK;
                    v_char.bkcolor = FontColor::BLACK;
                }
            }
            vline.index = LineIndex::new(0);
        }
        // 当已经使用的虚拟行总数等于真实行总数时，说明窗口中已经显示的文本行数已经达到了窗口的最大容量。这时，如果继续在窗口中添加新的文本，就会导致文本溢出窗口而无法显示。因此，需要往下滚动屏幕来显示更多的文本。

        if self.vlines_used == actual_line_sum {
            self.top_vline = self.top_vline + 1;

            if !self.top_vline.check(self.vline_sum) {
                self.top_vline = LineId::new(0);
            }

            // 刷新所有行
            self.textui_refresh_vlines(self.top_vline, actual_line_sum)?;
        } else {
            //换行说明上一行已经在缓冲区中，所以已经使用的虚拟行总数+1
            self.vlines_used += 1;
        }

        return Ok(0);
    }

    /// 真正向窗口的缓冲区上输入字符的函数(位置为window.vline_operating，window.vline_operating.index)
    /// ## 参数
    /// - window
    /// - character

    fn true_textui_putchar_window(
        &mut self,
        character: char,
        frcolor: FontColor,
        bkcolor: FontColor,
    ) -> Result<(), SystemError> {
        // 启用彩色字符
        if self.flags.contains(WindowFlag::TEXTUI_CHROMATIC) {
            let mut line_index = LineIndex::new(0); //操作的列号
            if let TextuiVline::Chromatic(vline) =
                &mut (self.vlines[<LineId as Into<usize>>::into(self.vline_operating)])
            {
                let index = <LineIndex as Into<usize>>::into(vline.index);

                if let Some(v_char) = vline.chars.get_mut(index) {
                    v_char.c = Some(character);
                    v_char.frcolor = frcolor;
                    v_char.bkcolor = bkcolor;
                }
                line_index = vline.index;
                vline.index = vline.index + 1;
            }

            self.textui_refresh_characters(self.vline_operating, line_index, 1)?;

            // 加入光标后，因为会识别光标，所以需超过该行最大字符数才能创建新行
            if !line_index.check(self.chars_per_line - 1) {
                self.textui_new_line()?;
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
        send_to_default_serial8250_port(&[character as u8]);

        //进行换行操作
        if character == '\n' {
            // 换行时还需要输出\r
            send_to_default_serial8250_port(&[b'\r']);
            if is_enable_window == true {
                self.textui_new_line()?;
            }
            return Ok(());
        }
        // 输出制表符
        else if character == '\t' {
            if is_enable_window == true {
                if let TextuiVline::Chromatic(vline) =
                    &self.vlines[<LineId as Into<usize>>::into(self.vline_operating)]
                {
                    //打印的空格数（注意将每行分成一个个表格，每个表格为8个字符）
                    let mut space_to_print = 8 - <LineIndex as Into<usize>>::into(vline.index) % 8;
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
                if let TextuiVline::Chromatic(vline) =
                    &mut self.vlines[<LineId as Into<usize>>::into(self.vline_operating)]
                {
                    vline.index = vline.index - 1;
                    tmp = vline.index;
                }
                if <LineIndex as Into<i32>>::into(tmp) >= 0 {
                    if let TextuiVline::Chromatic(vline) =
                        &mut self.vlines[<LineId as Into<usize>>::into(self.vline_operating)]
                    {
                        if let Some(v_char) =
                            vline.chars.get_mut(<LineIndex as Into<usize>>::into(tmp))
                        {
                            v_char.c = Some(' ');

                            v_char.bkcolor = bkcolor;
                        }
                    }
                    return self.textui_refresh_characters(self.vline_operating, tmp, 1);
                }
                // 需要向上缩一行
                if <LineIndex as Into<i32>>::into(tmp) < 0 {
                    // 当前行为空,需要重新刷新
                    if let TextuiVline::Chromatic(vline) =
                        &mut self.vlines[<LineId as Into<usize>>::into(self.vline_operating)]
                    {
                        vline.index = LineIndex::new(0);
                        for i in 0..self.chars_per_line {
                            if let Some(v_char) = vline.chars.get_mut(i as usize) {
                                v_char.c = None;
                                v_char.frcolor = FontColor::BLACK;
                                v_char.bkcolor = FontColor::BLACK;
                            }
                        }
                    }
                    // 上缩一行
                    self.vline_operating = self.vline_operating - 1;
                    if self.vline_operating.data() < 0 {
                        self.vline_operating = LineId(self.vline_sum - 1);
                    }

                    // 考虑是否向上滚动（在top_vline上退格）
                    if self.vlines_used > actual_line_sum {
                        self.top_vline = self.top_vline - 1;
                        if <LineId as Into<i32>>::into(self.top_vline) < 0 {
                            self.top_vline = LineId(self.vline_sum - 1);
                        }
                    }
                    //因为上缩一行所以显示在屏幕中的虚拟行少一
                    self.vlines_used -= 1;
                    self.textui_refresh_vlines(self.top_vline, actual_line_sum)?;
                }
            }
        } else {
            if is_enable_window == true {
                if let TextuiVline::Chromatic(vline) =
                    &self.vlines[<LineId as Into<usize>>::into(self.vline_operating)]
                {
                    if !vline.index.check(self.chars_per_line) {
                        self.textui_new_line()?;
                    }

                    return self.true_textui_putchar_window(character, frcolor, bkcolor);
                }
            }
        }

        return Ok(());
    }
}
impl Default for TextuiWindow {
    fn default() -> Self {
        TextuiWindow {
            id: WindowId(0),
            flags: WindowFlag::TEXTUI_CHROMATIC,
            vline_sum: 0,
            vlines_used: 1,
            top_vline: LineId::new(0),
            vlines: Vec::new(),
            vline_operating: LineId::new(0),
            chars_per_line: 0,
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
        textui_enable_put_to_window();
        return Ok(0);
    }
    // 禁用ui框架的回调函数
    fn disable(&self) -> Result<i32, SystemError> {
        textui_disable_put_to_window();

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
    let current_vcnum = CURRENT_VCNUM.load(Ordering::SeqCst);
    if current_vcnum != -1 {
        // tty已经初始化了之后才输出到屏幕
        let fr = (fr_color & 0x00ff0000) >> 16;
        let fg = (fr_color & 0x0000ff00) >> 8;
        let fb = fr_color & 0x000000ff;
        let br = (bk_color & 0x00ff0000) >> 16;
        let bg = (bk_color & 0x0000ff00) >> 8;
        let bb = bk_color & 0x000000ff;
        let buf = format!(
            "\x1B[38;2;{fr};{fg};{fb};48;2;{br};{bg};{bb}m{}\x1B[0m",
            character as char
        );
        let port = TTY_PORTS[current_vcnum as usize].clone();
        let tty = port.port_data().tty();
        if tty.is_some() {
            let tty = tty.unwrap();
            send_to_default_serial8250_port(&[character]);
            return tty
                .write_without_serial(buf.as_bytes(), buf.len())
                .map(|_| 0)
                .unwrap_or_else(|e| e.to_posix_errno());
        }
    }
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
            .lock_irqsave()
            .textui_putchar_window(
                character,
                fr_color,
                bk_color,
                textui_is_enable_put_to_window(),
            );
    } else {
        //未初始化暴力输出
        return no_init_textui_putchar_window(
            character,
            fr_color,
            bk_color,
            textui_is_enable_put_to_window(),
        );
    }
}

/// 向默认窗口输出一个字符串
pub fn textui_putstr(
    string: &str,
    fr_color: FontColor,
    bk_color: FontColor,
) -> Result<(), SystemError> {
    let window = if unsafe { TEXTUI_IS_INIT } {
        let fw = textui_framework();
        let w = fw.current_window.clone();
        Some(w)
    } else {
        None
    };

    let mut guard = window.as_ref().map(|w| w.lock_irqsave());

    for character in string.chars() {
        if unsafe { TEXTUI_IS_INIT } {
            guard.as_mut().unwrap().textui_putchar_window(
                character,
                fr_color,
                bk_color,
                textui_is_enable_put_to_window(),
            )?;
        } else {
            no_init_textui_putchar_window(
                character,
                fr_color,
                bk_color,
                textui_is_enable_put_to_window(),
            )?;
        }
    }

    return Ok(());
}

/// 初始化text ui框架
#[inline(never)]
pub fn textui_init() -> Result<i32, SystemError> {
    #[cfg(target_arch = "x86_64")]
    textui_framwork_init();

    return Ok(0);
}
