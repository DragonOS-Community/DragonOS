use crate::{
    arch::interrupt::{cli, sti},
    include::bindings::bindings::{io_in8, io_out8}, syscall::SystemError,
};

pub struct RtcTime {
    pub second: i32,
    pub minute: i32,
    pub hour: i32,
    pub day: i32,
    pub month: i32,
    pub year: i32,
}

impl Default for RtcTime {
    fn default() -> Self {
        Self {
            second: (0),
            minute: (0),
            hour: (0),
            day: (0),
            month: (0),
            year: (0),
        }
    }
}

impl RtcTime {
    ///@brief 从主板cmos中获取时间
    ///
    ///@param self time结构体
    ///@return int 成功则为0
    pub fn get(&mut self) -> Result<i32, SystemError> {
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
            self.year = read_cmos(CMOSTimeSelector::Year as u8) as i32;
            self.month = read_cmos(CMOSTimeSelector::Month as u8) as i32;
            self.day = read_cmos(CMOSTimeSelector::Day as u8) as i32;
            self.hour = read_cmos(CMOSTimeSelector::Hour as u8) as i32;
            self.minute = read_cmos(CMOSTimeSelector::Minute as u8) as i32;
            self.second = read_cmos(CMOSTimeSelector::Second as u8) as i32;

            if self.second == read_cmos(CMOSTimeSelector::Second as u8) as i32 {
                break;
            } // 若读取时间过程中时间发生跳变则重新读取
        }

        unsafe {
            io_out8(0x70, 0x00);
        }

        if !is_binary
        // 把BCD转为二进制
        {
            self.second = (self.second & 0xf) + (self.second >> 4) * 10;
            self.minute = (self.minute & 0xf) + (self.minute >> 4) * 10;
            self.hour = ((self.hour & 0xf) + ((self.hour & 0x70) >> 4) * 10) | (self.hour & 0x80);
            self.day = (self.day & 0xf) + ((self.day / 16) * 10);
            self.month = (self.month & 0xf) + (self.month >> 4) * 10;
            self.year = (self.year & 0xf) + (self.year >> 4) * 10;
        }
        self.year += 2000;

        if (!is_24h) && (self.hour & 0x80) != 0 {
            self.hour = ((self.hour & 0x7f) + 12) % 24;
        } // 将十二小时制转为24小时

        sti();

        return Ok(0);
    }
}

///置位0x70的第7位，禁止不可屏蔽中断
#[inline]
fn read_cmos(addr: u8) -> u8 {
    unsafe {
        io_out8(0x70, 0x80 | addr);
        return io_in8(0x71);
    }
}

/// used in the form of u8
#[repr(u8)]
enum CMOSTimeSelector {
    Second = 0x00,
    Minute = 0x02,
    Hour = 0x04,
    Day = 0x07,
    Month = 0x08,
    Year = 0x09,
}
