use core::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use bitmap::{traits::BitMapOps, StaticBitmap};

use crate::{
    driver::{
        serial::serial8250::send_to_default_serial8250_port,
        tty::{console::ConsoleSwitch, ConsoleFont, KDMode},
    },
    libs::{font::FontDesc, rwlock::RwLock},
    process::Pid,
};

use super::{
    console_map::{TranslationMap, TranslationMapType},
    Color, DrawRegion, VtMode, VtModeData, COLOR_TABLE, DEFAULT_BLUE, DEFAULT_GREEN, DEFAULT_RED,
};

pub(super) const NPAR: usize = 16;

lazy_static! {
    /// 是否已经添加了软光标
    pub(super) static ref SOFTCURSOR_ORIGINAL: RwLock<Option<VcCursor>> = RwLock::new(None);

    pub static ref CURRENT_VCNUM: AtomicIsize = AtomicIsize::new(-1);

    pub static ref CONSOLE_BLANKED: AtomicBool = AtomicBool::new(false);
}

/// ## 虚拟控制台的信息
#[derive(Debug, Clone)]
pub struct VirtualConsoleData {
    pub num: usize,
    pub state: VirtualConsoleInfo,
    pub saved_state: VirtualConsoleInfo,
    /// 最大列数
    pub cols: usize,
    /// 最大行数
    pub rows: usize,
    // ///  每行的字节数
    // pub bytes_per_row: usize,
    /// 扫描行数
    pub scan_lines: usize,
    /// 字符单元高度
    pub cell_height: u32,

    // /// 实际屏幕地址的开始
    // pub screen_base: VirtAddr,
    // /// 实际屏幕的结束
    // pub scr_end: u64,
    /// 可见窗口的开始
    pub visible_origin: usize,
    /// 滚动窗口的顶部
    pub top: usize,
    /// 滚动窗口的底部
    pub bottom: usize,
    /// 当前读取位置
    pub pos: usize,

    /// 颜色集合
    pub palette: [Color; 16],
    /// 默认颜色
    pub def_color: u8,
    /// 下划线颜色
    pub underline_color: u32,
    /// 斜体颜色
    pub italic_color: u32,
    /// 半强度颜色
    pub half_color: u32,

    pub mode: KDMode,
    pub vt_mode: VtModeData,

    /// 是否启用颜色
    pub color_mode: bool,

    // 字符
    pub hi_font_mask: u16,
    pub font: ConsoleFont,

    pub erase_char: u16,

    pub complement_mask: u16,
    pub s_complement_mask: u16,

    pub cursor_blink_ms: u16,

    pub pid: Option<Pid>,
    pub index: usize,

    pub vc_state: VirtualConsoleState,

    // 一些标志
    /// 指示是否显示 ASCII 字符小于 32 的控制字符(vc_disp_ctrl)
    pub display_ctrl: bool,
    /// 指示是否切换高位（meta）位。Meta 键是一个特殊的按键，用于扩展字符集。
    pub toggle_meta: bool,
    /// 表示屏幕模式(vc_decscnm)
    pub screen_mode: bool,
    /// 指定光标移动的起始位置，是相对于屏幕的左上角还是相对于当前页的左上角(vc_decom)
    pub origin_mode: bool,
    /// 控制光标到达行末时是否自动换行(vc_decawm)
    pub autowrap: bool,
    /// 控制光标的可见性(vc_deccm)
    pub cursor_visible: bool,
    /// 光标相关
    pub cursor_type: VcCursor,
    /// 控制插入或替换模式(vc_decim)
    pub insert_mode: bool,
    /// 表示一些私有模式或状态，通常由特定终端实现定义(vc_priv)
    pub private: Vt102_OP,
    /// 是否需要进行自动换行
    pub need_wrap: bool,
    /// 控制鼠标事件的报告方式
    pub report_mouse: u8,
    /// 指示终端是否使用 UTF-8 编码
    pub utf: bool,
    /// UTF-8 编码的字符计数，表示还需要多少个字节才能够构建完成
    pub utf_count: u8,
    /// UTF-8 编码的字符，表示正在构建的utf字符
    pub utf_char: u32,
    /// 构建utf时需要的参数，表示目前接收了多少个字节的数据来构建utf字符
    pub npar: u32,
    ///
    pub par: [u32; NPAR],

    /// 字符转换表 用于将输入字符映射到特定的字符
    pub translate: TranslationMap,

    pub tab_stop: StaticBitmap<256>,

    pub attr: u8,

    /// vc缓冲区
    pub screen_buf: Vec<u16>,

    /// 对应的Console Driver funcs
    driver_funcs: Option<Weak<dyn ConsoleSwitch>>,
}

impl VirtualConsoleData {
    #[inline(never)]
    pub fn new(num: usize) -> Self {
        Self {
            state: VirtualConsoleInfo::new(0, 0),
            saved_state: Default::default(),
            cols: Default::default(),
            rows: Default::default(),
            // bytes_per_row: Default::default(),
            scan_lines: Default::default(),
            cell_height: Default::default(),
            // origin: Default::default(),
            // scr_end: Default::default(),
            visible_origin: Default::default(),
            top: Default::default(),
            bottom: Default::default(),
            palette: [Default::default(); 16],
            def_color: Default::default(),
            underline_color: Default::default(),
            italic_color: Default::default(),
            half_color: Default::default(),
            mode: Default::default(),
            color_mode: Default::default(),
            hi_font_mask: Default::default(),
            erase_char: Default::default(),
            complement_mask: Default::default(),
            s_complement_mask: Default::default(),
            cursor_blink_ms: 200,
            pos: Default::default(),
            vt_mode: VtModeData {
                mode: VtMode::Auto,
                relsig: 0,
                acqsig: 0,
            },
            pid: None,
            index: 0,
            font: Default::default(),
            vc_state: VirtualConsoleState::ESnormal,
            display_ctrl: Default::default(),
            toggle_meta: Default::default(),
            screen_mode: Default::default(),
            origin_mode: Default::default(),
            autowrap: Default::default(),
            cursor_visible: Default::default(),
            insert_mode: Default::default(),
            private: Vt102_OP::EPecma,
            need_wrap: Default::default(),
            report_mouse: Default::default(),
            utf: Default::default(),
            utf_count: Default::default(),
            utf_char: Default::default(),
            translate: TranslationMap::new(TranslationMapType::Lat1Map),
            npar: Default::default(),
            tab_stop: StaticBitmap::new(),
            par: [0; 16],
            attr: Default::default(),
            screen_buf: Default::default(),
            driver_funcs: None,
            cursor_type: VcCursor::empty(),
            num,
        }
    }

    pub(super) fn init(&mut self, rows: Option<usize>, cols: Option<usize>, clear: bool) {
        if rows.is_some() {
            self.rows = rows.unwrap();
        }
        if cols.is_some() {
            self.cols = cols.unwrap();
        }

        self.pos = self.cols * self.state.y + self.state.x;
        // self.bytes_per_row = self.cols << 1;

        self.def_color = 15; // white
        self.italic_color = 2; // green
        self.underline_color = 3; // cyan
        self.half_color = 0x08; // grey

        self.reset(clear);

        self.screen_buf.resize(self.cols * self.rows, 0);
    }

