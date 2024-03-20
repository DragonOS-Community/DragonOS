use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{
    driver::{
        tty::{
            console::ConsoleSwitch,
            virtual_terminal::{
                virtual_console::{CursorOperation, ScrollDir, VcCursor, VirtualConsoleData},
                Color,
            },
        },
        video::fbdev::base::{
            CopyAreaData, FbCursor, FbCursorSetMode, FbImage, FbVisual, FillRectData, FillRectROP,
            FrameBuffer, ScrollMode, FRAME_BUFFER_SET,
        },
    },
    libs::{
        font::FontDesc,
        spinlock::{SpinLock, SpinLockGuard},
    },
};

use super::{FbConAttr, FrameBufferConsole, FrameBufferConsoleData};

#[derive(Debug)]
pub struct BlittingFbConsole {
    fb: SpinLock<Option<Arc<dyn FrameBuffer>>>,
    fbcon_data: SpinLock<FrameBufferConsoleData>,
}

unsafe impl Send for BlittingFbConsole {}
unsafe impl Sync for BlittingFbConsole {}

impl BlittingFbConsole {
    pub fn new() -> Result<Self, SystemError> {
        Ok(Self {
            fb: SpinLock::new(None),
            fbcon_data: SpinLock::new(FrameBufferConsoleData::default()),
        })
    }

    pub fn fb(&self) -> Arc<dyn FrameBuffer> {
        self.fb.lock().clone().unwrap()
    }

    pub fn get_color(&self, vc_data: &VirtualConsoleData, c: u16, is_fg: bool) -> u32 {
        let fb_info = self.fb();
        let mut color = 0;

        let depth = fb_info.color_depth();

        if depth != 1 {
            if is_fg {
                let fg_shift = if vc_data.hi_font_mask != 0 { 9 } else { 8 };
                color = (c as u32 >> fg_shift) & 0x0f
            } else {
                let bg_shift = if vc_data.hi_font_mask != 0 { 13 } else { 12 };
                color = (c as u32 >> bg_shift) & 0x0f
            }
        }

        match depth {
            1 => {
                let col = self.mono_color();
                let fg;
                let bg;
                if fb_info.current_fb_fix().visual != FbVisual::Mono01 {
                    fg = col;
                    bg = 0;
                } else {
                    fg = 0;
                    bg = col;
                }
                color = if is_fg { fg } else { bg };
            }
            2 => {
                /*
                    颜色深度为2，即16色，
                   将16色的颜色值映射到4色的灰度，
                   其中颜色0映射为黑色，颜色1到6映射为白色，
                   颜色7到8映射为灰色，其他颜色映射为强烈的白色。
                */
                if color >= 1 && color <= 6 {
                    // 白色
                    color = 2;
                } else if color >= 7 && color <= 8 {
                    // 灰色
                    color = 1;
                } else {
                    // 强白
                    color = 3;
                }
            }
            3 => {
                /*
                   颜色深度为3，即256色，仅保留颜色的低3位，即颜色 0 到 7
                */
                color &= 7;
            }
            _ => {}
        }
        color
    }

    /// ## 计算单色调的函数
    pub fn mono_color(&self) -> u32 {
        let fb_info = self.fb();
        let mut max_len = fb_info
            .current_fb_var()
            .green
            .length
            .max(fb_info.current_fb_var().red.length);

        max_len = max_len.max(fb_info.current_fb_var().blue.length);

        return (!(0xfff << max_len)) & 0xff;
    }

