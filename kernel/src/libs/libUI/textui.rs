use crate::{
    driver::uart::uart::{c_uart_send, c_uart_send_str, UartPort},
    libs::spinlock::SpinLock,
    syscall::SystemError,
};
use alloc::{collections::LinkedList, };
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{intrinsics::unlikely, ptr::copy_nonoverlapping};

use super::{
    screen_manager::{
        scm_register, ScmBufferInfo, ScmFramworkType, ScmUiFramework, ScmUiFrameworkMetadata,
        ScmUiPrivateInfo,
    },
    textui_render::textui_render_chromatic,
};
use lazy_static::lazy_static;

// 暂时初始化16080个初始字符对象以及67个虚拟行对象
const INITIAL_CHARS_NUM: usize = 16080;
const INITIAL_VLINES_NUM: usize = 1080 / 16;
lazy_static! {
    pub static ref INITIAL_CHARS: Vec<TextuiCharChromatic> =
        vec![TextuiCharChromatic::new(); INITIAL_CHARS_NUM];
}
lazy_static! {
    pub static ref INITIAL_VLINES: Vec<TextuiVline> =
        vec![TextuiVline::Chromatic(TextuiVlineChromatic::new()); INITIAL_VLINES_NUM];
}
lazy_static! {
pub static ref INITIAL_WINDOW: SpinLock<TextuiWindow> = SpinLock::new(TextuiWindow::new()); // 初始窗口
pub static ref WINDOW_LIST: SpinLock<LinkedList<TextuiWindow>> = SpinLock::new(LinkedList::new());
}
// 采用彩色字符
const TEXTUI_WF_CHROMATIC: u8 = 1;
// 每个字符的宽度和高度（像素）
const TEXTUI_CHAR_WIDTH: u32 = 8;
const TEXTUI_CHAR_HEIGHT: u32 = 16;

// 定义一个静态全局变量
lazy_static! {
    pub static ref TEXTUIFRAMEWORK: Arc<LockedTextUiFramework> =
        LockedTextUiFramework::new();
}

/**
 * @brief 黑白字符对象
 *
 */
#[derive(Clone, Debug)]
struct TextuiCharNormal {
    c: u8,
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
#[derive(Clone, Debug)]
pub struct TextuiVlineNormal {
    chars: Vec<TextuiCharNormal>, // 字符对象数组
    index: i16,                   // 当前操作的位置
}
/**
 * @brief 彩色显示的虚拟行结构体
 *
 */
#[derive(Clone, Debug)]
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
}

#[derive(Clone, Debug)]
pub enum TextuiVline {
    Chromatic(TextuiVlineChromatic),
    Normal(TextuiVlineNormal),
}