    pub fn should_update(&self) -> bool {
        self.is_visible() && !CONSOLE_BLANKED.load(Ordering::SeqCst)
    }

    pub fn is_visible(&self) -> bool {
        let cur_vc = CURRENT_VCNUM.load(Ordering::SeqCst);
        if cur_vc == -1 {
            return false;
        }

        cur_vc as usize == self.num
    }

    fn driver_funcs(&self) -> Arc<dyn ConsoleSwitch> {
        self.driver_funcs.as_ref().unwrap().upgrade().unwrap()
    }

    pub(super) fn set_driver_funcs(&mut self, func: Weak<dyn ConsoleSwitch>) {
        self.driver_funcs = Some(func);
    }

    pub(super) fn reset(&mut self, do_clear: bool) {
        self.mode = KDMode::KdText;
        // unicode?
        self.vt_mode.mode = VtMode::Auto;
        self.vt_mode.acqsig = 0;
        self.vt_mode.relsig = 0;
        self.display_ctrl = false;
        self.toggle_meta = false;
        self.screen_mode = false;
        self.origin_mode = false;
        self.autowrap = true;
        self.cursor_visible = true;
        self.insert_mode = false;
        self.need_wrap = false;
        self.report_mouse = 0;
        self.utf_count = 0;
        self.translate = TranslationMap::new(TranslationMapType::Lat1Map);
        self.utf = true;
        self.pid = None;
        self.vc_state = VirtualConsoleState::ESnormal;
        self.reset_palette();
        // self.cursor_type = VcCursor::CUR_UNDERLINE;
        self.cursor_type = VcCursor::CUR_BLOCK;

        self.default_attr();
        self.update_attr();

        self.tab_stop.set_all(false);

        for i in (0..256).step_by(8) {
            self.tab_stop.set(i, true);
        }

        self.state.x = 0;
        self.state.y = 0;
        self.pos = 0;

        if do_clear {
            self.csi_J(2);
        }
    }

    fn reset_palette(&mut self) {
        for (idx, color) in self.palette.iter_mut().enumerate() {
            color.red = DEFAULT_RED[idx];
            color.green = DEFAULT_GREEN[idx];
            color.blue = DEFAULT_BLUE[idx];
        }

        self.set_palette();
    }

    fn set_palette(&self) {
        if self.mode != KDMode::KdGraphics {
            // todo: 通知driver层的Console
            let _ = self.driver_funcs().con_set_palette(self, COLOR_TABLE);
        }
    }

    /// ## 翻译字符，将字符转换为终端控制符
    /// ### 参数
    ///
    /// ### c: 需要转换的字符
    ///
    /// ### 返回值
    /// ### （转换后的字符:i32，是否需要更多的数据才能进行转换:bool）
    pub(super) fn translate(&mut self, c: &mut u32) -> (Option<u32>, bool) {
        if self.vc_state != VirtualConsoleState::ESnormal {
            // 在控制字符状态下不需要翻译
            return (Some(*c), false);
        }
        if self.utf && !self.display_ctrl {
            // utf模式并且不显示控制字符
            let (ret, rescan) = self.translate_unicode(*c);
            if ret.is_some() {
                *c = ret.unwrap();
            }
            return (ret, rescan);
        }

        return (Some(self.translate_ascii(*c)), false);
    }