    pub fn bit_put_string(
        &self,
        vc_data: &VirtualConsoleData,
        buf: &[u16],
        attr: FbConAttr,
        cnt: u32,
        cellsize: u32,
        image: &mut FbImage,
    ) {
        let charmask = if vc_data.hi_font_mask != 0 {
            0x1ff
        } else {
            0xff
        };

        let mut offset;
        let image_line_byte = image.width as usize / 8;

        let byte_width = vc_data.font.width as usize / 8;
        let font_height = vc_data.font.height as usize;
        // let mut char_offset = 0;
        for char_offset in 0..cnt as usize {
            // 在字符表中的index
            let ch = buf[char_offset] & charmask;
            // 计算出在font表中的偏移量
            let font_offset = ch as usize * cellsize as usize;
            let font_offset_end = font_offset + cellsize as usize;
            // 设置image的data

            let src = &vc_data.font.data[font_offset..font_offset_end];
            let mut dst = Vec::new();
            dst.resize(src.len(), 0);
            dst.copy_from_slice(src);

            if !attr.is_empty() {
                attr.update_attr(&mut dst, src, vc_data)
            }

            offset = char_offset * byte_width;
            let mut dst_offset = 0;
            for _ in 0..font_height {
                let dst_offset_next = dst_offset + byte_width;
                image.data[offset..offset + byte_width]
                    .copy_from_slice(&dst[dst_offset..dst_offset_next]);

                offset += image_line_byte;
                dst_offset = dst_offset_next;
            }
        }

        self.fb().fb_image_blit(image);
    }
}

impl ConsoleSwitch for BlittingFbConsole {
    fn con_init(
        &self,
        vc_data: &mut VirtualConsoleData,
        init: bool,
    ) -> Result<(), system_error::SystemError> {
        let fb_set_guard = FRAME_BUFFER_SET.read();
        let fb = fb_set_guard.get(vc_data.index);
        if fb.is_none() {
            return Err(SystemError::EINVAL);
        }
        let fb = fb.unwrap();
        if fb.is_none() {
            panic!(
                "The Framebuffer with FbID {} has not been initialized yet.",
                vc_data.index
            )
        }

        let fb = fb.as_ref().unwrap().clone();

        if init {
            // 初始化字体
            let var = fb.current_fb_var();
            let font = FontDesc::get_default_font(var.xres, var.yres, 0, 0);
            vc_data.font.data = font.data.to_vec();
            vc_data.font.width = font.width;
            vc_data.font.height = font.height;
            vc_data.font.count = font.char_count;
        } else {
            kwarn!("The frontend Framebuffer is not implemented");
        }

        vc_data.color_mode = fb.color_depth() != 1;
        vc_data.complement_mask = if vc_data.color_mode { 0x7700 } else { 0x0800 };

        if vc_data.font.count == 256 {
            // ascii
            vc_data.hi_font_mask = 0;
        } else {
            vc_data.hi_font_mask = 0x100;
            if vc_data.color_mode {
                vc_data.complement_mask <<= 1;
            }
        }

        // TODO: 考虑rotate
        if init {
            vc_data.cols = (fb.current_fb_var().xres / vc_data.font.width) as usize;
            vc_data.rows = (fb.current_fb_var().yres / vc_data.font.height) as usize;

            vc_data.pos = vc_data.state.x + vc_data.state.y * vc_data.cols;

            let new_size = vc_data.cols * vc_data.rows;
            vc_data.screen_buf.resize(new_size, 0);
        } else {
            unimplemented!("Resize is not supported at the moment!");
        }

        // 初始化fb
        *self.fb.lock() = Some(fb);

        Ok(())
    }

    fn con_deinit(&self) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn con_clear(
        &self,
        vc_data: &mut VirtualConsoleData,
        sy: usize,
        sx: usize,
        height: usize,
        width: usize,
    ) -> Result<(), system_error::SystemError> {
        let fb_data = self.fbcon_data();

        if height == 0 || width == 0 {
            return Ok(());
        }

        let y_break = (fb_data.display.virt_rows - fb_data.display.yscroll) as usize;
        if sy < y_break && sy + height - 1 >= y_break {
            // 分两次clear
            let b = y_break - sy;
            let _ = self.clear(
                &vc_data,
                fb_data.display.real_y(sy as u32),
                sx as u32,
                b as u32,
                width as u32,
            );
            let _ = self.clear(
                &vc_data,
                fb_data.display.real_y((sy + b) as u32),
                sx as u32,
                (height - b) as u32,
                width as u32,
            );
        } else {
            let _ = self.clear(
                &vc_data,
                fb_data.display.real_y(sy as u32),
                sx as u32,
                height as u32,
                width as u32,
            );
        }

        Ok(())
    }

