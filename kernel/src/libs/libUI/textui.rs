use crate::{
    driver::uart::uart::{c_uart_send, c_uart_send_str, UartPort},
    include::bindings::bindings::{font_ascii, video_frame_buffer_info},
    libs::{rwlock::RwLock, spinlock::SpinLock},
    syscall::SystemError,
};
use alloc::{collections::LinkedList, string::ToString};
use alloc::{sync::Arc, vec::Vec};
use core::{
    fmt::Debug,
    intrinsics::unlikely,
    ops::{Add, AddAssign, Deref, DerefMut, Sub},
    ptr::copy_nonoverlapping,
    sync::atomic::{AtomicU32, Ordering},
};

use thingbuf::mpsc::{
    self,
    errors::{TryRecvError, TrySendError},
};

use super::screen_manager::{
    scm_register, ScmBufferInfo, ScmFramworkType, ScmUiFramework, ScmUiFrameworkMetadata,
};
use lazy_static::lazy_static;

// 暂时初始化16080个初始字符对象以及67个虚拟行对象
// const INITIAL_CHARS_NUM: usize = 200;
// const INITIAL_VLINES_NUM: usize = 67;
// const CHARS_PER_VLINE: usize = 200;
lazy_static! {
    pub static ref WINDOW_LIST: SpinLock<LinkedList<Arc<TextuiWindow>>> =
        SpinLock::new(LinkedList::new());
}
//window标志位
bitflags! {
    pub struct WindowFlag: u8 {
        // 采用彩色字符
        const TEXTUI_IS_CHROMATIC = 1 << 0;
    }
}

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
pub const MPSC_BUF_SIZE: usize = 512;
impl WindowMpsc {
    fn new() -> Self {
        // let window = &TEXTUI_PRIVATE_INFO.lock().current_window;
        let (window_s, window_r) = mpsc::channel::<TextuiWindow>(MPSC_BUF_SIZE);
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
#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Hash, Default)]
pub struct LineId(i32);
impl LineId {
    fn new(num: i32) -> Self {
        LineId(num)
    }
    fn check(&self, max: i32) -> bool {
        self.0 < max && self.0 >= 0
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
    fn new(num: i32) -> Self {
        LineIndex(num)
    }
    fn check(&self, chars_per_line: i32) -> bool {
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

/**
 * @brief 彩色字符对象
 *
 */
#[derive(Clone, Debug, Copy)]
pub struct TextuiCharChromatic {
    c: u8,

    // 前景色
    frcolor: FontColor, // rgb

    // 背景色
    bkcolor: FontColor, // rgb
}

// #[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Hash)]
// pub struct BufAddr(usize);
// impl BufAddr {
//     pub fn new(vaddr: usize) -> Self {
//         return Self(vaddr);
//     }
//     // fn get_buf_addr_start() -> BufAddr {
//     //     Self(*BUF_VADDR.read() as *mut usize)
//     // }
//     pub fn get_buf_addr_by_offset(count: isize) -> BufAddr {
//         let mut addr = BufAddr::new(BUF_VADDR.load(Ordering::SeqCst));
//         if count < 0 {
//             return addr - count.abs() as usize;
//         }else{
//             return addr + count as usize;
//         }
//     }
// }
// impl Into<usize> for BufAddr {
//     fn into(self) -> usize {
//         self.0
//     }
// }
// impl core::ops::Add<usize> for BufAddr {
//     type Output = Self;
//     fn add(self, rhs: usize) -> Self::Output {
//         BufAddr::new(self.0 + rhs)
//     }
// }
// impl core::ops::Sub<usize> for BufAddr {
//     type Output = Self;
//     fn sub(self, rhs: usize) -> Self::Output {
//         Self::new(self.0 - rhs)
//     }
// }
pub static TEXTUI_BUF_WIDTH: RwLock<u32> = RwLock::new(0);
pub static TEXTUI_BUF_VADDR: RwLock<usize> = RwLock::new(0);
pub static TEXTUI_BUF_SIZE: RwLock<usize> = RwLock::new(0);

pub fn set_textui_buf_vaddr(vaddr: usize) {
    *TEXTUI_BUF_VADDR.write() = vaddr;
}
pub fn set_textui_buf_size(size: usize) {
    *TEXTUI_BUF_SIZE.write() = size;
}
pub fn set_textui_buf_width(width: u32) {
    *TEXTUI_BUF_WIDTH.write() = width;
}
#[derive(Debug)]
pub struct TextuiBuf<'a>(&'a mut [u32]);

impl TextuiBuf<'_> {
    pub fn new(buf: &mut [u32]) -> TextuiBuf {
        TextuiBuf(buf)
    }
    pub fn get_buf_from_vaddr(vaddr: usize, len: usize) -> TextuiBuf<'static> {
        let new_buf: &mut [u32] =
            unsafe { core::slice::from_raw_parts_mut(vaddr as *mut u32, len) };
        let buf: TextuiBuf<'_> = TextuiBuf::new(new_buf);
        return buf;
    }

    // pub fn get_buf_from_video_frame_buffer_info() -> TextuiBuf<'static> {
    //     let len =
    //         unsafe { video_frame_buffer_info.width * video_frame_buffer_info.height } as usize;
    //     TextuiBuf::get_buf_from_vaddr(unsafe { video_frame_buffer_info.vaddr }as usize, len)
    // }
    pub fn put_color_in_pixel(&mut self, color: u32, index: usize) {
        let buf: &mut [u32] = self.0;
        buf[index] = color;
    }
    pub fn get_index_of_next_line(now_index: usize) -> usize {
        *(TEXTUI_BUF_WIDTH.read()) as usize + now_index
    }
    pub fn get_index_by_x_y(x: usize, y: usize) -> usize {
        *(TEXTUI_BUF_WIDTH.read()) as usize * y + x
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
// impl Into<&mut [u32]> for TextuiBuf<'_> {
//     fn into(&self) -> &mut [u32] {
//         self.0
//     }
// }
// impl Clone for Buf<'_> {
//     fn clone(&self) -> Self {
//         let mut buf = Vec::new();
//         for i in self.0.iter() {
//             &buf.push(*i);
//         }
//         let b = buf.as_mut_slice();
//         // 转移对 cloned_data 的所有权到新创建的 Buf 实例中
//         let cloned = Buf::new(b);
//         core::mem::forget(b);
//         return cloned;
//     }
// }
// impl TextuiBuf<'_>{
//     pub fn new(buf:Buf)->Self{
//         TextuiBuf(buf)
//     }
//     pub fn put_color_in_pixel(&self,color:u32,index:usize){
//         let buf:&mut [u32]=(*self).into();
//         buf[index]=color;
//     }
//     pub fn get_index_of_next_line(now_index:usize)->usize{
//         *(BUF_WIDTH.read()) as usize+now_index
//     }
//     pub fn get_index_by_x_y(x:usize,y:usize)->usize{
//         *(BUF_WIDTH.read()) as usize*y+x
//     }
//     pub fn get_start_index_by_lineid_lineindex(lineid:LineId,lineindex:LineIndex)->usize{
//         //   x 左上角列像素点位置
//         //   y 左上角行像素点位置
//         let index_x: u32 = lineindex.into();
//         let x: u32 = index_x * TEXTUI_CHAR_WIDTH;
//         let id_y: u32 = lineid.into();
//         let y: u32 = id_y * TEXTUI_CHAR_HEIGHT;
//         TextuiBuf::get_index_by_x_y(x as usize, y as usize)
//     }
// }
// impl Into<&mut [u32]> for TextuiBuf<'_> {
//     fn into(self) -> &'static mut [u32] {
//         self.0.into()
//     }
// }
#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub struct Font([u8; 16]);
impl Font {
    pub fn get_font(index: usize) -> Font {
        Self(unsafe { font_ascii[index] })
    }
    pub fn is_frcolor(&self, height: usize, width: usize) -> bool {
        let w = self.0[height];
        let testbit = 1 << (8 - width);
        w & testbit != 0
    }
}

impl TextuiCharChromatic {
    fn new() -> Self {
        TextuiCharChromatic {
            c: 0,
            frcolor: FontColor::BLACK,
            bkcolor: FontColor::BLACK,
        }
    }
    /**
     * @brief 将该字符对象输出到缓冲区
     *
     * @param line_id 要放入的真实行号
     * @param index 要放入的真实列号
     */
    pub fn textui_refresh_character(
        &self,
        lineid: LineId,
        lineindex: LineIndex,
    ) -> Result<i32, SystemError> {
        // 找到要渲染的字符的像素点数据
        let font: Font = Font::get_font(self.c as usize);

        let mut count = TextuiBuf::get_start_index_by_lineid_lineindex(lineid, lineindex);

        // let buf=TEXTUI_BUF.lock();
        let vaddr = *TEXTUI_BUF_VADDR.read();
        let len = *TEXTUI_BUF_SIZE.read();
        let mut buf = TextuiBuf::get_buf_from_vaddr(vaddr, len);
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
        let font = unsafe { font_ascii }[self.c as usize];

        //   x 左上角列像素点位置
        //   y 左上角行像素点位置
        let index_x: u32 = lineindex.into();
        let x: u32 = index_x * TEXTUI_CHAR_WIDTH;

        let id_y: u32 = lineid.into();
        let y: u32 = id_y * TEXTUI_CHAR_HEIGHT;
        // 找到输入缓冲区的起始地址位置
        let fb = unsafe { video_frame_buffer_info.vaddr };

        let mut testbit: u32; // 用来测试特定行的某列是背景还是字体本身

        // 在缓冲区画出一个字体，每个字体有TEXTUI_CHAR_HEIGHT行，TEXTUI_CHAR_WIDTH列个像素点
        for i in 0..TEXTUI_CHAR_HEIGHT {
            // 计算出帧缓冲区每一行打印的起始位置的地址（起始位置+（y+i）*缓冲区的宽度+x）

            let mut addr: *mut u32 = (fb as u32
                + unsafe { video_frame_buffer_info.width } * 4 * (y as u32 + i)
                + 4 * x as u32) as *mut u32;

            testbit = 1 << (TEXTUI_CHAR_WIDTH + 1);
            for _j in 0..TEXTUI_CHAR_WIDTH {
                //从左往右逐个测试相应位
                testbit >>= 1;
                if (font[i as usize] & testbit as u8) != 0 {
                    unsafe { *addr = self.frcolor.into() }; // 字，显示前景色
                } else {
                    unsafe { *addr = self.bkcolor.into() }; // 背景色
                }

                unsafe {
                    addr = (addr.offset(1)) as *mut u32;
                }
            }
        }
        // let font: Font = Font::get_font(self.c as usize);

        // let mut count = TextuiBuf::get_start_index_by_lineid_lineindex(lineid, lineindex);

        // // let buf=TEXTUI_BUF.lock();
        // // let vaddr = unsafe { video_frame_buffer_info.vaddr } as usize;
        // // let len = unsafe { video_frame_buffer_info.height*video_frame_buffer_info.width }as usize;
        // let mut buf = TextuiBuf::get_buf_from_video_frame_buffer_info();
        // for i in 0..TEXTUI_CHAR_HEIGHT {
        //     let start = count;
        //     for j in 0..TEXTUI_CHAR_WIDTH {
        //         if font.is_frcolor(i as usize, j as usize) {
        //             // 字，显示前景色
        //             buf.put_color_in_pixel(self.frcolor.into(), count);
        //         } else {
        //             // 背景色
        //             buf.put_color_in_pixel(self.bkcolor.into(), count);
        //         }
        //         count += 1;
        //     }
        //     count = TextuiBuf::get_index_of_next_line(start);
        // }
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
    index: LineIndex,                // 当前操作的位置
}
impl TextuiVlineChromatic {
    fn new() -> Self {
        TextuiVlineChromatic {
            chars: Vec::new(),
            index: LineIndex::new(0),
        }
    }
    /**
     * @brief 初始化虚拟行对象
     *
     * @param vline 虚拟行对象指针
     * @param chars_ptr 字符对象数组指针
     */
    fn textui_init_vline(&mut self, num: usize) {
        self.index = LineIndex(0);
        let value = TextuiCharChromatic::new();
        for _i in 0..num {
            self.chars.push(value);
        }
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
pub static ACTUAL_LINE_NUM: RwLock<i32> = RwLock::new(0);
pub static CORRENT_WINDOW_ID: RwLock<WindowId> = RwLock::new(WindowId(0));
impl TextuiWindow {
    fn new() -> Self {
        TextuiWindow {
            id: WindowId(0),
            flags: WindowFlag::TEXTUI_IS_CHROMATIC,
            vline_sum: 0,
            vlines_used: 1,
            top_vline: LineId::new(0),
            vlines: Vec::new(),
            vline_operating: LineId::new(0),
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

        flags: WindowFlag,
        vlines_num: i32,
        vlines_ptr: Vec<TextuiVline>,
        cperline: i32,
    ) -> Result<i32, SystemError> {
        window.id = WindowId::new();
        window.flags = flags;
        window.vline_sum = vlines_num;
        window.vlines_used = 1;
        window.top_vline = LineId::new(0);
        window.vlines = vlines_ptr;
        window.vline_operating = LineId::new(0);
        window.chars_per_line = cperline;

        WINDOW_LIST.lock().push_back(Arc::new(window.clone()));

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
        &mut self,
        vline_id: LineId,
        start: LineIndex,
        count: i32,
    ) -> Result<i32, SystemError> {
        let corrent_window_id = *CORRENT_WINDOW_ID.read();

        let actual_line_sum = *ACTUAL_LINE_NUM.read();
        // 要刷新的窗口正在使用，则退出刷新
        if self.id != corrent_window_id {
            return Ok(0);
        }
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
        if self.flags.contains(WindowFlag::TEXTUI_IS_CHROMATIC) {
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

        return Ok(0);
    }

    /**
     * @brief 重新渲染某个窗口的某个虚拟行
     *
     * @param window 窗口结构体
     * @param vline_id 虚拟行号
     * @return int 错误码
     */
    fn textui_refresh_vline(&mut self, vline_id: LineId) -> Result<i32, SystemError> {
        if self.flags.contains(WindowFlag::TEXTUI_IS_CHROMATIC) {
            return self.textui_refresh_characters(
                vline_id,
                LineIndex::new(0),
                self.chars_per_line,
            );
        } else {
            //todo支持纯文本字符
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

    /**
     * @brief 往某个窗口的缓冲区的某个虚拟行插入换行
     *
     * @param window 窗口结构体
     * @param vline_id 虚拟行号
     * @return int
     */
    fn textui_new_line(&mut self) -> Result<i32, SystemError> {
        // todo: 支持在两个虚拟行之间插入一个新行

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
                    v_char.c = 0;
                    v_char.frcolor = FontColor::BLACK;
                    v_char.bkcolor = FontColor::BLACK;
                }
            }
            vline.index = LineIndex::new(0);
        }
        // 当已经使用的虚拟行总数等于真实行总数时，说明窗口中已经显示的文本行数已经达到了窗口的最大容量。这时，如果继续在窗口中添加新的文本，就会导致文本溢出窗口而无法显示。因此，需要往下滚动屏幕来显示更多的文本。

        if self.vlines_used == *ACTUAL_LINE_NUM.read() {
            self.top_vline = self.top_vline + 1;

            if !self.top_vline.check(self.vline_sum) {
                self.top_vline = LineId::new(0);
            }

            // 刷新所有行
            self.textui_refresh_vlines(self.top_vline, *ACTUAL_LINE_NUM.read())?;
        } else {
            //换行说明上一行已经在缓冲区中，所以已经使用的虚拟行总数+1
            self.vlines_used += 1;
        }

        return Ok(0);
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
        frcolor: FontColor,
        bkcolor: FontColor,
    ) -> Result<i32, SystemError> {
        // 启用彩色字符
        if self.flags.contains(WindowFlag::TEXTUI_IS_CHROMATIC) {
            let mut s_index = LineIndex::new(0); //操作的列号
            if let TextuiVline::Chromatic(vline) =
                &mut (self.vlines[<LineId as Into<usize>>::into(self.vline_operating)])
            {
                let index = <LineIndex as Into<usize>>::into(vline.index);

                if let Some(v_char) = vline.chars.get_mut(index) {
                    v_char.c = character;
                    v_char.frcolor = frcolor;
                    v_char.bkcolor = bkcolor;
                }
                s_index = vline.index;
                vline.index = vline.index + 1;
            }

            self.textui_refresh_characters(self.vline_operating, s_index, 1)?;

            // 加入光标后，因为会识别光标，所以需超过该行最大字符数才能创建新行
            if !s_index.check(self.chars_per_line - 1) {
                self.textui_new_line()?;
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
        frcolor: FontColor,
        bkcolor: FontColor,
    ) -> Result<i32, SystemError> {
        //字符'\0'代表ASCII码表中的空字符,表示字符串的结尾
        if unlikely(character == b'\0') {
            return Ok(0);
        }
        // 暂不支持纯文本窗口
        if !self.flags.contains(WindowFlag::TEXTUI_IS_CHROMATIC) {
            return Ok(0);
        }

        //进行换行操作
        if character == b'\n' {
            // 换行时还需要输出\r
            c_uart_send(UartPort::COM1.to_u16(), b'\r');
            self.textui_new_line()?;

            return Ok(0);
        }
        // 输出制表符
        else if character == b'\t' {
            if let TextuiVline::Chromatic(vline) =
                &self.vlines[<LineId as Into<usize>>::into(self.vline_operating)]
            {
                //打印的空格数（注意将每行分成一个个表格，每个表格为8个字符）
                let mut space_to_print = 8 - <LineIndex as Into<usize>>::into(vline.index) % 8;
                while space_to_print > 0 {
                    self.ture_textui_putchar_window(b' ', frcolor, bkcolor)?;
                    space_to_print -= 1;
                }
            }
        }
        // 字符 '\x08' 代表 ASCII 码中的退格字符。它在输出中的作用是将光标向左移动一个位置，并在该位置上输出后续的字符，从而实现字符的删除或替换。
        else if character == b'\x08' {
            let mut tmp = LineIndex(0);
            if let TextuiVline::Chromatic(vline) =
                &mut self.vlines[<LineId as Into<usize>>::into(self.vline_operating)]
            {
                // if self.vline_operating == 1&&vline.index==9 {
                //     loop {}
                // }
                vline.index = vline.index - 1;
                tmp = vline.index;
            }
            if <LineIndex as Into<i32>>::into(tmp) >= 0 {
                if let TextuiVline::Chromatic(vline) =
                    &mut self.vlines[<LineId as Into<usize>>::into(self.vline_operating)]
                {
                    if let Some(v_char) = vline.chars.get_mut(<LineIndex as Into<usize>>::into(tmp))
                    {
                        v_char.c = b' ';

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
                            v_char.c = 0;
                            v_char.frcolor = FontColor::BLACK;
                            v_char.bkcolor = FontColor::BLACK;
                        }
                    }
                }
                // 上缩一行
                self.vline_operating = self.vline_operating - 1;
                if <LineId as Into<i32>>::into(self.vline_operating) < 0 {
                    self.vline_operating = LineId(self.vline_sum - 1);
                }

                // 考虑是否向上滚动（在top_vline上退格）
                if self.vlines_used > *ACTUAL_LINE_NUM.read() {
                    self.top_vline = self.top_vline - 1;
                    if <LineId as Into<i32>>::into(self.top_vline) < 0 {
                        self.top_vline = LineId(self.vline_sum - 1);
                    }
                }
                //因为上缩一行所以显示在屏幕中的虚拟行少一
                self.vlines_used -= 1;
                self.textui_refresh_vlines(self.top_vline, *ACTUAL_LINE_NUM.read())?;
            }
        } else {
            // 输出其他字符
            c_uart_send(UartPort::COM1.to_u16(), character);
            if let TextuiVline::Chromatic(vline) =
                &self.vlines[<LineId as Into<usize>>::into(self.vline_operating)]
            {
                if !vline.index.check(self.chars_per_line) {
                    self.textui_new_line()?;
                }

                return self.ture_textui_putchar_window(character, frcolor, bkcolor);
            }
        }

        return Ok(0);
    }
}
impl Default for TextuiWindow {
    fn default() -> Self {
        TextuiWindow {
            id: WindowId(0),
            flags: WindowFlag::TEXTUI_IS_CHROMATIC,
            vline_sum: 0,
            vlines_used: 1,
            top_vline: LineId::new(0),
            vlines: Vec::new(),
            vline_operating: LineId::new(0),
            chars_per_line: 0,
        }
    }
}
#[derive(Clone, Debug)]
pub struct TextuiPrivateInfo {
    pub actual_line: i32, // 真实行的数量（textui的帧缓冲区能容纳的内容的行数）
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
    metadata: ScmUiFrameworkMetadata,
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
    fn install(&self) -> Result<i32, SystemError> {
        c_uart_send_str(
            UartPort::COM1.to_u16(),
            "\ntextui_install_handler\n\0".as_ptr(),
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
            "\ntextui_enable_handler\n\0".as_ptr(),
        );
        return Ok(0);
    }
    // 禁用ui框架的回调函数
    fn disable(&self) -> Result<i32, SystemError> {
        return Ok(0);
    }
    // 改变ui框架的帧缓冲区的回调函数
    fn change(&self, buf: ScmBufferInfo) -> Result<i32, SystemError> {
        // let framework = TEXTUIFRAMEWORK.0.lock();
        let src = self.metadata.buf_info.get_vaddr() as *const u8;
        let dst = buf.get_vaddr() as *mut u8;
        let count = self.metadata.buf_info.get_size_about_u8() as usize;
        unsafe { copy_nonoverlapping(src, dst, count) };
        set_textui_buf_vaddr(buf.get_vaddr());
        return Ok(0);
    }
    /// @brief 获取ScmUiFramework的元数据
    ///
    /// @return 成功：Ok(ScmUiFramework的元数据)
    ///         失败：Err(错误码)
    fn metadata(&self) -> Result<ScmUiFrameworkMetadata, SystemError> {
        return Ok(self.metadata.clone());
    }
}
// impl<'a> Deref for TextUiFramework<'a> {
//     type Target = ScmUiFrameworkMetadata<'a>;

//     fn deref(&self) -> &Self::Target {
//         &self.metadata
//     }
// }

// impl DerefMut for TextUiFramework<'_> {
//     fn deref_mut(&mut self) -> &mut Self::Target {
//         &mut self.metadata
//     }
// }
impl Deref for TextUiFramework {
    type Target = ScmUiFrameworkMetadata;

    fn deref(&self) -> &Self::Target {
        &self.metadata
    }
}

impl DerefMut for TextUiFramework {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.metadata
    }
}
// 每个字符的宽度和高度（像素）
pub const TEXTUI_CHAR_WIDTH: u32 = 8;
pub const TEXTUI_CHAR_HEIGHT: u32 = 16;

pub static mut TEST_IS_INIT: bool = false;
pub static TRUE_LINE_NUM: RwLock<i32> = RwLock::new(0);
pub static CHAR_PER_LINE: RwLock<i32> = RwLock::new(0);
//textui 未初始化时直接向缓冲区写，不使用虚拟行
pub static NO_INIT_OPERATIONS_LINE: RwLock<i32> = RwLock::new(0);
pub static NO_INIT_OPERATIONS_INDEX: RwLock<i32> = RwLock::new(0);
pub static mut ENABLE_PUT_TO_WINDOW: bool = true; //因为只在未初始化textui之前而其他模块使用的内存将要到达48M时到在初始化textui时才为false,所以只会修改两次，应该不需加锁

pub fn no_init_textui_putchar_window(
    character: u8,
    frcolor: FontColor,
    bkcolor: FontColor,
    is_put_to_window: bool,
) -> Result<i32, SystemError> {
    if *NO_INIT_OPERATIONS_LINE.read() > *TRUE_LINE_NUM.read() {
        *NO_INIT_OPERATIONS_LINE.write() = 0;
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
        if is_put_to_window == true {
            *NO_INIT_OPERATIONS_LINE.write() += 1;
            *NO_INIT_OPERATIONS_INDEX.write() = 0;
        }
        return Ok(0);
    }
    // 输出制表符
    else if character == b'\t' {
        if is_put_to_window == true {
            let char = TextuiCharChromatic {
                c: b' ',
                frcolor,
                bkcolor,
            };

            //打印的空格数（注意将每行分成一个个表格，每个表格为8个字符）
            let mut space_to_print = 8 - *NO_INIT_OPERATIONS_INDEX.read() % 8;
            while space_to_print > 0 {
                char.no_init_textui_render_chromatic(
                    LineId::new(*NO_INIT_OPERATIONS_LINE.read()),
                    LineIndex::new(*NO_INIT_OPERATIONS_INDEX.read()),
                );
                *NO_INIT_OPERATIONS_INDEX.write() += 1;
                space_to_print -= 1;
            }
            return Ok(0);
        }
    }
    // 字符 '\x08' 代表 ASCII 码中的退格字符。它在输出中的作用是将光标向左移动一个位置，并在该位置上输出后续的字符，从而实现字符的删除或替换。
    else if character == b'\x08' {
        if is_put_to_window == true {
            *NO_INIT_OPERATIONS_INDEX.write() -= 1;
            let op_char = *NO_INIT_OPERATIONS_INDEX.read();
            if op_char >= 0 {
                let char = TextuiCharChromatic {
                    c: b' ',
                    frcolor,
                    bkcolor,
                };
                char.no_init_textui_render_chromatic(
                    LineId::new(*NO_INIT_OPERATIONS_LINE.read()),
                    LineIndex::new(*NO_INIT_OPERATIONS_INDEX.read()),
                );

                *NO_INIT_OPERATIONS_INDEX.write() += 1;
            }
            // 需要向上缩一行
            if op_char < 0 {
                // 上缩一行
                *NO_INIT_OPERATIONS_LINE.write() -= 1;
                *NO_INIT_OPERATIONS_INDEX.write() = 0;
                if *NO_INIT_OPERATIONS_LINE.read() < 0 {
                    *NO_INIT_OPERATIONS_INDEX.write() = 0
                }
            }
        }
    } else {
        if is_put_to_window == true {
            // 输出其他字符
            let char = TextuiCharChromatic {
                c: character,
                frcolor,
                bkcolor,
            };

            if *NO_INIT_OPERATIONS_INDEX.read() == *CHAR_PER_LINE.read() {
                *NO_INIT_OPERATIONS_INDEX.write() = 0;
                *NO_INIT_OPERATIONS_LINE.write() += 1;
            }
            char.no_init_textui_render_chromatic(
                LineId::new(*NO_INIT_OPERATIONS_LINE.read()),
                LineIndex::new(*NO_INIT_OPERATIONS_INDEX.read()),
            );

            *NO_INIT_OPERATIONS_INDEX.write() += 1;
        }
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
    let r = true_textui_putchar(
        character,
        FontColor::from(fr_color),
        FontColor::from(bk_color),
    )
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
fn true_textui_putchar(
    character: u8,
    fr_color: FontColor,
    bk_color: FontColor,
) -> Result<i32, SystemError> {
    if unsafe { TEST_IS_INIT } {
        let val = WINDOW_MPSC.window_r.try_recv_ref();

        let mut window: TextuiWindow;
        if let Err(err) = val {
            match err {
                TryRecvError::Empty => {
                    window = CURRENT_WINDOW.clone();
                }
                _ => {
                    c_uart_send_str(
                        UartPort::COM1.to_u16(),
                        "true_textui_putchar fail\n\0".as_ptr(),
                    );
                    todo!();
                }
            }
        } else {
            let r = val.unwrap();

            window = r.clone(); //当mpsc_buf_size太大时会卡在这里，不知为何
        }
        window.textui_putchar_window(character, fr_color, bk_color)?;

        let mut window_s = WINDOW_MPSC.window_s.try_send_ref();

        loop {
            if let Err(err) = window_s {
                match err {
                    TrySendError::Full(_) => {
                        window_s = WINDOW_MPSC.window_s.try_send_ref();
                    }
                    _ => todo!(),
                }
            } else {
                break;
            }
        }
        *window_s.unwrap() = window;
    } else {
        //未初始化暴力输出

        return no_init_textui_putchar_window(character, fr_color, bk_color, unsafe {
            ENABLE_PUT_TO_WINDOW
        });
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
//允许往窗口打印信息
#[no_mangle]
pub extern "C" fn scm_enable_put_to_window() {
    unsafe { ENABLE_PUT_TO_WINDOW = true };
}
//禁止往窗口打印信息
#[no_mangle]
pub extern "C" fn scm_disable_put_to_window() {
    // unsafe{kfree(*TEXTUI_BUF_VADDR.read() as *mut c_void)};

    unsafe { ENABLE_PUT_TO_WINDOW = false };
}
// 将窗口的输出缓冲区重置
// fb:缓冲区起始地址
// num:要重置的像素点数量
pub fn renew_buf(fb: usize, num: u32) {
    let mut addr: *mut u32 = fb as *mut u32;
    for _i in 0..num {
        unsafe { *addr = 0 };
        unsafe {
            addr = (addr.offset(1)) as *mut u32;
        }
    }
}
// pub fn textui_enable_double_buffer() -> Result<i32, SystemError> {
//     c_uart_send_str(
//         UartPort::COM1.to_u16(),
//         "\ninit textui double buffer\n\0".as_ptr(),
//     );
//     // 创建双缓冲区
//     let buf_into = ScmBufferInfo::new(ScmBfFlag::SCM_BF_DB | ScmBfFlag::SCM_BF_PIXEL)?;
//     let mut framework = TEXTUIFRAMEWORK.0.lock();
//     if !framework.change(buf_into.clone()).is_err() {
//         framework.buf_info = buf_into;
//     }
//     return Ok(0);
// }
pub fn textui_change_buf(buf_info: ScmBufferInfo) -> Result<i32, SystemError> {
    let mut framework = TEXTUIFRAMEWORK.0.lock();
    if !framework.change(buf_info.clone()).is_err() {
        framework.buf_info = buf_info;
    }
    // println!("vaddr:{:#018x}",*TEXTUI_BUF_VADDR.read());
    return Ok(0);
}
fn textui_init() -> Result<i32, SystemError> {
    let name: &str = "textui";

    let mut framework = TEXTUIFRAMEWORK.0.lock();

    framework.metadata.name = name.to_string();

    framework.metadata.f_type = ScmFramworkType::Text;

    let private_info = &mut TEXTUI_PRIVATE_INFO.lock();

    private_info.actual_line =
        (framework.metadata.buf_info.get_height_about_u32() / TEXTUI_CHAR_HEIGHT) as i32;

    // 注册框架到屏幕管理器
    let textui = TextUiFramework {
        metadata: framework.metadata.clone(),
    };
    scm_register(Arc::new(textui))?;

    // 初始化虚拟行
    let vlines_num = (framework.metadata.buf_info.get_height_about_u32() / TEXTUI_CHAR_HEIGHT) as usize;

    let chars_num = (framework.metadata.buf_info.get_width_about_u32() / TEXTUI_CHAR_WIDTH) as usize;

    let mut initial_vlines = Vec::new();

    for _i in 0..vlines_num {
        let mut vline = TextuiVlineChromatic::new();

        vline.textui_init_vline(chars_num);

        initial_vlines.push(TextuiVline::Chromatic(vline));
    }

    // 初始化窗口
    let mut initial_window = TextuiWindow::new();

    TextuiWindow::init_window(
        &mut initial_window,
        WindowFlag::TEXTUI_IS_CHROMATIC,
        vlines_num as i32,
        initial_vlines,
        chars_num as i32,
    )?;

    framework.metadata.window_max_id += 1;

    // let num = framework.metadata.buf_info.get_width() * framework.metadata.buf_info.get_height();
    let num = framework.metadata.buf_info.get_size_about_u32();

    private_info.current_window = initial_window.clone();

    private_info.default_window = initial_window;

    *CORRENT_WINDOW_ID.write() = private_info.current_window.id;

    *ACTUAL_LINE_NUM.write() = private_info.actual_line as i32;

    drop(private_info);

    unsafe { TEST_IS_INIT = true };

    // *TEXTUI_BUF_WIDTH.write() = framework.metadata.buf_info.get_width();
    set_textui_buf_width(framework.metadata.buf_info.get_width_about_u32());

    // *TEXTUI_BUF_VADDR.write() = framework.metadata.buf_info.get_vaddr();
    set_textui_buf_vaddr(framework.metadata.buf_info.get_vaddr());

    // *TEXTUI_BUF_SIZE.write() = num as usize;
    set_textui_buf_size(num as usize);

    renew_buf(framework.metadata.buf_info.get_vaddr(), num);

    drop(framework);

    //     let c = TextuiCharChromatic {
    //         c: b'3',
    //         frcolor: FontColor::WHITE,
    //         bkcolor: FontColor::BLACK,
    //     };
    // for i in 0..150{
    //     for j in 0..100{
    //     c.textui_refresh_character(LineId(i), LineIndex(j));
    //     }
    // }
    // loop {}

    c_uart_send_str(
        UartPort::COM1.to_u16(),
        "\ntext ui initialized\n\0".as_ptr(),
    );
    return Ok(0);
}