    /// 该数组包含每个字节序列长度变化的阈值
    /// 即如果由两个字节组成的unicode字符，则长度应该在UTF8_LENGTH_CHANGES[0] ~ UTF8_LENGTH_CHANGES[1]之间
    const UTF8_LENGTH_CHANGES: &'static [u32] = &[
        0x0000007f, 0x000007ff, 0x0000ffff, 0x001fffff, 0x03ffffff, 0x7fffffff,
    ];

    /// ## 翻译字符，将UTF-8 编码的字符转换为 Unicode 编码
    /// ### 参数
    ///
    /// ### c: 需要转换的字符
    ///
    /// ### 返回值
    /// ### （转换后的字符:i32，是否需要重新传入该字符:bool）
    ///
    /// !!! 注意，该函数返回true时，元组的第一个数据是无效数据（未转换完成）
    fn translate_unicode(&mut self, c: u32) -> (Option<u32>, bool) {
        // 收到的字符不是首个
        if (c & 0xc8) == 0x80 {
            // 已经不需要继续的字符了，说明这个字符是非法的
            if self.utf_count == 0 {
                return (Some(0xfffd), false);
            }

            self.utf_char = (self.utf_char << 6) | (c & 0x3f);
            self.npar += 1;

            self.utf_count -= 1;
            if self.utf_count > 0 {
                // 表示需要更多字节
                return (None, false);
            }

            let c = self.utf_char;

            // 先检查一遍是否合格
            if c <= Self::UTF8_LENGTH_CHANGES[self.npar as usize - 1]
                || c > Self::UTF8_LENGTH_CHANGES[self.npar as usize]
            {
                return (Some(0xfffd), false);
            }

            return (Some(Self::sanitize_unicode(c)), false);
        }

        // 接收到单个ASCII字符或者一个序列的首字符,且上次的未处理完,则上一个字符视为无效，则需要重新传入该字符处理
        if self.utf_count > 0 {
            self.utf_count = 0;
            return (Some(0xfffd), true);
        }

        // ascii
        if c <= 0x7f {
            return (Some(c), false);
        }

        // 第一个字节
        self.npar = 0;
        if (c & 0xe0) == 0xc0 {
            self.utf_count = 1;
            self.utf_char = c & 0x1f;
        } else if (c & 0xf0) == 0xe0 {
            self.utf_count = 2;
            self.utf_char = c & 0x0f;
        } else if (c & 0xf8) == 0xf0 {
            self.utf_count = 3;
            self.utf_char = c & 0x07;
        } else if (c & 0xfc) == 0xf8 {
            self.utf_count = 4;
            self.utf_char = c & 0x03;
        } else if (c & 0xfe) == 0xfc {
            self.utf_count = 5;
            self.utf_char = c & 0x01;
        } else {
            /* 254 and 255 are invalid */
            return (Some(0xfffd), false);
        }

        (None, false)
    }

    /// ## 翻译字符，将字符转换为Ascii
    fn translate_ascii(&self, c: u32) -> u32 {
        let mut c = c;
        if self.toggle_meta {
            c |= 0x80;
        }

        return self.translate.translate(c) as u32;
    }

    /// ## 用于替换无效的 Unicode 代码点（code points）。
    /// Unicode 代码点的范围是从 U+0000 到 U+10FFFF，
    /// 但是有一些特殊的代码点是无效的或者保留给特定用途的。
    /// 这个函数的主要目的是将无效的 Unicode 代码点替换为 U+FFFD，即 Unicode 替代字符。
    fn sanitize_unicode(c: u32) -> u32 {
        if (c >= 0xd800 && c <= 0xdfff) || c == 0xfffe || c == 0xffff {
            return 0xfffd;
        }
        return c;
    }

    /// 用于表示小于 32 的字符中，哪些字符对应的位被设置为 1，
    /// 表示这些字符会触发一些特殊的动作，比如光标移动等。
    /// 这些字符在 disp_ctrl 模式未开启时不应该被显示为图形符号
    const CTRL_ACTION: u32 = 0x0d00ff81;
    /// 用于表示哪些控制字符是始终显示的，即便 disp_ctrl 模式未开启。
    /// 这些字符对于终端来说是必要的，显示它们是为了保证终端正常工作。
    /// 这些字符在 disp_ctrl 模式开启或关闭时都应该显示为控制字符。
    const CTRL_ALWAYS: u32 = 0x0800f501;

    /// ## 用于判断tc(终端字符)在当前VC下是不是需要显示的控制字符
    pub(super) fn is_control(&self, tc: u32, c: u32) -> bool {
        // 当前vc状态机不在正常状态，即在接收特殊字符的状态，则是控制字符
        if self.vc_state != VirtualConsoleState::ESnormal {
            return true;
        }

        if tc == 0 {
            return true;
        }

        if c < 32 {
            if self.display_ctrl {
                // 查看在位图中是否有该字符
                return Self::CTRL_ALWAYS & (1 << c) != 0;
            } else {
                return self.utf || (Self::CTRL_ACTION & (1 << c) != 0);
            }
        }

        if c == 127 && !self.display_ctrl {
            return true;
        }

        if c == 128 + 27 {
            return true;
        }

        false
    }

    pub(super) fn set_cursor(&mut self) {
        if self.mode == KDMode::KdGraphics {
            return;
        }

        if self.cursor_visible {
            // TODO: 处理选择
            self.add_softcursor();
            if self.cursor_type.cursor_size() != VcCursor::CUR_NONE {
                self.driver_funcs().con_cursor(self, CursorOperation::Draw);
            }
        } else {
            self.hide_cursor();
        }
    }

    /// ## 添加软光标
    fn add_softcursor(&mut self) {
        let mut i = self.screen_buf[self.pos] as u32;
        let cursor_type = self.cursor_type;

        if !cursor_type.contains(VcCursor::CUR_SW) {
            return;
        }

        if SOFTCURSOR_ORIGINAL.read_irqsave().is_some() {
            // 已经设置了软光标
            return;
        }

        let mut soft_cursor_guard = SOFTCURSOR_ORIGINAL.write_irqsave();
        *soft_cursor_guard = Some(unsafe { VcCursor::from_bits_unchecked(i as u32) });

        let soft_cursor = soft_cursor_guard.unwrap();

        i |= cursor_type.cursor_set();
        i ^= cursor_type.cursor_change();
        if cursor_type.contains(VcCursor::CUR_ALWAYS_BG)
            && ((soft_cursor.bits & VcCursor::CUR_BG.bits) == (i & VcCursor::CUR_BG.bits))
        {
            i ^= VcCursor::CUR_BG.bits;
        }
        if cursor_type.contains(VcCursor::CUR_INVERT_FG_BG)
            && ((i & VcCursor::CUR_FG.bits) == ((i & VcCursor::CUR_BG.bits) >> 4))
        {
            i ^= VcCursor::CUR_FG.bits;
        }

        self.screen_buf[self.pos] = i as u16;

        let _ =
            self.driver_funcs()
                .con_putc(&self, i as u16, self.state.y as u32, self.state.x as u32);
    }

    pub fn hide_cursor(&mut self) {
        // TODO: 处理选择

        self.driver_funcs().con_cursor(self, CursorOperation::Erase);
        self.hide_softcursor();
    }

    fn hide_softcursor(&mut self) {
        let softcursor = SOFTCURSOR_ORIGINAL.upgradeable_read_irqsave();
        if softcursor.is_some() {
            self.screen_buf[self.pos] = softcursor.unwrap().bits as u16;
            let _ = self.driver_funcs().con_putc(
                &self,
                softcursor.unwrap().bits as u16,
                self.state.y as u32,
                self.state.x as u32,
            );

            *softcursor.upgrade() = None;
        }
    }

    fn gotoxay(&mut self, x: i32, y: i32) {
        if self.origin_mode {
            self.gotoxy(x, self.top as i32 + y);
        } else {
            self.gotoxy(x, y)
        }
    }

    // ## 将当前vc的光标移动到目标位置
    fn gotoxy(&mut self, x: i32, y: i32) {
        if x < 0 {
            self.state.x = 0;
        } else {
            if x as usize >= self.cols {
                self.state.x = self.cols - 1;
            } else {
                self.state.x = x as usize;
            }
        }

        let max_y;
        let min_y;
        if self.origin_mode {
            min_y = self.top;
            max_y = self.bottom - 1;
        } else {
            min_y = 0;
            max_y = self.rows - 1;
        }

        if y < min_y as i32 {
            self.state.y = min_y as usize;
        } else if y >= max_y as i32 {
            self.state.y = max_y as usize;
        } else {
            self.state.y = y as usize;
        }

        self.pos = self.state.y * self.cols + self.state.x;
        self.need_wrap = false;
    }

    fn scroll(&mut self, dir: ScrollDir, mut nr: usize) {
        // todo: uniscr_srceen
        if self.top + nr >= self.bottom {
            // 滚动超过一页,则按一页计算
            nr = self.bottom - self.top - 1;
        }

        if nr < 1 {
            return;
        }

        if self.is_visible()
            && self
                .driver_funcs()
                .con_scroll(self, self.top, self.bottom, dir, nr)
        {
            // 如果成功
            return;
        }

        // 调整screen_buf
        let count = nr * self.cols;
        if dir == ScrollDir::Up {
            for i in self.screen_buf[0..count].iter_mut() {
                *i = self.erase_char;
            }
            self.screen_buf.rotate_left(count);
        } else if dir == ScrollDir::Down {
            todo!();
        }
    }

    /// ## 退格
    fn backspace(&mut self) {
        if self.state.x > 0 {
            self.pos -= 1;
            self.state.x -= 1;
            self.need_wrap = false;

            // TODO: notify
        }
    }

    /// ## 换行
    fn line_feed(&mut self) {
        if self.state.y + 1 == self.bottom as usize {
            self.scroll(ScrollDir::Up, 1);
        } else if self.state.y < self.rows - 1 {
            self.state.y += 1;
            self.pos += self.cols;
        }

        self.need_wrap = false;
        // TODO: Notify write
    }

    /// ## 回车
    fn carriage_return(&mut self) {
        // 写入位置回退到该行最前
        self.pos -= self.state.x;
        self.need_wrap = false;
        self.state.x = 0;
    }

    /// ## Del
    fn delete(&mut self) {
        // ignore
    }

    /// ## 向上滚动虚拟终端的内容，或者将光标上移一行
    fn reverse_index(&mut self) {
        if self.state.y == self.top as usize {
            self.scroll(ScrollDir::Down, 1);
        } else if self.state.y > 0 {
            self.state.y -= 1;
            self.pos -= self.cols;
        }
        self.need_wrap = false;
    }

    /// https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/tty/vt/vt.c#restore_cur
    fn restore_cursor(&mut self) {
        self.saved_state = self.state.clone();

        self.gotoxy(self.state.x as i32, self.state.y as i32);

        // TODO Gx_charset

        self.update_attr();
        self.need_wrap = false;
        todo!()
    }

    /// ## 设置当前vt的各项属性
    fn set_mode(&mut self, on_off: bool) {
        for i in 0..self.npar as usize {
            if self.private == Vt102_OP::EPdec {
                match self.par[i] {
                    1 => {
                        todo!("kbd todo");
                    }
                    3 => {
                        todo!("reisze todo");
                    }
                    5 => {
                        todo!("invert_screen todo");
                    }
                    6 => {
                        self.origin_mode = on_off;
                        if on_off {
                            self.gotoxy(0, self.top as i32);
                        } else {
                            self.gotoxy(0, 0);
                        }
                    }
                    7 => {
                        self.autowrap = on_off;
                    }
                    8 => {
                        todo!("kbd todo");
                    }
                    9 => {
                        todo!("report mouse todo");
                    }
                    25 => {
                        self.cursor_visible = on_off;
                    }
                    1000 => {
                        todo!("report mouse todo");
                    }
                    _ => {}
                }
            } else {
                match self.par[i] {
                    3 => {
                        self.display_ctrl = on_off;
                    }
                    4 => {
                        self.insert_mode = on_off;
                    }
                    20 => {
                        todo!("kbd todo");
                    }
                    _ => {}
                }
            }
        }
    }

    #[inline(never)]
    fn do_getpars(&mut self, c: char) {
        if c == ';' && self.npar < (NPAR - 1) as u32 {
            self.npar += 1;
            return;
        }

        if c >= '0' && c <= '9' {
            self.par[self.npar as usize] *= 10;
            self.par[self.npar as usize] += (c as u8 - '0' as u8) as u32;
            return;
        }

        if c as u8 >= 0x20 && c as u8 <= 0x3f {
            self.vc_state = VirtualConsoleState::EScsiignore;
            return;
        }

        self.vc_state = VirtualConsoleState::ESnormal;

        match c {
            'h' => {
                if self.private <= Vt102_OP::EPdec {
                    self.set_mode(true);
                }
                return;
            }
            'l' => {
                if self.private <= Vt102_OP::EPdec {
                    self.set_mode(false);
                }
                return;
            }
            'c' => {
                if self.private == Vt102_OP::EPdec {
                    if self.par[0] != 0 {
                        self.cursor_type =
                            VcCursor::make_cursor(self.par[0], self.par[1], self.par[2])
                    } else {
                        self.cursor_type = VcCursor::CUR_UNDERLINE;
                    }
                    return;
                }
            }
            'm' => {
                if self.private == Vt102_OP::EPdec {
                    if self.par[0] != 0 {
                        self.complement_mask = (self.par[0] << 8 | self.par[1]) as u16;
                    } else {
                        self.complement_mask = self.s_complement_mask;
                    }
                    return;
                }
            }
            'n' => {
                if self.private == Vt102_OP::EPecma {
                    if self.par[0] == 5 {
                        send_to_default_serial8250_port("tty status report todo".as_bytes());
                        panic!();
                    } else if self.par[0] == 6 {
                        send_to_default_serial8250_port("tty cursor report todo".as_bytes());
                        panic!();
                    }
                }
                return;
            }
            _ => {}
        }

        if self.private != Vt102_OP::EPecma {
            self.private = Vt102_OP::EPecma;
            return;
        }

        match c {
            'G' | '`' => {
                if self.par[0] != 0 {
                    self.par[0] -= 1;
                }
                self.gotoxy(self.par[0] as i32, self.state.y as i32);
                return;
            }
            'A' => {
                if self.par[0] == 0 {
                    self.par[0] += 1;
                }
                self.gotoxy(
                    self.state.x as i32,
                    (self.state.y - self.par[0] as usize) as i32,
                );
                return;
            }
            'B' | 'e' => {
                if self.par[0] == 0 {
                    self.par[0] += 1;
                }
                self.gotoxy(
                    self.state.x as i32,
                    (self.state.y + self.par[0] as usize) as i32,
                );
                return;
            }
            'C' | 'a' => {
                if self.par[0] == 0 {
                    self.par[0] += 1;
                }
                self.gotoxy(
                    (self.state.x + self.par[0] as usize) as i32,
                    self.state.y as i32,
                );
                return;
            }
            'D' => {
                if self.par[0] == 0 {
                    self.par[0] += 1;
                }
                self.gotoxy(
                    self.state.x as i32 - self.par[0] as i32,
                    self.state.y as i32,
                );
                return;
            }
            'E' => {
                if self.par[0] == 0 {
                    self.par[0] += 1;
                }
                self.gotoxy(0, (self.state.y + self.par[0] as usize) as i32);
                return;
            }
            'F' => {
                if self.par[0] == 0 {
                    self.par[0] += 1;
                }
                self.gotoxy(0, self.state.y as i32 - self.par[0] as i32);
                return;
            }
            'd' => {
                if self.par[0] != 0 {
                    self.par[0] -= 1;
                }
                self.gotoxay(self.state.x as i32, self.par[0] as i32);
                return;
            }
            'H' | 'f' => {
                // MOVETO
                if self.par[0] != 0 {
                    self.par[0] -= 1;
                }
                if self.par[1] != 0 {
                    self.par[1] -= 1;
                }
                self.gotoxay(self.par[1] as i32, self.par[0] as i32);
                return;
            }
            'J' => {
                self.csi_J(self.par[0]);
                return;
            }
            'K' => {
                self.csi_K(self.par[0]);
                return;
            }
            'L' => {
                todo!("csi_L todo");
            }
            'M' => {
                todo!("csi_M todo");
            }
            'P' => {
                todo!("csi_P todo");
            }

            // 非ANSI标准，为ANSI拓展
            'S' => {
                self.scroll(ScrollDir::Up, self.par[0] as usize);
                return;
            }

            'T' => {
                self.scroll(ScrollDir::Down, self.par[0] as usize);
                return;
            }

            'c' => {
                if self.par[0] == 0 {
                    kwarn!("respone ID todo");
                }
                return;
            }
            'g' => {
                if self.par[0] == 0 && self.state.x < 256 {
                    self.tab_stop.set(self.state.x as usize, true);
                } else if self.par[0] == 3 {
                    self.tab_stop.set_all(false);
                }
                return;
            }
            'm' => {
                self.csi_m();
                return;
            }
            'q' => {
                if self.par[0] < 4 {
                    todo!("vt set led state todo");
                }
                return;
            }
            'r' => {
                if self.par[0] == 0 {
                    self.par[0] += 1;
                }
                if self.par[1] == 0 {
                    self.par[1] = self.rows as u32;
                }
                if self.par[0] < self.par[1] && self.par[1] <= self.rows as u32 {
                    self.top = self.par[0] as usize - 1;
                    self.bottom = self.par[1] as usize;
                    self.gotoxay(0, 0);
                }
                return;
            }
            's' => {
                self.saved_state = self.state.clone();
                return;
            }
            'u' => {
                self.restore_cursor();
                return;
            }
            '@' => {
                todo!("csi_at todo");
            }
            ']' => {
                todo!("set termial command todo");
            }
            _ => {}
        }
    }

    /// ##  处理Control Sequence Introducer（控制序列引导符） m字符
    #[inline(never)]
    fn csi_m(&mut self) {
        let mut i = 0;
        loop {
            if i > self.npar as usize {
                break;
            }
            match self.par[i] {
                0 => {
                    // 关闭所有属性
                    self.default_attr();
                }

                1 => {
                    // 设置粗体
                    self.state.intensity = VirtualConsoleIntensity::VciBold;
                }

                2 => {
                    // 设置半亮度（半明显
                    self.state.intensity = VirtualConsoleIntensity::VciHalfBright;
                }

                3 => {
                    // 斜体
                    self.state.italic = true;
                }

                4 | 21 => {
                    // 下划线

                    // 21是设置双下划线，但是不支持，就单下划线
                    self.state.underline = true;
                }

                5 => {
                    // 设置闪烁
                    self.state.blink = true;
                }

                7 => {
                    // 设置反显（前景色与背景色对调）
                    self.state.reverse = true;
                }

                10 => {
                    // 选择主要字体
                    todo!()
                }

                11 => {
                    // 选择第一个替代字体
                    todo!()
                }

                12 => {
                    // 选择第二个替代字体
                    todo!()
                }

                22 => {
                    //  关闭粗体和半亮度，恢复正常亮度
                    self.state.intensity = VirtualConsoleIntensity::VciNormal;
                }

                23 => {
                    // 关闭斜体
                    self.state.italic = false;
                }

                24 => {
                    // 关闭下划线
                    self.state.underline = false;
                }

                25 => {
                    // 关闭字符闪烁
                    self.state.blink = false;
                }

                27 => {
                    // 关闭反显
                    self.state.reverse = false;
                }

                38 => {
                    // 设置前景色
                    let (idx, color) = self.t416_color(i);
                    i = idx;
                    if color.is_some() {
                        let color = color.unwrap();
                        let mut max = color.red.max(color.green);
                        max = max.max(color.blue);

                        let mut hue = 0;
                        if color.red > max / 2 {
                            hue |= 4;
                        }
                        if color.green > max / 2 {
                            hue |= 2;
                        }
                        if color.blue > max / 2 {
                            hue |= 1;
                        }

                        if hue == 7 && max <= 0x55 {
                            hue = 0;
                            self.state.intensity = VirtualConsoleIntensity::VciBold;
                        } else if max > 0xaa {
                            self.state.intensity = VirtualConsoleIntensity::VciBold;
                        } else {
                            self.state.intensity = VirtualConsoleIntensity::VciNormal;
                        }

                        self.state.color = (self.state.color & 0xf0) | hue;
                    }
                }

                48 => {
                    // 设置背景色
                    let (idx, color) = self.t416_color(i);
                    i = idx;
                    if color.is_some() {
                        let color = color.unwrap();
                        self.state.color = (self.state.color & 0x0f)
                            | ((color.red as u8 & 0x80) >> 1)
                            | ((color.green as u8 & 0x80) >> 2)
                            | ((color.blue as u8 & 0x80) >> 3);
                    }
                }

                39 => {
                    // 恢复默认前景色
                    self.state.color = (self.def_color & 0x0f) | (self.state.color & 0xf0);
                }

                49 => {
                    // 恢复默认背景色
                    self.state.color = (self.def_color & 0xf0) | (self.state.color & 0x0f);
                }

                _ => {
                    if self.par[i] >= 90 && self.par[i] <= 107 {
                        if self.par[i] < 100 {
                            self.state.intensity = VirtualConsoleIntensity::VciBold;
                        }
                        self.par[i] -= 60;
                    }

                    if self.par[i] >= 30 && self.par[i] <= 37 {
                        self.state.color =
                            COLOR_TABLE[self.par[i] as usize - 30] | self.state.color & 0xf0;
                    } else if self.par[i] >= 40 && self.par[i] <= 47 {
                        self.state.color =
                            (COLOR_TABLE[self.par[i] as usize - 40] << 4) | self.state.color & 0xf0;
                    }
                }
            }

            i += 1;
        }

        self.update_attr();
    }

    /// ##  处理Control Sequence Introducer（控制序列引导符） J字符
    /// 该命令用于擦除终端显示区域的部分或全部内容。根据参数 vpar 的不同值，执行不同的擦除操作：
    /// - vpar 为 0 时，擦除从光标位置到显示区域末尾的内容；
    /// - vpar 为 1 时，擦除从显示区域起始位置到光标位置的内容；
    /// - vpar 为 2 或 3 时，分别表示擦除整个显示区域的内容，其中参数 3 还会清除回滚缓冲区的内容。
    #[allow(non_snake_case)]
    fn csi_J(&mut self, vpar: u32) {
        let count;
        let start;

        match vpar {
            0 => {
                // 擦除从光标位置到显示区域末尾的内容
                count = self.screen_buf.len() - self.pos;
                start = self.pos;
            }
            1 => {
                // 擦除从显示区域起始位置到光标位置的内容
                count = self.pos;
                start = 0;
            }
            2 => {
                // 擦除整个显示区域的内容
                count = self.screen_buf.len();
                start = 0;
            }
            3 => {
                // 表示擦除整个显示区域的内容，还会清除回滚缓冲区的内容
                // TODO:当前未实现回滚缓冲
                count = self.screen_buf.len();
                start = 0;
            }
            _ => {
                return;
            }
        }

        for i in self.screen_buf[start..(start + count)].iter_mut() {
            *i = self.erase_char;
        }

        if self.should_update() {
            self.do_update_region(start, count)
        }

        self.need_wrap = false;
    }

    /// ##  处理Control Sequence Introducer（控制序列引导符） K字符
    /// 该命令用于擦除终端当前行的部分或全部内容。根据参数 vpar 的不同值，执行不同的擦除操作：
    /// - vpar 为 0 时，擦除从光标位置到该行末尾的内容
    /// - vpar 为 1 时，擦除从该行起始位置到光标位置的内容
    /// - vpar 为 2 时，擦除整个行。
    #[allow(non_snake_case)]
    fn csi_K(&mut self, vpar: u32) {
        let count;
        let start;

        match vpar {
            0 => {
                // 擦除从光标位置到该行末尾的内容
                count = self.cols - self.state.x;
                start = self.pos;
            }
            1 => {
                // 擦除从该行起始位置到光标位置的内容
                count = self.state.x + 1;
                start = self.pos - self.state.x;
            }
            2 => {
                // 擦除整个行
                count = self.cols;
                start = self.pos - self.state.x;
            }
            _ => {
                return;
            }
        }

        let max_idx = self.screen_buf.len();
        for i in self.screen_buf[start..max_idx.min(start + count)].iter_mut() {
            *i = self.erase_char;
        }

        if self.should_update() {
            self.do_update_region(start, count.min(max_idx - start))
        }

        self.need_wrap = false;
    }

    fn t416_color(&mut self, mut idx: usize) -> (usize, Option<Color>) {
        idx += 1;
        if idx > self.npar as usize {
            return (idx, None);
        }

        if self.par[idx] == 5 && idx + 1 <= self.npar as usize {
            // 256色
            idx += 1;
            return (idx, Some(Color::from_256(self.par[idx])));
        } else if self.par[idx] == 2 && idx + 3 <= self.npar as usize {
            // 24位
            let mut color = Color::default();
            color.red = self.par[idx + 1] as u16;
            color.green = self.par[idx + 2] as u16;
            color.blue = self.par[idx + 3] as u16;
            idx += 3;
            return (idx, Some(color));
        } else {
            return (idx, None);
        }
    }

    /// ## 处理终端控制字符
    #[inline(never)]
    pub(super) fn do_control(&mut self, ch: u32) {
        // 首先检查是否处于 ANSI 控制字符串状态
        if self.vc_state.is_ansi_control_string() && ch >= 8 && ch <= 13 {
            return;
        }

        match ch {
            0 => {
                return;
            }
            7 => {
                // BEL
                if self.vc_state.is_ansi_control_string() {
                    self.vc_state = VirtualConsoleState::ESnormal;
                }
                // TODO: 发出声音？
                return;
            }
            8 => {
                // BS backspace
                self.backspace();
                return;
            }
            9 => {
                // 水平制表符（Horizontal Tab）
                self.pos -= self.state.x;

                let ret = self.tab_stop.next_index(self.state.x + 1);

                if ret.is_none() {
                    self.state.x = self.cols - 1;
                } else {
                    self.state.x = ret.unwrap();
                }

                self.pos += self.state.x;
                // TODO: notify
                return;
            }
            10 | 11 | 12 => {
                // LD line feed
                self.line_feed();
                // TODO: 检查键盘模式
                self.carriage_return();
                return;
            }
            13 => {
                // CR 回车符
                self.carriage_return();
                return;
            }
            14 => {
                todo!("Gx_charset todo!");
            }
            15 => {
                todo!("Gx_charset todo!");
            }
            24 | 26 => {
                self.vc_state = VirtualConsoleState::ESnormal;
                return;
            }
            27 => {
                // esc
                self.vc_state = VirtualConsoleState::ESesc;
                return;
            }
            127 => {
                // delete
                self.delete();
                return;
            }
            155 => {
                // '['
                self.vc_state = VirtualConsoleState::ESsquare;
                return;
            }
            _ => {}
        }

        match self.vc_state {
            VirtualConsoleState::ESesc => {
                self.vc_state = VirtualConsoleState::ESnormal;
                match ch as u8 as char {
                    '[' => {
                        self.vc_state = VirtualConsoleState::ESsquare;
                    }
                    ']' => {
                        self.vc_state = VirtualConsoleState::ESnonstd;
                    }
                    '_' => {
                        self.vc_state = VirtualConsoleState::ESapc;
                    }
                    '^' => {
                        self.vc_state = VirtualConsoleState::ESpm;
                    }
                    '%' => {
                        self.vc_state = VirtualConsoleState::ESpercent;
                    }
                    'E' => {
                        self.carriage_return();
                        self.line_feed();
                    }
                    'M' => {
                        self.reverse_index();
                    }
                    'D' => {
                        self.line_feed();
                    }
                    'H' => {
                        if self.state.x < 256 {
                            self.tab_stop.set(self.state.x, true);
                        }
                    }
                    'P' => {
                        self.vc_state = VirtualConsoleState::ESdcs;
                    }
                    'Z' => {
                        todo!("Respond ID todo!");
                    }
                    '7' => self.saved_state = self.state.clone(),
                    '8' => self.restore_cursor(),
                    '(' => {
                        self.vc_state = VirtualConsoleState::ESsetG0;
                    }
                    ')' => {
                        self.vc_state = VirtualConsoleState::ESsetG1;
                    }
                    '#' => {
                        self.vc_state = VirtualConsoleState::EShash;
                    }
                    'c' => {
                        self.reset(true);
                    }
                    '>' => {
                        todo!("clr_kbd todo");
                    }
                    '=' => {
                        todo!("set_kbd todo");
                    }
                    _ => {}
                }
            }
            VirtualConsoleState::ESsquare => {
                for i in self.par.iter_mut() {
                    *i = 0;
                }
                self.vc_state = VirtualConsoleState::ESgetpars;
                self.npar = 0;
                let c = ch as u8 as char;
                if c == '[' {
                    self.vc_state = VirtualConsoleState::ESfunckey;
                    return;
                }

                match c {
                    '?' => {
                        self.private = Vt102_OP::EPdec;
                        return;
                    }
                    '>' => {
                        self.private = Vt102_OP::EPgt;
                        return;
                    }
                    '=' => {
                        self.private = Vt102_OP::EPeq;
                        return;
                    }
                    '<' => {
                        self.private = Vt102_OP::EPlt;
                        return;
                    }
                    _ => {}
                }

                self.private = Vt102_OP::EPecma;
                self.do_getpars(c);
            }
            VirtualConsoleState::ESgetpars => {
                let c = ch as u8 as char;
                self.do_getpars(c);
            }
            VirtualConsoleState::ESfunckey => {
                self.vc_state = VirtualConsoleState::ESnormal;
                return;
            }
            VirtualConsoleState::EShash => {
                self.vc_state = VirtualConsoleState::ESnormal;
                if ch as u8 as char == '8' {
                    self.erase_char = (self.erase_char & 0xff00) | 'E' as u16;
                    self.csi_J(2);
                    self.erase_char = (self.erase_char & 0xff00) | ' ' as u16;
                    self.do_update_region(0, self.screen_buf.len());
                }
                return;
            }
            VirtualConsoleState::ESsetG0 => {
                todo!("SetGx todo");
            }
            VirtualConsoleState::ESsetG1 => {
                todo!("SetGx todo");
            }
            VirtualConsoleState::ESpercent => {
                self.vc_state = VirtualConsoleState::ESnormal;
                let c = ch as u8 as char;
                match c {
                    '@' => {
                        self.utf = false;
                        return;
                    }
                    'G' | '8' => {
                        self.utf = true;
                        return;
                    }
                    _ => {}
                }
                return;
            }
            VirtualConsoleState::EScsiignore => {
                if ch >= 20 && ch <= 0x3f {
                    return;
                }
                self.vc_state = VirtualConsoleState::ESnormal;
                return;
            }
            VirtualConsoleState::ESnonstd => {
                let c = ch as u8 as char;
                if c == 'P' {
                    for i in self.par.iter_mut() {
                        *i = 0;
                    }
                    self.npar = 0;
                    self.vc_state = VirtualConsoleState::ESpalette;
                    return;
                } else if c == 'R' {
                    self.reset_palette();
                    self.vc_state = VirtualConsoleState::ESnormal;
                } else if c >= '0' && c <= '9' {
                    self.vc_state = VirtualConsoleState::ESosc;
                } else {
                    self.vc_state = VirtualConsoleState::ESnormal;
                }
            }
            VirtualConsoleState::ESpalette => {
                let c = ch as u8 as char;
                if c.is_digit(16) {
                    self.npar += 1;
                    self.par[self.npar as usize] = c.to_digit(16).unwrap();

                    if self.npar == 7 {
                        let mut i = self.par[0] as usize;
                        let mut j = 0;
                        self.palette[i].red = self.par[j] as u16;
                        j += 1;
                        self.palette[i].green = self.par[j] as u16;
                        j += 1;
                        self.palette[i].blue = self.par[j] as u16;
                        j += 1;
                        i += 1;
                        self.palette[i].red = self.par[j] as u16;
                        j += 1;
                        self.palette[i].green = self.par[j] as u16;
                        j += 1;
                        self.palette[i].blue = self.par[j] as u16;
                        self.set_palette();
                        self.vc_state = VirtualConsoleState::ESnormal;
                    }
                }
            }
            VirtualConsoleState::ESosc => {}
            VirtualConsoleState::ESapc => {}
            VirtualConsoleState::ESpm => {}
            VirtualConsoleState::ESdcs => {}
            VirtualConsoleState::ESnormal => {}
        }
    }

    #[inline(never)]
    pub(super) fn console_write_normal(
        &mut self,
        mut tc: u32,
        c: u32,
        draw: &mut DrawRegion,
    ) -> bool {
        let mut attr = self.attr;
        let himask = self.hi_font_mask;
        let charmask = if himask == 0 { 0xff } else { 0x1ff };
        let mut width = 1;
        // 表示需不需要反转
        let mut invert = false;
        if self.utf && !self.display_ctrl {
            if FontDesc::is_double_width(c) {
                width = 2;
            }
        }

        let tmp = self.unicode_to_index(tc);
        if tmp & (!charmask as i32) != 0 {
            if tmp == -1 || tmp == -2 {
                return false;
            }

            // 未找到
            if (!self.utf || self.display_ctrl || c < 128) && c & !charmask == 0 {
                tc = c;
            } else {
                let tmp = self.unicode_to_index(0xfffd);
                if tmp < 0 {
                    invert = true;
                    let tmp = self.unicode_to_index('?' as u32);
                    if tmp < 0 {
                        tc = '?' as u32;
                    } else {
                        tc = tmp as u32;
                    }

                    attr = self.invert_attr();
                    self.flush(draw);
                }
            }
        }

        loop {
            if self.need_wrap || self.insert_mode {
                self.flush(draw);
            }
            if self.need_wrap {
                self.carriage_return();
                self.line_feed();
            }

            if self.insert_mode {
                self.insert_char(1);
            }

            // TODO: 处理unicode screen buf

            if himask != 0 {
                tc = ((if tc & 0x100 != 0 { himask as u32 } else { 0 }) | (tc & 0xff)) as u32;
            }

            tc |= ((attr as u32) << 8) & (!himask as u32);

            // kwarn!(
            //     "ch {} pos {} x {} y {} cols {}",
            //     c as u8 as char,
            //     self.pos,
            //     self.state.x,
            //     self.state.y,
            //     self.cols,
            // );
            self.screen_buf[self.pos] = tc as u16;

            if draw.x.is_none() {
                // 设置draw参数
                draw.x = Some(self.state.x as u32);
                draw.offset = self.pos;
            }

            if self.state.x == self.cols - 1 {
                // 需要换行？
                self.need_wrap = self.autowrap;
                draw.size += 1;
            } else {
                self.state.x += 1;
                self.pos += 1;
                draw.size += 1;
            }

            width -= 1;
            if width == 0 {
                break;
            }
            let tmp = self.unicode_to_index(' ' as u32);
            tc = if tmp < 0 { ' ' as u32 } else { tmp as u32 };
        }

        if invert {
            self.flush(draw);
        }

        true
    }

    /// ## 当前vc插入nr个字符
    fn insert_char(&mut self, nr: usize) {
        // TODO: 管理unicode屏幕信息

        let pos = self.pos;
        // 把当前位置以后得字符向后移动nr*2位
        self.screen_buf[pos..].rotate_right(nr * 2);

        // 把空出来的位置用erase_char填充
        for c in &mut self.screen_buf[pos..(pos + nr * 2)] {
            *c = self.erase_char
        }

        self.need_wrap = false;

        // 更新本行后面部分
        self.do_update_region(self.pos, self.cols - self.state.x);
    }

    /// ## 更新虚拟控制台指定区域的显示
    fn do_update_region(&self, mut start: usize, mut count: usize) {
        let ret = self.driver_funcs().con_getxy(self, start);
        let (mut x, mut y) = if ret.is_err() {
            (start % self.cols, start / self.cols)
        } else {
            let (_, tmp_x, tmp_y) = ret.unwrap();
            // start = tmp_start;
            (tmp_x, tmp_y)
        };

        loop {
            // 记录当前字符的属性
            let mut attr = self.screen_buf[start] & 0xff00;
            let mut startx = x;
            let mut size = 0;

            while count != 0 && x < self.cols {
                // 检查属性是否变化，如果属性变了，则将前一个字符先输出
                if attr != (self.screen_buf[start] & 0xff00) {
                    if size > 0 {
                        let _ = self.driver_funcs().con_putcs(
                            self,
                            &self.screen_buf[start..],
                            size,
                            y as u32,
                            startx as u32,
                        );
                        startx = x;
                        start += size;
                        size = 0;
                        attr = self.screen_buf[start] & 0xff00;
                    }
                }
                size += 1;
                x += 1;
                count -= 1;
            }

            if size > 0 {
                let _ = self.driver_funcs().con_putcs(
                    self,
                    &self.screen_buf[start..],
                    size,
                    y as u32,
                    startx as u32,
                );
            }
            if count == 0 {
                break;
            }

            // 一行
            x = 0;
            y += 1;

            let ret = self.driver_funcs().con_getxy(self, start);
            if ret.is_ok() {
                start = ret.unwrap().0;
            } else {
                return;
            }
        }
    }

    const UNI_DIRECT_MAKS: u32 = 0x01ff;
    const UNI_DIRECT_BASE: u32 = 0xf000;
    /// ## unicode字符转对应的坐标，暂时这样写，还没有适配unicode
    /// 这里是糊代码的，后面重写
    fn unicode_to_index(&self, ch: u32) -> i32 {
        if ch > 0xfff {
            // 未找到
            return -4;
        } else if ch < 0x20 {
            // 不可打印
            return -1;
        } else if ch == 0xfeff || (ch >= 0x200b && ch <= 0x200f) {
            // 零长空格
            return -2;
        } else if (ch & !Self::UNI_DIRECT_MAKS) == Self::UNI_DIRECT_BASE {
            return (ch & Self::UNI_DIRECT_MAKS) as i32;
        }

        // TODO: 暂时这样写，表示不支持
        return -3;
    }

    fn invert_attr(&self) -> u8 {
        if !self.color_mode {
            return self.attr ^ 0x08;
        }

        if self.hi_font_mask == 0x100 {
            return (self.attr & 0x11) | ((self.attr & 0xe0) >> 4) | ((self.attr & 0x0e) << 4);
        }

        return (self.attr & 0x88) | ((self.attr & 0x70) >> 4) | ((self.attr & 0x07) << 4);
    }

    pub(super) fn flush(&self, draw: &mut DrawRegion) {
        if draw.x.is_none() {
            return;
        }

        let _ = self.driver_funcs().con_putcs(
            self,
            &self.screen_buf[draw.offset..draw.offset + draw.size],
            draw.size,
            self.state.y as u32,
            draw.x.unwrap() as u32,
        );

        draw.x = None;
        draw.size = 0;
    }

    fn build_attr(
        &self,
        color: u8,
        intensity: VirtualConsoleIntensity,
        blink: bool,
        underline: bool,
        reverse: bool,
        italic: bool,
    ) -> u8 {
        let ret = self
            .driver_funcs()
            .con_build_attr(self, color, intensity, blink, underline, reverse, italic);

        if ret.is_ok() {
            return ret.unwrap();
        }

        let mut ret = color;

        if !self.color_mode {
            return intensity as u8
                | (italic as u8) << 1
                | (underline as u8) << 2
                | (reverse as u8) << 3
                | (blink as u8) << 7;
        }

        if italic {
            ret = (ret & 0xf0) | self.italic_color as u8;
        } else if underline {
            ret = (ret & 0xf0) | self.underline_color as u8;
        } else if intensity == VirtualConsoleIntensity::VciHalfBright {
            ret = (ret & 0xf0) | self.half_color as u8;
        }

        if reverse {
            ret = (ret & 0x88) | (((ret >> 4) | (ret << 4)) & 0x77);
        }

        if blink {
            ret ^= 0x80;
        }

        if intensity == VirtualConsoleIntensity::VciBold {
            ret ^= 0x08;
        }

        if self.hi_font_mask == 0x100 {
            ret <<= 1;
        }

        ret
    }

    pub(super) fn update_attr(&mut self) {
        self.attr = self.build_attr(
            self.state.color,
            self.state.intensity,
            self.state.blink,
            self.state.underline,
            self.state.reverse ^ self.screen_mode,
            self.state.italic,
        );

        self.erase_char = ' ' as u16
            | ((self.build_attr(
                self.state.color,
                VirtualConsoleIntensity::VciNormal,
                self.state.blink,
                false,
                self.screen_mode,
                false,
            ) as u16)
                << 8);
    }

    fn default_attr(&mut self) {
        self.state.intensity = VirtualConsoleIntensity::VciNormal;
        self.state.italic = false;
        self.state.underline = false;
        self.state.reverse = false;
        self.state.blink = false;
        self.state.color = self.def_color;
    }
}