    fn con_putc(
        &self,
        vc_data: &VirtualConsoleData,
        ch: u16,
        xpos: u32,
        ypos: u32,
    ) -> Result<(), system_error::SystemError> {
        self.con_putcs(vc_data, &[ch], 1, ypos, xpos)
    }

    fn con_putcs(
        &self,
        vc_data: &VirtualConsoleData,
        buf: &[u16],
        count: usize,
        ypos: u32,
        xpos: u32,
    ) -> Result<(), SystemError> {
        if count == 0 {
            return Ok(());
        }
        let fbcon_data = self.fbcon_data();
        let c = buf[0];
        self.put_string(
            vc_data,
            buf,
            count as u32,
            fbcon_data.display.real_y(ypos),
            xpos,
            self.get_color(vc_data, c, true),
            self.get_color(vc_data, c, false),
        )
    }

    fn con_getxy(
        &self,
        vc_data: &VirtualConsoleData,
        pos: usize,
    ) -> Result<(usize, usize, usize), SystemError> {
        if pos < vc_data.screen_buf.len() {
            let x = pos % vc_data.cols;
            let y = pos / vc_data.cols;
            let mut next_line_start = pos + (vc_data.cols - x);
            if next_line_start >= vc_data.screen_buf.len() {
                next_line_start = 0
            }
            return Ok((next_line_start, x, y));
        } else {
            return Ok((0, 0, 0));
        }
    }

    fn con_cursor(
        &self,
        vc_data: &VirtualConsoleData,
        op: crate::driver::tty::virtual_terminal::virtual_console::CursorOperation,
    ) {
        let mut fbcon_data = self.fbcon_data();

        let c = vc_data.screen_buf[vc_data.pos];

        if vc_data.cursor_type.contains(VcCursor::CUR_SW) {
            // 取消硬光标Timer，但是现在没有硬光标，先写在这
        } else {
            // 添加硬光标Timer
        }

        fbcon_data.cursor_flash = op != CursorOperation::Erase;

        drop(fbcon_data);

        self.cursor(
            vc_data,
            op,
            self.get_color(vc_data, c, true),
            self.get_color(vc_data, c, false),
        );
    }

    fn con_set_palette(
        &self,
        vc_data: &VirtualConsoleData,
        color_table: &[u8],
    ) -> Result<(), SystemError> {
        let fb_info = self.fb();
        let depth = fb_info.color_depth();
        let mut palette = Vec::new();
        palette.resize(16, Color::default());
        if depth > 3 {
            let vc_palette = &vc_data.palette;
            for i in 0..16 {
                let idx = color_table[i];
                let col = palette.get_mut(idx as usize).unwrap();
                col.red = (vc_palette[i].red << 8) | vc_palette[i].red;
                col.green = (vc_palette[i].green << 8) | vc_palette[i].green;
                col.blue = (vc_palette[i].blue << 8) | vc_palette[i].blue;
            }
        } else {
            todo!()
        }

        self.fb().set_color_map(palette)?;

        Ok(())
    }

