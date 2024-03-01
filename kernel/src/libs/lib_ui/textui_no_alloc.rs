use core::{
    intrinsics::unlikely,
    sync::atomic::{AtomicI32, Ordering},
};

use system_error::SystemError;

use crate::driver::{
    serial::serial8250::send_to_default_serial8250_port, video::video_refresh_manager,
};

use super::textui::{
    FontColor, LineId, LineIndex, TextuiCharChromatic, TEXTUI_CHAR_HEIGHT, TEXTUI_CHAR_WIDTH,
};

pub static TRUE_LINE_NUM: AtomicI32 = AtomicI32::new(0);
pub static CHAR_PER_LINE: AtomicI32 = AtomicI32::new(0);
/// textui 未初始化时直接向缓冲区写，不使用虚拟行
pub static NO_ALLOC_OPERATIONS_LINE: AtomicI32 = AtomicI32::new(0);
pub static NO_ALLOC_OPERATIONS_INDEX: AtomicI32 = AtomicI32::new(0);

/// 当系统刚启动的时候，由于内存管理未初始化，而texiui需要动态内存分配。因此只能暂时暴力往屏幕（video_frame_buffer_info）输出信息
pub fn textui_init_no_alloc(video_enabled: bool) {
    if video_enabled {
        let height = video_refresh_manager().device_buffer().height();
        let width = video_refresh_manager().device_buffer().width();
        TRUE_LINE_NUM.store((height / TEXTUI_CHAR_HEIGHT) as i32, Ordering::SeqCst);

        CHAR_PER_LINE.store((width / TEXTUI_CHAR_WIDTH) as i32, Ordering::SeqCst);
    }
}

pub fn no_init_textui_putchar_window(
    character: char,
    frcolor: FontColor,
    bkcolor: FontColor,
    is_put_to_window: bool,
) -> Result<(), SystemError> {
    if NO_ALLOC_OPERATIONS_LINE.load(Ordering::SeqCst) > TRUE_LINE_NUM.load(Ordering::SeqCst) {
        NO_ALLOC_OPERATIONS_LINE.store(0, Ordering::SeqCst);
    }
    //字符'\0'代表ASCII码表中的空字符,表示字符串的结尾
    if unlikely(character == '\0') {
        return Ok(());
    }
    send_to_default_serial8250_port(&[character as u8]);

    // 进行换行操作
    if unlikely(character == '\n') {
        // 换行时还需要输出\r
        send_to_default_serial8250_port(&[b'\r']);
        if is_put_to_window == true {
            NO_ALLOC_OPERATIONS_LINE.fetch_add(1, Ordering::SeqCst);
            NO_ALLOC_OPERATIONS_INDEX.store(0, Ordering::SeqCst);
        }
        return Ok(());
    }
    // 输出制表符
    else if character == '\t' {
        if is_put_to_window == true {
            let char = TextuiCharChromatic::new(Some(' '), frcolor, bkcolor);

            //打印的空格数（注意将每行分成一个个表格，每个表格为8个字符）
            let mut space_to_print = 8 - NO_ALLOC_OPERATIONS_INDEX.load(Ordering::SeqCst) % 8;
            while space_to_print > 0 {
                char.no_init_textui_render_chromatic(
                    LineId::new(NO_ALLOC_OPERATIONS_LINE.load(Ordering::SeqCst)),
                    LineIndex::new(NO_ALLOC_OPERATIONS_INDEX.load(Ordering::SeqCst)),
                );
                NO_ALLOC_OPERATIONS_INDEX.fetch_add(1, Ordering::SeqCst);
                space_to_print -= 1;
            }
            return Ok(());
        }
    }
    // 字符 '\x08' 代表 ASCII 码中的退格字符。它在输出中的作用是将光标向左移动一个位置，并在该位置上输出后续的字符，从而实现字符的删除或替换。
    else if character == '\x08' {
        if is_put_to_window == true {
            NO_ALLOC_OPERATIONS_INDEX.fetch_sub(1, Ordering::SeqCst);
            let op_char = NO_ALLOC_OPERATIONS_INDEX.load(Ordering::SeqCst);
            if op_char >= 0 {
                let char = TextuiCharChromatic::new(Some(' '), frcolor, bkcolor);
                char.no_init_textui_render_chromatic(
                    LineId::new(NO_ALLOC_OPERATIONS_LINE.load(Ordering::SeqCst)),
                    LineIndex::new(NO_ALLOC_OPERATIONS_INDEX.load(Ordering::SeqCst)),
                );

                NO_ALLOC_OPERATIONS_INDEX.fetch_add(1, Ordering::SeqCst);
            }
            // 需要向上缩一行
            if op_char < 0 {
                // 上缩一行
                NO_ALLOC_OPERATIONS_INDEX.store(0, Ordering::SeqCst);
                NO_ALLOC_OPERATIONS_LINE.fetch_sub(1, Ordering::SeqCst);

                if NO_ALLOC_OPERATIONS_LINE.load(Ordering::SeqCst) < 0 {
                    NO_ALLOC_OPERATIONS_LINE.store(0, Ordering::SeqCst);
                }
            }
        }
    } else {
        if is_put_to_window == true {
            // 输出其他字符
            let char = TextuiCharChromatic::new(Some(character), frcolor, bkcolor);

            if NO_ALLOC_OPERATIONS_INDEX.load(Ordering::SeqCst)
                == CHAR_PER_LINE.load(Ordering::SeqCst)
            {
                NO_ALLOC_OPERATIONS_INDEX.store(0, Ordering::SeqCst);
                NO_ALLOC_OPERATIONS_LINE.fetch_add(1, Ordering::SeqCst);
            }
            char.no_init_textui_render_chromatic(
                LineId::new(NO_ALLOC_OPERATIONS_LINE.load(Ordering::SeqCst)),
                LineIndex::new(NO_ALLOC_OPERATIONS_INDEX.load(Ordering::SeqCst)),
            );

            NO_ALLOC_OPERATIONS_INDEX.fetch_add(1, Ordering::SeqCst);
        }
    }

    return Ok(());
}
