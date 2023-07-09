// use crate::{
//     include::bindings::bindings::{font_ascii, video_frame_buffer_info},
//     libs::rwlock::RwLock,
// };

// use super::textui::{TextuiCharChromatic, TEXTUI_CHAR_HEIGHT, TEXTUI_CHAR_WIDTH};
// // #[allow(dead_code)]
// // const WHITE: u32 = 0x00ffffff; // 白
// // #[allow(dead_code)]
// // const BLACK: u32 = 0x00000000; // 黑
// // #[allow(dead_code)]
// // const RED: u32 = 0x00ff0000; // 红
// // #[allow(dead_code)]
// // const ORANGE: u32 = 0x00ff8000; // 橙
// // #[allow(dead_code)]
// // const YELLOW: u32 = 0x00ffff00; // 黄
// // #[allow(dead_code)]
// // const GREEN: u32 = 0x0000ff00; // 绿
// // #[allow(dead_code)]
// // const BLUE: u32 = 0x000000ff; // 蓝
// // #[allow(dead_code)]
// // const INDIGO: u32 = 0x0000ffff; // 靛
// // #[allow(dead_code)]
// // const PURPLE: u32 = 0x008000ff; // 紫
// //                                 // 每个字符的宽度和高度（像素）
// // const TEXTUI_CHAR_WIDTH: u32 = 8;
// // const TEXTUI_CHAR_HEIGHT: u32 = 16;

// // #[derive(Copy, Clone, Debug)]
// // pub struct FontColor(u32);
// // #[allow(dead_code)]
// // impl FontColor {
// //     pub const BLUE: FontColor = FontColor::new(0, 0, 0xff);
// //     pub const RED: FontColor = FontColor::new(0xff, 0, 0);
// //     pub const GREEN: FontColor = FontColor::new(0, 0xff, 0);
// //     pub const WHITE: FontColor = FontColor::new(0xff, 0xff, 0xff);
// //     pub const BLACK: FontColor = FontColor::new(0, 0, 0);

// //     pub const fn new(r: u8, g: u8, b: u8) -> Self {
// //         let val = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
// //         return FontColor(val & 0x00ffffff);
// //     }
// //     pub fn to_u32(&self) -> u32 {
// //         self.0.clone()
// //     }
// // }

// // impl From<u32> for FontColor {
// //     fn from(value: u32) -> Self {
// //         return Self(value & 0x00ffffff);
// //     }
// // }

// // pub static BUF_VADDR: RwLock<usize> = RwLock::new(0);
// // pub static BUF_WIDTH: RwLock<u32> = RwLock::new(0);
// ///  渲染彩色字符（往帧缓冲区对应位置修改像素点数据，之后会一一对应在屏幕上指定位置打印字符）
// ///  `actual_line`: 真实行的行号
// ///  `index`: 列号
// ///  `character`: 要渲染的字符
// pub fn textui_render_chromatic(actual_line: u16, index: u16, character: &TextuiCharChromatic) {
//     // 找到要渲染的字符的像素点数据
//     let font = unsafe { font_ascii }[character.c as usize];

//     let fr_color = character.frcolor.to_u32();
//     let bk_color = character.bkcolor.to_u32();
//     //   x 左上角列像素点位置
//     //   y 左上角行像素点位置
//     let x = index * TEXTUI_CHAR_WIDTH as u16;
//     let y = actual_line * TEXTUI_CHAR_HEIGHT as u16;

//     let mut testbit: u32; //用来测试特定行的某列是背景还是字体本身

//     // 在缓冲区画出一个字体，每个字体有TEXTUI_CHAR_HEIGHT行，TEXTUI_CHAR_WIDTH列个像素点
//     for i in 0..TEXTUI_CHAR_HEIGHT {
//         //计算出帧缓冲区每一行打印的起始位置的地址（起始位置+（y+i）*缓冲区的宽度+x）

//         let mut addr: *mut u32 = (*BUF_VADDR.read() as u32
//             + *BUF_WIDTH.read() * 4 * (y as u32 + i)
//             + 4 * x as u32) as *mut u32;

//         testbit = 1 << (TEXTUI_CHAR_WIDTH + 1);
//         for _j in 0..TEXTUI_CHAR_WIDTH {
//             //从左往右逐个测试相应位
//             testbit >>= 1;
//             if font[i as usize] & testbit as u8 != 0 {
//                 unsafe { *addr = fr_color as u32 }; // 字，显示前景色
//             } else {
//                 unsafe { *addr = bk_color as u32 }; // 背景色
//             }

//             unsafe {
//                 addr = (addr.offset(1)) as *mut u32;
//             }
//         }
//     }
// }
// // 将窗口的输出缓冲区重置
// // fb:缓冲区起始地址
// // num:要重置的像素点数量
// pub fn renew_buf(fb: usize, num: u32) {
//     let mut addr: *mut u32 = fb as *mut u32;
//     for _i in 0..num {
//         unsafe { *addr = 0 };
//         unsafe {
//             addr = (addr.offset(1)) as *mut u32;
//         }
//     }
// }
// pub fn no_init_textui_render_chromatic(
//     actual_line: u32,
//     index: u32,
//     character: &TextuiCharChromatic,
// ) {
//     // 找到要渲染的字符的像素点数据
//     let font = unsafe { font_ascii }[character.c as usize];
//     // 找到输入缓冲区的起始地址位置
//     let fb = unsafe { video_frame_buffer_info.vaddr };
//     //   x 左上角列像素点位置
//     //   y 左上角行像素点位置
//     //   frcolor 字体颜色
//     //   bkcolor 背景颜色
//     let fr_color = character.frcolor.to_u32();
//     let bk_color = character.bkcolor.to_u32();
//     let x = index * TEXTUI_CHAR_WIDTH;
//     let y = actual_line * TEXTUI_CHAR_HEIGHT;

//     let mut testbit: u32; //用来测试特定行的某列是背景还是字体本身

//     // 在缓冲区画出一个字体，每个字体有TEXTUI_CHAR_HEIGHT行，TEXTUI_CHAR_WIDTH列个像素点
//     for i in 0..TEXTUI_CHAR_HEIGHT {
//         //计算出帧缓冲区每一行打印的起始位置的地址（起始位置+（y+i）*缓冲区的宽度+x）

//         let mut addr: *mut u32 = (fb as u32
//             + unsafe { video_frame_buffer_info.width } * 4 * (y as u32 + i)
//             + 4 * x as u32) as *mut u32;

//         testbit = 1 << (TEXTUI_CHAR_WIDTH + 1);
//         for _j in 0..TEXTUI_CHAR_WIDTH {
//             //从左往右逐个测试相应位
//             testbit >>= 1;
//             if (font[i as usize] & testbit as u8) != 0 {
//                 unsafe { *addr = fr_color as u32 }; // 字，显示前景色
//             } else {
//                 unsafe { *addr = bk_color as u32 }; // 背景色
//             }

//             unsafe {
//                 addr = (addr.offset(1)) as *mut u32;
//             }
//         }
//     }
// }