/// ## 虚拟控制台的状态信息
#[derive(Debug, Default, Clone)]
pub struct VirtualConsoleInfo {
    // x,y表示光标坐标
    pub x: usize,
    pub y: usize,
    pub color: u8,

    /// 表示字符的强度
    intensity: VirtualConsoleIntensity,
    /// 斜体
    italic: bool,
    /// 下划线
    underline: bool,
    /// 字符闪烁
    blink: bool,
    /// 前景与背景色反转
    reverse: bool,
}

impl VirtualConsoleInfo {
    pub fn new(x: usize, y: usize) -> Self {
        Self {
            x,
            y,
            color: Default::default(),
            intensity: Default::default(),
            italic: Default::default(),
            underline: Default::default(),
            blink: Default::default(),
            reverse: Default::default(),
        }
    }
}

/// 字符强度
#[derive(Debug, Clone, PartialEq, Copy)]
pub enum VirtualConsoleIntensity {
    /// 暗淡
    VciHalfBright = 0,
    /// 正常
    VciNormal = 1,
    /// 粗体
    VciBold = 2,
}

impl Default for VirtualConsoleIntensity {
    fn default() -> Self {
        Self::VciNormal
    }
}

/// ## 虚拟控制台的状态
///
/// 可以把VC的接收字符理解为一个状态机
#[derive(Debug, PartialEq, Clone)]
pub enum VirtualConsoleState {
    /// 正常状态
    ESnormal,
    /// 收到了转义字符 \e，即"Escape"字符
    ESesc,
    /// 收到了 "[" 字符，通常是 ANSI 控制序列的开始
    ESsquare,
    /// 解析参数状态
    ESgetpars,
    /// 功能键状态
    ESfunckey,
    /// 收到了 "#" 字符
    EShash,
    /// 设置 G0 字符集状态
    ESsetG0,
    /// 设置 G1 字符集状态
    ESsetG1,
    /// 收到了 "%" 字符
    ESpercent,
    /// 忽略 ANSI 控制序列状态
    EScsiignore,
    /// 非标准字符状态
    ESnonstd,
    /// 调色板状态
    ESpalette,
    /// Operating System Command (OSC) 状态
    ESosc,
    ///  Application Program Command (APC) 状态
    ESapc,
    /// Privacy Message (PM) 状态
    ESpm,
    /// Device Control String (DCS) 状态
    ESdcs,
}

