use crate::include::bindings::bindings::font_ascii;

use super::textui::{TextuiCharChromatic, TEXTUIFRAMEWORK};

const WHITE: u32 = 0x00ffffff; // 白
const BLACK: u32 = 0x00000000; // 黑
const RED: u32 = 0x00ff0000; // 红
const ORANGE: u32 = 0x00ff8000; // 橙
const YELLOW: u32 = 0x00ffff00; // 黄
const GREEN: u32 = 0x0000ff00; // 绿
const BLUE: u32 = 0x000000ff; // 蓝
const INDIGO: u32 = 0x0000ffff; // 靛
const PURPLE: u32 = 0x008000ff; // 紫
                                // 每个字符的宽度和高度（像素）
const TEXTUI_CHAR_WIDTH: u32 = 8;
const TEXTUI_CHAR_HEIGHT: u32 = 16;
/// ## 渲染彩色字符（往帧缓冲区对应位置修改像素点数据，之后会一一对应在屏幕上指定位置打印字符）
///
/// * `actual_line`: 真实行的行号
/// * `index`: 列号
/// * `character`: 要渲染的字符

pub fn textui_render_chromatic(actual_line: u16, index: u16, character: &TextuiCharChromatic) {
    //   font 字符的bitmap
    // 找到要渲染的字符的像素点数据
    let font = unsafe { font_ascii }[character.c as usize];
    // 找到输入缓冲区的起始地址位置
    let fb = TEXTUIFRAMEWORK.0.lock().metadata.buf.vaddr;
    //   x 左上角列像素点位置
    //   y 左上角行像素点位置
    //   frcolor 字体颜色
    //   bkcolor 背景颜色
    let fr_color = character.frcolor & 0x00ffffff;
    let bk_color = character.bkcolor & 0x00ffffff;
    let x = index * TEXTUI_CHAR_WIDTH as u16;
    let y = actual_line * TEXTUI_CHAR_HEIGHT as u16;

    let mut testbit: u32; //用来测试特定行的某列是背景还是字体本身

    // 在缓冲区画出一个字体，每个字体有TEXTUI_CHAR_HEIGHT行，TEXTUI_CHAR_WIDTH列个像素点
    for i in 0..TEXTUI_CHAR_HEIGHT {
        //计算出帧缓冲区每一行打印的起始位置的地址（起始位置+（y+i）*缓冲区的宽度+x）

        let mut addr: *mut u32 = (fb as u32
            + TEXTUIFRAMEWORK.0.lock().metadata.buf.width * (y as u32 + i)
            + x as u32) as *mut u32;

        testbit = 1 << (TEXTUI_CHAR_WIDTH + 1);
        for _j in 0..TEXTUI_CHAR_WIDTH {
            //从左往右逐个测试相应位
            testbit >>= 1;
            if font[i as usize] & testbit as u8 != 0 {
                unsafe { *addr = fr_color as u32 }; // 字，显示前景色
            } else {
                unsafe { *addr = bk_color as u32 }; // 背景色
            }

            unsafe {
                addr = (addr.offset(1)) as *mut u32;
            }
        }
    }
}
