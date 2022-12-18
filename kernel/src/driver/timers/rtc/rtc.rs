pub struct rtc_time_t {
    pub second: u32,
    pub minute: u32,
    pub hour: u32,
    pub day: u32,
    pub month: u32,
    pub year: u32,
}

use crate::{
    arch::x86_64::interrupt::{cli, sti},
    include::bindings::bindings::{io_in8, io_out8},
};

///置位0x70的第7位，禁止不可屏蔽中断
#[inline]
fn read_cmos(addr: u8) -> u8 {
    unsafe {
        io_out8(0x70, 0x80 | addr);
        return io_in8(0x71);
    }
}

enum CMOSTimeSelector {
    T_SECOND = 0x00,
    T_MINUTE = 0x02,
    T_HOUR = 0x04,
    T_DAY = 0x07,
    T_MONTH = 0x08,
    T_YEAR = 0x09,
}

///@brief 从主板cmos中获取时间
///
///@param t time结构体
///@return int 成功则为0
pub fn rtc_get_cmos_time(t: &mut rtc_time_t) -> i32 {
    unsafe {
        // 为防止中断请求打断该过程，需要先关中断
        cli();
        //0x0B
        let status_register_B: u8 = read_cmos(0x0B); // 读取状态寄存器B
        let is_24h: bool = if (status_register_B & 0x02) != 0 {
            true
        } else {
            false
        }; // 判断是否启用24小时模式

        let is_binary: bool = if (status_register_B & 0x04) != 0 {
            true
        } else {
            false
        }; // 判断是否为二进制码

        loop {
            t.year = read_cmos(CMOSTimeSelector::T_YEAR as u8) as u32;
            t.month = read_cmos(CMOSTimeSelector::T_MONTH as u8) as u32;
            t.day = read_cmos(CMOSTimeSelector::T_DAY as u8) as u32;
            t.hour = read_cmos(CMOSTimeSelector::T_HOUR as u8) as u32;
            t.minute = read_cmos(CMOSTimeSelector::T_MINUTE as u8) as u32;
            t.second = read_cmos(CMOSTimeSelector::T_SECOND as u8) as u32;

            if t.second == read_cmos(CMOSTimeSelector::T_SECOND as u8) as u32 {
                break;
            } // 若读取时间过程中时间发生跳变则重新读取
        }

        io_out8(0x70, 0x00);

        if !is_binary
        // 把BCD转为二进制
        {
            t.second = (t.second & 0xf) + (t.second >> 4) * 10;
            t.minute = (t.minute & 0xf) + (t.minute >> 4) * 10;
            t.hour = ((t.hour & 0xf) + ((t.hour & 0x70) >> 4) * 10) | (t.hour & 0x80);
            t.day = (t.day & 0xf) + ((t.day / 16) * 10);
            t.month = (t.month & 0xf) + (t.month >> 4) * 10;
            t.year = (t.year & 0xf) + (t.year >> 4) * 10;
        }
        t.year += 2000;

        if (!is_24h) && (t.hour & 0x80) != 0 {
            t.hour = ((t.hour & 0x7f) + 12) % 24;
        } // 将十二小时制转为24小时

        sti();
    }
    return 0;
}
