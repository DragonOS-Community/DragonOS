pub struct RtcTimeT {
    pub second: i32,
    pub minute: i32,
    pub hour: i32,
    pub day: i32,
    pub month: i32,
    pub year: i32,
}

use crate::{
    arch::interrupt::{cli, sti},
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
    TSecond = 0x00,
    TMinute = 0x02,
    THour = 0x04,
    TDay = 0x07,
    TMonth = 0x08,
    TYear = 0x09,
}

///@brief 从主板cmos中获取时间
///
///@param t time结构体
///@return int 成功则为0
pub fn rtc_get_cmos_time(t: &mut RtcTimeT) -> Result<i32,i32> {
    unsafe {
        // 为防止中断请求打断该过程，需要先关中断
        cli();
        //0x0B
        let status_register_b: u8 = read_cmos(0x0B); // 读取状态寄存器B
        let is_24h: bool = if (status_register_b & 0x02) != 0 {
            true
        } else {
            false
        }; // 判断是否启用24小时模式

        let is_binary: bool = if (status_register_b & 0x04) != 0 {
            true
        } else {
            false
        }; // 判断是否为二进制码

        loop {
            t.year = read_cmos(CMOSTimeSelector::TYear as u8) as i32;
            t.month = read_cmos(CMOSTimeSelector::TMonth as u8) as i32;
            t.day = read_cmos(CMOSTimeSelector::TDay as u8) as i32;
            t.hour = read_cmos(CMOSTimeSelector::THour as u8) as i32;
            t.minute = read_cmos(CMOSTimeSelector::TMinute as u8) as i32;
            t.second = read_cmos(CMOSTimeSelector::TSecond as u8) as i32;

            if t.second == read_cmos(CMOSTimeSelector::TSecond as u8) as i32 {
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
    return Ok(0);
}
