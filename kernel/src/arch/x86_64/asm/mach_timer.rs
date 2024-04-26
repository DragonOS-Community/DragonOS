
use crate::arch::{x86_64::driver::tsc::PIT_TICK_RATE, io::PortIOArch, CurrentPortIOArch};

// 参考：https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/include/asm/mach_timer.h?fi=mach_prepare_counter

pub const CALIBRATE_TIME_MSEC:u64 = 30;
pub const CALIBRATE_LATCH:u64 = (PIT_TICK_RATE*CALIBRATE_TIME_MSEC + 1000/2)/1000;

#[inline(always)]
#[allow(dead_code)]
pub fn mach_prepare_counter() {
    unsafe {
        // 将Gate位设置为高电平，从而禁用扬声器
        CurrentPortIOArch::out8(0x61,(CurrentPortIOArch::in8(0x61)& !0x02)|0x01);

        // 针对计数器/定时器控制器的通道2进行配置，设置为模式0，二进制计数
        CurrentPortIOArch::out8(0x43,0xb0);
        CurrentPortIOArch::out8(0x42,(CALIBRATE_LATCH & 0xff) as u8);
        CurrentPortIOArch::out8(0x42,(CALIBRATE_LATCH >> 8) as u8);
    }
}

#[inline(always)]
#[allow(dead_code)]
pub fn mach_countup(count: &mut u32) {
    let mut tmp:u32 = 0;
    loop {
        tmp += 1;
        if (0x61 & 0x20) != 0 {
            break;
        }
    }
    *count = tmp;
}