#[derive(Clone, Debug)]
pub struct TextuiWindow {
    // list:LinkedList<>,
    pub id: u32,
    pub vline_sum: i16,
    pub vlines_used: i16,
    pub top_vline: i16,
    pub vlines: Vec<TextuiVline>,
    pub vline_operating: i16,
    pub chars_per_line: i16,
    pub flags: u8,
}
impl TextuiWindow {
    fn new() -> Self {
        let textui_window = TextuiWindow {
            // list:Linkedlist::new(),
            id: 0,
            flags: 0,
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
        flags: u8,
        vlines_num: i16,
        vlines_ptr: Vec<TextuiVline>,
        cperline: i16,
    ) -> Result<i32, SystemError> {
        // spin_lock(&__window_id_lock);
        TEXTUIFRAMEWORK.0.lock().metadata.window_max_id += 1;
        window.id = TEXTUIFRAMEWORK.0.lock().metadata.window_max_id;
        // spin_unlock(&__window_id_lock);

        window.flags = flags;
        window.vline_sum = vlines_num;
        window.vlines_used = 1;
        window.top_vline = 0;
        window.vlines = vlines_ptr;
        window.vline_operating = 0;
        window.chars_per_line = cperline;

         WINDOW_LIST.lock().push_back(window.clone()) ;
        return Ok(0);
    }
}

// struct LockedtextuiWindow(SpinLock<TextuiWindow>);
#[derive(Clone, Debug)]
pub struct TextuiPrivateInfo {
    pub actual_line: i16,                  // 真实行的数量
    pub current_window: Box<TextuiWindow>, // 当前的主窗口
    pub default_window: Box<TextuiWindow>, // 默认print到的窗口
}
impl TextuiPrivateInfo {
    pub fn new() -> Self {
        TextuiPrivateInfo {
            actual_line: 0,
            current_window: Box::new(TextuiWindow::new()),
            default_window: Box::new(TextuiWindow::new()),
        }
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
    fn new() -> Arc<Self> {
        let inner = TextUiFramework {
            // self_ref: Weak::default(),
            metadata: ScmUiFrameworkMetadata::new(ScmFramworkType::Text),
        };
        let result = Arc::new(Self(SpinLock::new(inner)));
        // let mut guard = result.0.lock();
        // guard.self_ref = Arc::downgrade(&result);
        // drop(guard);
        return result;
    }
}
impl ScmUiFramework for TextUiFramework {
    // 安装ui框架的回调函数
    fn install(&self, _buf: ScmBufferInfo) -> Result<i32, SystemError> {
        c_uart_send_str(UartPort::COM1.to_u16(), "textui_install_handler".as_ptr());
        return Ok(0);
    }
    // 卸载ui框架的回调函数
    fn uninstall(&self) -> Result<i32, SystemError> {
        return Ok(0);
    }
    // 启用ui框架的回调函数
    fn enable(&self) -> Result<i32, SystemError> {
        c_uart_send_str(UartPort::COM1.to_u16(), "textui_enable_handler\n".as_ptr());
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
        // 若文件系统没有实现此方法，则返回“不支持”
        return Ok(self.metadata.clone());
    }
}

/**
 * @brief 刷新某个虚拟行的连续n个字符对象
 *
 * @param window 窗口结构体
 * @param vline_id 虚拟行号
 * @param start 起始字符号
 * @param count 要刷新的字符数量
 * @return int 错误码
 */
fn textui_refresh_characters(
    window: &mut TextuiWindow,
    vline_id: i16,
    start: i16,
    count: i16,
) -> Result<i32, SystemError> {
    match &TEXTUIFRAMEWORK.0.lock().metadata.private_info {
        ScmUiPrivateInfo::Textui(private_info) => {
            if window.id != private_info.current_window.id {
                return Ok(0);
            }
            // 判断虚拟行参数是否合法
            if unlikely(vline_id >= window.vline_sum && (start + count) > window.chars_per_line) {
                return Err(SystemError::EINVAL);
            }
            // 计算虚拟行对应的真实行
            let mut actual_line_id = vline_id - window.top_vline;
            if actual_line_id < 0 {
                actual_line_id += private_info.actual_line;
            }
            // 判断真实行id是否合理
            if unlikely(actual_line_id < 0 || actual_line_id >= private_info.actual_line) {
                return Ok(0);
            }
            // 若是彩色像素模式
            if window.flags == TEXTUI_WF_CHROMATIC {
                let vline = &window.vlines[vline_id as usize];
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
        ScmUiPrivateInfo::Gui => todo!(),
        ScmUiPrivateInfo::Unused => todo!(),
    }
}
/**
 * @brief 重新渲染整个虚拟行
 *
 * @param window 窗口结构体
 * @param vline_id 虚拟行号
 * @return int 错误码
 */
fn textui_refresh_vline(window: &mut TextuiWindow, vline_id: u16) -> Result<i32, SystemError> {
    if window.flags == TEXTUI_WF_CHROMATIC {
        return textui_refresh_characters(window, vline_id as i16, 0, window.chars_per_line);
    } else {
        return textui_refresh_characters(window, vline_id as i16, 0, window.chars_per_line);
    }
}

// 刷新所有行
fn textui_refresh_vlines(window: &mut TextuiWindow, start: u16, count: u16) -> i32 {
    // let mut bufff: [u8; 16] = [0; 16];
    for i in start..(window.vline_sum as u16).min(start + count) {
        textui_refresh_vline(window, i);
    }
    // let mut remaining_count = count.saturating_sub((window.vline_sum as u16).saturating_sub(start));
    let mut refresh_start = 0;
    let mut refresh_count = count;
    while refresh_count > 0 {
        textui_refresh_vline(window, refresh_start);
        refresh_start += 1;
        refresh_count -= 1;
    }
    return 0;
}

/**
 * @brief 插入换行
 *
 * @param window 窗口结构体
 * @param vline_id 虚拟行号
 * @return int
 */
fn textui_new_line(window: &mut TextuiWindow, _vline_id: u16) -> i32 {
    // todo: 支持在两个虚拟行之间插入一个新行

    window.vline_operating += 1;

    if window.vline_operating == window.vline_sum {
        window.vline_operating = 0;
    }

    if let TextuiVline::Chromatic(vline) = &mut (window.vlines[window.vline_operating as usize]) {
        for _i in 0..window.chars_per_line {
            vline.chars.push(TextuiCharChromatic::new());
        }
        vline.index = 0;
    }
    // 需要滚动屏幕
    if window.vlines_used == window.vline_sum {
        window.top_vline += 1;

        if window.top_vline >= window.vline_sum {
            window.top_vline = 0;
        }

        // 刷新所有行
        textui_refresh_vlines(window, window.top_vline as u16, window.vline_sum as u16);
    } else {
        window.vlines_used += 1;
    }
    return 0;
}

/**
 * @brief 真正向屏幕上输出字符的函数
 *
 * @param window
 * @param character
 * @return int
 */
fn ture_textui_putchar_window(
    window: &mut TextuiWindow,
    character: u8,
    frcolor: usize,
    bkcolor: usize,
) -> Result<i32, SystemError> {
    if window.flags == TEXTUI_WF_CHROMATIC {
        // 启用彩色字符
        let mut s_vline = 0;
        if let TextuiVline::Chromatic(vline) = &mut (window.vlines[window.vline_operating as usize])
        {
            vline.chars[vline.index as usize].c = character;
            vline.chars[vline.index as usize].frcolor = frcolor & 0xffffff;
            vline.chars[vline.index as usize].bkcolor = bkcolor & 0xffffff;
            vline.index += 1;
            s_vline = vline.index;
        }
        textui_refresh_characters(window, window.vline_operating, s_vline, 1); // 换行
                                                                               // 加入光标后，因为会识别光标，所以需超过该行最大字符数才能创建新行
        if let TextuiVline::Chromatic(vline) = &mut (window.vlines[window.vline_operating as usize])
        {
            if vline.index > window.chars_per_line {
                textui_new_line(window, window.vline_operating as u16);
            }
        }
    } else {
        // todo: 支持纯文本字符
        todo!();
    }
    return Ok(0);
}

/**
 * @brief 在指定窗口上输出一个字符
 *
 * @param window 窗口
 * @param character 字符
 * @param FRcolor 前景色（RGB）
 * @param BKcolor 背景色（RGB）
 * @return int
 */
fn textui_putchar_window(
    window: &mut TextuiWindow,
    character: u8,
    frcolor: u32,
    bkcolor: u32,
) -> Result<i32, SystemError> {
    if unlikely(character == b'\0') {
        return Ok(0);
    }
    if window.flags != TEXTUI_WF_CHROMATIC {
        return Ok(0);
    }

    // let window=LockedtextuiWindow(SpinLock::new(window));
    c_uart_send(UartPort::COM1.to_u16(), character);
    if unlikely(character == b'\n') {
        // 换行时还需要输出\r
        c_uart_send(UartPort::COM1.to_u16(), b'\r');
        textui_new_line(window, window.vline_operating as u16);
        // spin_unlock_no_preempt(&window.lock);
        return Ok(0);
    } else if character == b'\t' {
        // 输出制表符
        if let TextuiVline::Chromatic(vline) = &window.vlines[window.vline_operating as usize] {
            let mut space_to_print = 8 - vline.index % 8;
            while space_to_print > 0 {
                ture_textui_putchar_window(window, b' ', frcolor as usize, bkcolor as usize);
                space_to_print -= 1;
            }
        }
    } else if character == b'\x08' {
        // 退格
        let mut tmp = 0;
        if let TextuiVline::Chromatic(vline) = &mut window.vlines[window.vline_operating as usize] {
            vline.index -= 1;
            tmp = vline.index;
        }
        if tmp >= 0 {
            if let TextuiVline::Chromatic(vline) =
                &mut window.vlines[window.vline_operating as usize]
            {
                vline.chars[tmp as usize].c = b' ';
                vline.chars[tmp as usize].bkcolor = bkcolor as usize & 0xffffff;
            }
            textui_refresh_characters(window, window.vline_operating, tmp, 1);
        }
        // 需要向上缩一行
        if tmp <= 0 {
            // 当前行为空,重新刷新
            if let TextuiVline::Chromatic(vline) =
                &mut window.vlines[window.vline_operating as usize]
            {
                vline.index = 0;
                for i in 0..window.chars_per_line {
                    vline.chars.push(TextuiCharChromatic::new());
                }
            }
            // 上缩一行
            window.vline_operating -= 1;
            if window.vline_operating < 0 {
                window.vline_operating = window.vline_sum - 1;
            }

            // 考虑是否向上滚动
            if let ScmUiPrivateInfo::Textui(private_info) =
                &TEXTUIFRAMEWORK.0.lock().metadata.private_info
            {
                if window.vlines_used > private_info.actual_line {
                    window.top_vline -= 1;
                    if window.top_vline < 0 {
                        window.top_vline = window.vline_sum - 1;
                    }
                }
                window.vlines_used -= 1;
                textui_refresh_vlines(
                    window,
                    window.top_vline as u16,
                    private_info.actual_line as u16,
                );
            }
        }
    } else {
        // 输出其他字符
        if let TextuiVline::Chromatic(vline) = &window.vlines[window.vline_operating as usize] {
            if vline.index == window.chars_per_line {
                textui_new_line(window, window.vline_operating as u16);
            }
            return textui_putchar_window(window, character, frcolor, bkcolor);
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
    if let ScmUiPrivateInfo::Textui(private_info) = &TEXTUIFRAMEWORK.0.lock().metadata.private_info {
        let window = &private_info.default_window;
        return textui_putchar_window(window.clone().as_mut(), character, fr_color, bk_color).unwrap();
    } else {
        return 0;
    }
}
/**
 * @brief 初始化虚拟行对象
 *
 * @param vline 虚拟行对象指针
 * @param chars_ptr 字符对象数组指针
 */
fn textui_init_vline(vline: &mut TextuiVline, chars_ptr: Vec<TextuiCharChromatic>) {
    if let TextuiVline::Chromatic(vline) = vline {
        vline.index = 0;
        vline.chars = chars_ptr;
    }
}

/**
 * @brief 初始化text ui框架
 *
 * @return int
 */
#[no_mangle]
pub extern "C" fn textui_init() -> i32 {
    // spin_init(&change_lock);

    // spin_init(&__window_id_lock);

    // Barrier();
    let name: &str = "textui";
    TEXTUIFRAMEWORK
        .0.lock()
        .metadata
        .name
        .copy_from_slice(name.as_bytes());
    TEXTUIFRAMEWORK.0.lock().metadata.f_type = ScmFramworkType::Text;
    // 注册框架到屏幕管理器
    let textui=TextUiFramework{ metadata:TEXTUIFRAMEWORK.0.lock().metadata.clone() };
    let retval = scm_register(Arc::new(textui));
    if retval != 0 {
        c_uart_send_str(UartPort::COM1.to_u16(), "text ui init failed\n".as_ptr());
        loop {}
    }

    let chars_per_vline = TEXTUIFRAMEWORK.0.lock().metadata.buf.width / TEXTUI_CHAR_WIDTH;
    let total_vlines = TEXTUIFRAMEWORK.0.lock().metadata.buf.height / TEXTUI_CHAR_HEIGHT;
    let cnt = chars_per_vline * total_vlines;

    let vl_ptr =  &mut INITIAL_VLINES.clone() ;
    let ch_ptr = &mut INITIAL_CHARS.clone() ;

    // 初始化虚拟行
    for i in 0..total_vlines {
        textui_init_vline(
            &mut vl_ptr[i as usize],
            ch_ptr[(i * chars_per_vline) as usize..((i + 1) * chars_per_vline) as usize].to_vec(),
        )
    }

    // 初始化窗口

    TextuiWindow::init_window(
        unsafe { &mut INITIAL_WINDOW.lock() },
        TEXTUI_WF_CHROMATIC,
        total_vlines as i16,
        unsafe { INITIAL_VLINES.to_vec() },
        chars_per_vline as i16,
    );
    if let ScmUiPrivateInfo::Textui(private_info) =
        &mut TEXTUIFRAMEWORK.0.lock().metadata.private_info
    {
        private_info.current_window = unsafe { Box::new(INITIAL_WINDOW.lock().clone()) };
        private_info.default_window = unsafe { Box::new(INITIAL_WINDOW.lock().clone()) };
        private_info.actual_line =
            (TEXTUIFRAMEWORK.0.lock().metadata.buf.height / TEXTUI_CHAR_HEIGHT) as i16;
    }

    c_uart_send_str(UartPort::COM1.to_u16(), "text ui initialized\n".as_ptr());
    return 0;
}