    #[inline(never)]
    fn con_scroll(
        &self,
        vc_data: &mut VirtualConsoleData,
        top: usize,
        bottom: usize,
        dir: crate::driver::tty::virtual_terminal::virtual_console::ScrollDir,
        mut count: usize,
    ) -> bool {
        self.con_cursor(vc_data, CursorOperation::Erase);

        let fbcon_data = self.fbcon_data();
        let scroll_mode = fbcon_data.display.scroll_mode;

        drop(fbcon_data);

        match dir {
            ScrollDir::Up => {
                if count > vc_data.rows {
                    count = vc_data.rows;
                }

                match scroll_mode {
                    ScrollMode::Move => {
                        let start = top * vc_data.cols;
                        let end = bottom * vc_data.cols;
                        vc_data.screen_buf[start..end].rotate_left(count * vc_data.cols);

                        let _ = self.bmove(
                            vc_data,
                            top as i32,
                            0,
                            top as i32 - count as i32,
                            0,
                            (bottom - top) as u32,
                            vc_data.cols as u32,
                        );

                        let _ = self.con_clear(vc_data, bottom - count, 0, count, vc_data.cols);

                        let offset = vc_data.cols * (bottom - count);
                        for i in
                            vc_data.screen_buf[offset..(offset + (vc_data.cols * count))].iter_mut()
                        {
                            *i = vc_data.erase_char;
                        }

                        return true;
                    }
                    ScrollMode::PanMove => todo!(),
                    ScrollMode::WrapMove => todo!(),
                    ScrollMode::Redraw => {
                        let start = top * vc_data.cols;
                        let end = bottom * vc_data.cols;
                        vc_data.screen_buf[start..end].rotate_left(count * vc_data.cols);

                        let data = &vc_data.screen_buf[start..(bottom - count) * vc_data.cols];

                        for line in top..(bottom - count) {
                            let mut start = line * vc_data.cols;
                            let end = start + vc_data.cols;
                            let mut offset = start;
                            let mut attr = 1;
                            let mut x = 0;
                            while offset < end {
                                let c = data[offset];

                                if attr != c & 0xff00 {
                                    // 属性变化，输出完上一个的并且更新属性
                                    attr = c & 0xff00;

                                    let count = offset - start;
                                    let _ = self.con_putcs(
                                        vc_data,
                                        &data[start..offset],
                                        count,
                                        line as u32,
                                        x,
                                    );
                                    start = offset;
                                    x += count as u32;
                                }

                                offset += 1;
                            }
                            let _ = self.con_putcs(
                                vc_data,
                                &data[start..offset],
                                offset - start,
                                line as u32,
                                x,
                            );
                        }

                        let _ = self.con_clear(vc_data, bottom - count, 0, count, vc_data.cols);

                        let offset = vc_data.cols * (bottom - count);
                        for i in
                            vc_data.screen_buf[offset..(offset + (vc_data.cols * count))].iter_mut()
                        {
                            *i = vc_data.erase_char;
                        }

                        return true;
                    }
                    ScrollMode::PanRedraw => todo!(),
                }
            }
            ScrollDir::Down => {
                if count > vc_data.rows {
                    count = vc_data.rows;
                }

                match scroll_mode {
                    ScrollMode::Move => {
                        let start = top * vc_data.cols;
                        let end = bottom * vc_data.cols;
                        vc_data.screen_buf[start..end].rotate_right(count * vc_data.cols);

                        let _ = self.bmove(
                            vc_data,
                            top as i32,
                            0,
                            top as i32 + count as i32,
                            0,
                            (bottom - top - count) as u32,
                            vc_data.cols as u32,
                        );

                        let _ = self.con_clear(vc_data, top, 0, count, vc_data.cols);

                        let offset = vc_data.cols * count;
                        for i in vc_data.screen_buf[start..(start + offset)].iter_mut() {
                            *i = vc_data.erase_char;
                        }

                        return true;
                    }
                    ScrollMode::PanMove => todo!(),
                    ScrollMode::WrapMove => todo!(),
                    ScrollMode::Redraw => {
                        // self.scroll_redraw(
                        //     vc_data,
                        //     bottom - 1,
                        //     bottom - top - count,
                        //     count * vc_data.cols,
                        //     false,
                        // );

                        let _ = self.con_clear(vc_data, top, 0, count, vc_data.cols);

                        let offset = vc_data.cols * top;
                        for i in
                            vc_data.screen_buf[offset..(offset + (vc_data.cols * count))].iter_mut()
                        {
                            *i = vc_data.erase_char;
                        }

                        return true;
                    }
                    ScrollMode::PanRedraw => todo!(),
                }
            }
        }
    }
}

impl FrameBufferConsole for BlittingFbConsole {
    fn bmove(
        &self,
        vc_data: &VirtualConsoleData,
        sy: i32,
        sx: i32,
        dy: i32,
        dx: i32,
        height: u32,
        width: u32,
    ) -> Result<(), SystemError> {
        let area = CopyAreaData::new(
            dx * vc_data.font.width as i32,
            dy * vc_data.font.height as i32,
            width * vc_data.font.width,
            height * vc_data.font.height,
            sx * vc_data.font.width as i32,
            sy * vc_data.font.height as i32,
        );

        self.fb().fb_copyarea(area);
        Ok(())
    }

