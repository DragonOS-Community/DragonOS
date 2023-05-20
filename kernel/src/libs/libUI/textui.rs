use crate::{
    driver::uart::uart::{c_uart_send, c_uart_send_str, UartPort},
    include::bindings::bindings::{scm_buffer_info_t, video_set_refresh_target},
    libs::{libUI::screen_manager::SCM_DOUBLE_BUFFER_ENABLED, spinlock::SpinLock},
    syscall::SystemError,
};
use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use alloc::{collections::LinkedList, string::ToString};
use core::{fmt::Debug, intrinsics::unlikely, ptr::copy_nonoverlapping, sync::atomic::Ordering};

use super::{
    screen_manager::{
        scm_register, ScmBufferInfo, ScmFramworkType, ScmUiFramework, ScmUiFrameworkMetadata,
        CURRENT_FRAMEWORK_METADATA,
    },
    textui_render::textui_render_chromatic,
};
use lazy_static::lazy_static;

// 暂时初始化16080个初始字符对象以及67个虚拟行对象
// const INITIAL_CHARS_NUM: usize = 200;
const INITIAL_VLINES_NUM: usize = 10;
const  CHARS_PER_VLINE: usize = 200;
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

lazy_static! {
    pub static ref TEXTUIFRAMEWORK: LockedTextUiFramework = LockedTextUiFramework::new();
}
lazy_static! {
    pub static ref TEXTUI_PRIVATE_INFO: Arc<TextuiPrivateInfo> = Arc::new(TextuiPrivateInfo::new());
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
#[derive(Clone, Debug,Default)]
pub struct TextuiVlineNormal {
    _chars: Vec<TextuiCharNormal>, // 字符对象数组
    _index: i16,                   // 当前操作的位置
}
/**
 * @brief 彩色显示的虚拟行结构体
 *
 */
#[derive(Clone, Debug,Default)]
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
        c_uart_send_str(
            UartPort::COM1.to_u16(),
            "\naaaa scm register 2\n\0".as_ptr(),
        );
        for _i in 0..num {
            self.chars.push(value);
            c_uart_send_str(
                UartPort::COM1.to_u16(),
                "\n11111 scm register 2\n\0".as_ptr(),
            );
        }
        c_uart_send_str(
            UartPort::COM1.to_u16(),
            "\neeee scm register 2\n\0".as_ptr(),
        );
    
    }
    fn clone_s(&self)->TextuiVlineChromatic{
        TextuiVlineChromatic {
            chars: self.chars.clone(),
            index: self.index,
    }}
}