impl VirtualConsoleState {
    pub fn is_ansi_control_string(&self) -> bool {
        if *self == Self::ESosc
            || *self == Self::ESapc
            || *self == Self::ESpm
            || *self == Self::ESdcs
        {
            return true;
        }

        false
    }
}

#[derive(Debug, Clone, PartialEq, PartialOrd)]
#[allow(non_camel_case_types)]
pub enum Vt102_OP {
    EPecma,
    EPdec,
    EPeq,
    EPgt,
    EPlt,
}

bitflags! {
    #[derive(Default)]
    pub struct VcCursor: u32 {
        /// 默认
        const CUR_DEF			=       0;
        /// 无光标
        const CUR_NONE			=       1;
        /// 下划线形式
        const CUR_UNDERLINE		=	    2;
        /// 光标占据底部的三分之一
        const CUR_LOWER_THIRD	=	    3;
        /// 光标占据底部的一半
        const CUR_LOWER_HALF	=       4;
        ///  光标占据底部的三分之二
        const CUR_TWO_THIRDS	=       5;
        /// 光标为块状（方块）形式
        const CUR_BLOCK			=       6;
        /// 光标属性，用于指示软件光标
        const CUR_SW			=	0x000010;
        /// 光标属性，用于指示光标是否始终在背景上显示
        const CUR_ALWAYS_BG		=	0x000020;
        /// 光标属性，用于指示前景和背景是否反转
        const CUR_INVERT_FG_BG	=	0x000040;
        /// 光标前景色属性，用于指定光标的前景色
        const CUR_FG			=	0x000700;
        /// 光标背景色属性，用于指定光标的背景色
        const CUR_BG			=	0x007000;
    }
}

impl VcCursor {
    pub fn make_cursor(size: u32, change: u32, set: u32) -> Self {
        unsafe { Self::from_bits_unchecked(size | (change << 8) | (set << 16)) }
    }

    pub fn cursor_size(&self) -> Self {
        Self::from_bits_truncate(self.bits & 0x00000f)
    }

    pub fn cursor_set(&self) -> u32 {
        (self.bits & 0xff0000) >> 8
    }

    pub fn cursor_change(&self) -> u32 {
        self.bits & 0x00ff00
    }
}

#[derive(Debug, PartialEq)]
#[allow(dead_code)]
pub enum CursorOperation {
    Draw,
    Erase,
    Move,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ScrollDir {
    Up,
    Down,
}