    fn clear(
        &self,
        vc_data: &VirtualConsoleData,
        sy: u32,
        sx: u32,
        height: u32,
        width: u32,
    ) -> Result<(), SystemError> {
        let region = FillRectData::new(
            sx * vc_data.font.width,
            sy * vc_data.font.height,
            width * vc_data.font.width,
            height * vc_data.font.height,
            self.get_color(vc_data, vc_data.erase_char, false),
            FillRectROP::Copy,
        );

        self.fb().fb_fillrect(region)?;

        Ok(())
    }

    fn put_string(
        &self,
        vc_data: &VirtualConsoleData,
        data: &[u16],
        mut count: u32,
        y: u32,
        x: u32,
        fg: u32,
        bg: u32,
    ) -> Result<(), SystemError> {
        // 向上取整
        let width = (vc_data.font.width + 7) / 8;
        let cellsize = width * vc_data.font.height;
        let fb_info = self.fb();
        // 一次能输出的最大字数，避免帧缓冲区溢出
        let max_cnt = (fb_info.current_fb_var().xres * fb_info.current_fb_var().yres) / cellsize;
        let attr = FbConAttr::get_attr(data[0], fb_info.color_depth());

        let mut image = FbImage {
            x: x * vc_data.font.width,
            y: y * vc_data.font.height,
            width: 0,
            height: vc_data.font.height,
            fg,
            bg,
            depth: 1,
            data: Default::default(),
        };

        image.data.resize(cellsize as usize * count as usize, 0);

        while count > 0 {
            let cnt = count.min(max_cnt);

            image.width = vc_data.font.width * cnt;

            self.bit_put_string(vc_data, data, attr, cnt, cellsize, &mut image);

            image.x += cnt * vc_data.font.width;
            count -= cnt;
        }

        Ok(())
    }

    fn fbcon_data(&self) -> SpinLockGuard<super::FrameBufferConsoleData> {
        self.fbcon_data.lock()
    }