#[derive(Clone, Debug,)]
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
impl TextuiWindow {
    fn new() -> Self {
        let textui_window = TextuiWindow {
            // list:Linkedlist::new(),
            id: 0,
            flags: WindowFlag::TEXTUI_IS_CHROMATIC,
            vline_sum: 0,
            vlines_used: 1,
            top_vline: 0,
            vlines: Vec::new(),
            vline_operating: 0,
            chars_per_line: 0,
        };

        textui_window
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
        vlines_num: i16,
        vlines_ptr: Vec<TextuiVline>,
        cperline: i16,
    ) -> Result<i32, SystemError> {
        let mut framework = TEXTUIFRAMEWORK.0.lock();
        window.id = framework.metadata.window_max_id;
        framework.metadata.window_max_id += 1;
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
        if self.id != TEXTUI_PRIVATE_INFO.current_window.id {
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
            actual_line_id += TEXTUI_PRIVATE_INFO.actual_line;
        }
        // 判断真实行id是否合理
        if unlikely(actual_line_id < 0 || actual_line_id >= TEXTUI_PRIVATE_INFO.actual_line) {
            return Ok(0);
        }

        // 将此窗口的某个虚拟行的连续n个字符对象往缓存区写入
        if self.flags.contains(WindowFlag::TEXTUI_IS_CHROMATIC) {
            let vline = &self.vlines[vline_id as usize];
            let mut i = 0;
            while i < count {
                if let TextuiVline::Chromatic(vline) = vline {
                    textui_render_chromatic(
                        actual_line_id as u16,
                        (start + i) as u16,
                        &vline.chars[(start + i) as usize],
                    );
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
    fn textui_new_line(&mut self, _vline_id: u16) -> i32 {
        // todo: 支持在两个虚拟行之间插入一个新行

        self.vline_operating += 1;
        //如果已经到了最大行数，则重新从0开始
        if self.vline_operating == self.vline_sum {
            self.vline_operating = 0;
        }

        if let TextuiVline::Chromatic(vline) = &mut (self.vlines[self.vline_operating as usize]) {
            for _i in 0..self.chars_per_line {
                vline.chars.push(TextuiCharChromatic::new());
            }
            vline.index = 0;
        }
        // 当已经使用的虚拟行总数等于真实行总数时，说明窗口中已经显示的文本行数已经达到了窗口的最大容量。这时，如果继续在窗口中添加新的文本，就会导致文本溢出窗口而无法显示。因此，需要往下滚动屏幕来显示更多的文本。

        if self.vlines_used == TEXTUI_PRIVATE_INFO.actual_line {
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
                vline.chars[vline.index as usize].c = character;
                vline.chars[vline.index as usize].frcolor = frcolor & 0xffffff;
                vline.chars[vline.index as usize].bkcolor = bkcolor & 0xffffff;
                vline.index += 1;
                s_vline = vline.index - 1;
            }
            let _ = self.textui_refresh_characters(self.vline_operating, s_vline, 1);
            // 加入光标后，因为会识别光标，所以需超过该行最大字符数才能创建新行
            if s_vline + 1 > self.chars_per_line {
                self.textui_new_line(self.vline_operating as u16);
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
            self.textui_new_line(self.vline_operating as u16);

            return Ok(0);
        }
        // 输出制表符
        else if character == b'\t' {
            if let TextuiVline::Chromatic(vline) = &self.vlines[self.vline_operating as usize] {
                //打印的空格数（注意将每行分成一个个表格，每个表格为8个字符）
                let mut space_to_print = 8 - vline.index % 8;
                while space_to_print > 0 {
                    let _ =
                        self.ture_textui_putchar_window(b' ', frcolor as usize, bkcolor as usize);
                    space_to_print -= 1;
                }
            }
        }
        // 字符 '\x08' 代表 ASCII 码中的退格字符。它在输出中的作用是将光标向左移动一个位置，并在该位置上输出后续的字符，从而实现字符的删除或替换。
        else if character == b'\x08' {
            let mut tmp = 0;
            if let TextuiVline::Chromatic(vline) = &mut self.vlines[self.vline_operating as usize] {
                vline.index -= 1;
                tmp = vline.index;
            }
            if tmp >= 0 {
                if let TextuiVline::Chromatic(vline) =
                    &mut self.vlines[self.vline_operating as usize]
                {
                    vline.chars[tmp as usize].c = b' ';
                    vline.chars[tmp as usize].bkcolor = bkcolor as usize & 0xffffff;
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
                    for _i in 0..self.chars_per_line {
                        vline.chars.push(TextuiCharChromatic::new());
                    }
                }
                // 上缩一行
                self.vline_operating -= 1;
                if self.vline_operating < 0 {
                    self.vline_operating = self.vline_sum - 1;
                }

                // 考虑是否向上滚动（在top_vline上退格）

                //?
                if self.vlines_used > TEXTUI_PRIVATE_INFO.actual_line {
                    self.top_vline -= 1;
                    if self.top_vline < 0 {
                        self.top_vline = self.vline_sum - 1;
                    }
                }
                //因为上缩一行所以显示在屏幕中的虚拟行少一
                self.vlines_used -= 1;
                self.textui_refresh_vlines(
                    self.top_vline as u16,
                    TEXTUI_PRIVATE_INFO.actual_line as u16,
                );
            }
        } else {
            // 输出其他字符
            if let TextuiVline::Chromatic(vline) = &self.vlines[self.vline_operating as usize] {
                if vline.index == self.chars_per_line {
                    self.textui_new_line(self.vline_operating as u16);
                }
                return self.textui_putchar_window(character, frcolor, bkcolor);
            }
        }

        return Ok(0);
    }
}

// struct LockedtextuiWindow(SpinLock<TextuiWindow>);
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
            // self_ref: Weak::default(),
            metadata: ScmUiFrameworkMetadata::new(ScmFramworkType::Text),
        };
        let result = Self(SpinLock::new(inner));
        // let mut guard = result.0.lock();
        // guard.self_ref = Arc::downgrade(&result);
        // drop(guard);

        return result;
    }

    pub fn reinit(&self) -> i32 {
        c_uart_send_str(UartPort::COM1.to_u16(), "start_textui\n\0".as_ptr());
        let name: &str = "textui";
        // let a = &self.0;
        c_uart_send_str(UartPort::COM1.to_u16(), "textui 5\n\0".as_ptr());
        let framework = &mut self.0.lock();
        c_uart_send_str(UartPort::COM1.to_u16(), "textui 5\n\0".as_ptr());

        framework.metadata.name = name.to_string();
        framework.metadata.f_type = ScmFramworkType::Text;
        // 注册框架到屏幕管理器
        let textui = TextUiFramework {
            metadata: framework.metadata.clone(),
        };
        let retval = scm_register(Arc::new(textui));
        c_uart_send_str(UartPort::COM1.to_u16(), "\ntext ui 1\n\0".as_ptr());
        if retval != 0 {
            c_uart_send_str(UartPort::COM1.to_u16(), "text ui init failed\n\0".as_ptr());
        }

        let r_chars_per_vline = framework.metadata.buf.width / TEXTUI_CHAR_WIDTH;
        let total_vlines = framework.metadata.buf.height / TEXTUI_CHAR_HEIGHT;

        let mut initial_vlines: Vec<TextuiVline> =
            vec![TextuiVline::Chromatic(TextuiVlineChromatic::new()); INITIAL_VLINES_NUM];

        c_uart_send_str(
            UartPort::COM1.to_u16(),
            "\neeee scm register 2\n\0".as_ptr(),
        );

        // 初始化虚拟行
        for i in 0..total_vlines {
            if let TextuiVline::Chromatic(vline) = &mut initial_vlines[i as usize] {
                vline.textui_init_vline(r_chars_per_vline as usize)
            }
        }
        c_uart_send_str(UartPort::COM1.to_u16(), "\ntext ui 2\n\0".as_ptr());
        // 初始化窗口
        let mut initial_window = TextuiWindow::new();
        let _ = TextuiWindow::init_window(
            &mut initial_window,
            WindowFlag::TEXTUI_IS_CHROMATIC,
            total_vlines as i16,
            initial_vlines.to_vec(),
            r_chars_per_vline as i16,
        );

        c_uart_send_str(UartPort::COM1.to_u16(), "text ui initialized\n\0".as_ptr());
        return 0;
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
        let src = TEXTUIFRAMEWORK.0.lock().metadata.buf.vaddr as *const u8;
        let dst = buf.vaddr as *mut u8;
        let count = TEXTUIFRAMEWORK.0.lock().metadata.buf.size as usize;
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
    // fn clone_box(&self) -> Result<Box<dyn ScmUiFramework>,SystemError>{
    //     let result:TextUiFramework=TextUiFramework::new();
    //     result.metadata=self.metadata();
    //     let ans=Box::new(result);
    //     return Ok(ans);
    // }
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
    let window = &TEXTUI_PRIVATE_INFO.default_window;
    return window
        .clone()
        .textui_putchar_window(character, fr_color, bk_color)
        .unwrap();
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
        c_uart_send_str(UartPort::COM1.to_u16(), "textu init failed.\n\0".as_ptr());
    }
    return r;
}

fn textui_init() -> Result<i32, SystemError> {
    let f_name = "textui";
    // let a = &TEXTUIFRAMEWORK.0;

    let framework = &mut TEXTUIFRAMEWORK.0.lock();
    // let a=&framework.metadata.name;
    framework.metadata.f_type = ScmFramworkType::Text;

    framework.metadata.name=f_name.to_string();

    // 调用ui框架的回调函数以安装ui框架，并将其激活
    framework.install(framework.metadata()?.buf)?;
    framework.enable()?;

    if CURRENT_FRAMEWORK_METADATA.lock().is_null {
        if framework.metadata().unwrap().buf.vaddr == 0 {
            return Err(SystemError::EINVAL);
        }

        let mut current_framework = CURRENT_FRAMEWORK_METADATA.lock();

        // spin_lock(&scm_screen_own_lock);

        if unsafe { SCM_DOUBLE_BUFFER_ENABLED.load(Ordering::SeqCst) } == true {
            let buf: *mut scm_buffer_info_t =
                &mut framework.metadata().unwrap().buf.to_c() as *mut scm_buffer_info_t;
            let retval = unsafe { video_set_refresh_target(buf) };

            if retval == 0 {
                current_framework.id = framework.metadata()?.id;

                current_framework.buf = framework.metadata()?.buf;

                current_framework.f_type = framework.metadata()?.f_type;
                current_framework.name = framework.metadata()?.name;
                // current_framework.private_info = ui.metadata().unwrap().private_info;
                current_framework.is_null = framework.metadata()?.is_null;
                current_framework.window_max_id = framework.metadata()?.window_max_id;
            }
        } else {
            current_framework.id = framework.metadata()?.id;

            current_framework.buf = framework.metadata()?.buf;

            current_framework.f_type = framework.metadata()?.f_type;
            current_framework.name = framework.metadata()?.name;
            // current_framework.private_info = ui.metadata()?.private_info;
            current_framework.is_null = framework.metadata()?.is_null;
            current_framework.window_max_id = framework.metadata()?.window_max_id;
        }
    }


    // c_uart_send_str(UartPort::COM1.to_u16(), chars_per_vline.to_string().as_ptr());
    // let s: String = format!("{:?}\n\0", chars_per_vline as i32);
    // c_uart_send_str(UartPort::COM1.to_u16(), "\ntext ui 2\n\0".as_ptr());
    // c_uart_send_str(UartPort::COM1.to_u16(), s.as_ptr());
    // let a: String = format!("{:?}\n\0", total_vlines);
    // c_uart_send_str(UartPort::COM1.to_u16(), a.as_ptr());
    // 初始化虚拟行
    let chars=[TextuiCharChromatic::new(); CHARS_PER_VLINE ];
    let vline: TextuiVline=TextuiVline::Chromatic(TextuiVlineChromatic { chars: chars.to_vec(), index: 0 });
    c_uart_send_str(UartPort::COM1.to_u16(), "\naaa text ui 2\n\0".as_ptr());

    let initial_vlines: [TextuiVline;  INITIAL_VLINES_NUM] =[vline.clone(),vline.clone(),vline.clone(),vline.clone(),vline.clone(),vline.clone(),vline.clone(),vline.clone(),vline.clone(),vline.clone()];


    c_uart_send_str(UartPort::COM1.to_u16(), "\ntext ui 2\n\0".as_ptr());
    // 初始化窗口
    let mut initial_window = TextuiWindow::new();
    let _ = TextuiWindow::init_window(
        &mut initial_window,
        WindowFlag::TEXTUI_IS_CHROMATIC,
        INITIAL_VLINES_NUM as i16,
        initial_vlines.to_vec(),
        CHARS_PER_VLINE as i16,
    );

    c_uart_send_str(UartPort::COM1.to_u16(), "text ui initialized\n\0".as_ptr());
    return Ok(0);

}