    fn cursor(&self, vc_data: &VirtualConsoleData, op: CursorOperation, fg: u32, bg: u32) {
        let mut fbcon_data = self.fbcon_data();
        let fb_info = self.fb();
        let mut cursor = FbCursor::default();
        let charmask = if vc_data.hi_font_mask != 0 {
            0x1ff
        } else {
            0xff
        };

        // 向上取整
        let w = (vc_data.font.width + 7) / 8;
        let y = fbcon_data.display.real_y(vc_data.state.y as u32);

        let c = vc_data.screen_buf[vc_data.pos];
        let attr = FbConAttr::get_attr(c, fb_info.color_depth());
        let char_offset = (c as usize & charmask) * ((w * vc_data.font.height) as usize);

        if fbcon_data.cursor_state.image.data != &vc_data.font.data[char_offset..]
            || fbcon_data.cursor_reset
        {
            fbcon_data.cursor_state.image.data = vc_data.font.data[char_offset..].to_vec();
            cursor.set_mode.insert(FbCursorSetMode::FB_CUR_SETIMAGE);
        }

        if !attr.is_empty() {
            fbcon_data
                .cursor_data
                .resize(w as usize * vc_data.font.height as usize, 0);

            attr.update_attr(
                &mut fbcon_data.cursor_data,
                &vc_data.font.data[char_offset..],
                vc_data,
            );
        }

        if fbcon_data.cursor_state.image.fg != fg
            || fbcon_data.cursor_state.image.bg != bg
            || fbcon_data.cursor_reset
        {
            fbcon_data.cursor_state.image.fg = fg;
            fbcon_data.cursor_state.image.bg = bg;
            cursor.set_mode.insert(FbCursorSetMode::FB_CUR_SETCMAP);
        }

        if fbcon_data.cursor_state.image.x != (vc_data.font.width * vc_data.state.x as u32)
            || fbcon_data.cursor_state.image.y != (vc_data.font.height * y)
            || fbcon_data.cursor_reset
        {
            fbcon_data.cursor_state.image.x = vc_data.font.width * vc_data.state.x as u32;
            fbcon_data.cursor_state.image.y = vc_data.font.height * y;
            cursor.set_mode.insert(FbCursorSetMode::FB_CUR_SETPOS);
        }

        if fbcon_data.cursor_state.image.height != vc_data.font.height
            || fbcon_data.cursor_state.image.width != vc_data.font.width
            || fbcon_data.cursor_reset
        {
            fbcon_data.cursor_state.image.height = vc_data.font.height;
            fbcon_data.cursor_state.image.width = vc_data.font.width;
            cursor.set_mode.insert(FbCursorSetMode::FB_CUR_SETSIZE);
        }

        if fbcon_data.cursor_state.hot_x > 0
            || fbcon_data.cursor_state.hot_y > 0
            || fbcon_data.cursor_reset
        {
            fbcon_data.cursor_state.hot_x = 0;
            cursor.hot_y = 0;
            cursor.set_mode.insert(FbCursorSetMode::FB_CUR_SETHOT);
        }

        if cursor.set_mode.contains(FbCursorSetMode::FB_CUR_SETSIZE)
            || vc_data.cursor_type != fbcon_data.display.cursor_shape
            || fbcon_data.cursor_state.mask.is_empty()
            || fbcon_data.cursor_reset
        {
            fbcon_data.display.cursor_shape = vc_data.cursor_type;
            cursor.set_mode.insert(FbCursorSetMode::FB_CUR_SETSHAPE);

            let cur_height;
            match fbcon_data.display.cursor_shape.cursor_size() {
                VcCursor::CUR_NONE => {
                    cur_height = 0;
                }
                VcCursor::CUR_UNDERLINE => {
                    if vc_data.font.height < 10 {
                        cur_height = 1;
                    } else {
                        cur_height = 2;
                    }
                }
                VcCursor::CUR_LOWER_THIRD => {
                    cur_height = vc_data.font.height / 3;
                }
                VcCursor::CUR_LOWER_HALF => {
                    cur_height = vc_data.font.height >> 1;
                }
                VcCursor::CUR_TWO_THIRDS => {
                    cur_height = (vc_data.font.height << 1) / 3;
                }
                _ => {
                    cur_height = vc_data.font.height;
                }
            }

            // 表示空白部分
            let mut size = (vc_data.font.height - cur_height) * w;
            while size > 0 {
                size -= 1;
                fbcon_data.cursor_state.mask.push(0x00);
            }
            size = cur_height * w;
            // 表示光标显示部分
            while size > 0 {
                size -= 1;
                fbcon_data.cursor_state.mask.push(0xff);
            }
        }

        match op {
            CursorOperation::Erase => {
                fbcon_data.cursor_state.enable = false;
            }
            _ => {
                fbcon_data.cursor_state.enable = !vc_data.cursor_type.contains(VcCursor::CUR_SW);
            }
        }

        if !attr.is_empty() {
            cursor.image.data = fbcon_data.cursor_data.clone();
        } else {
            cursor.image.data = vc_data.font.data
                [char_offset..char_offset + (w as usize * vc_data.font.height as usize)]
                .to_vec();
        }
        cursor.image.fg = fbcon_data.cursor_state.image.fg;
        cursor.image.bg = fbcon_data.cursor_state.image.bg;
        cursor.image.x = fbcon_data.cursor_state.image.x;
        cursor.image.y = fbcon_data.cursor_state.image.y;
        cursor.image.height = fbcon_data.cursor_state.image.height;
        cursor.image.width = fbcon_data.cursor_state.image.width;
        cursor.hot_x = fbcon_data.cursor_state.hot_x;
        cursor.hot_y = fbcon_data.cursor_state.hot_y;
        cursor.mask = fbcon_data.cursor_state.mask.clone();
        cursor.enable = fbcon_data.cursor_state.enable;
        cursor.image.depth = 1;
        cursor.rop = true;

        if fb_info.fb_cursor(&cursor).is_err() {
            let _ = fb_info.soft_cursor(cursor);
        }

        fbcon_data.cursor_reset = false;
    }
}
