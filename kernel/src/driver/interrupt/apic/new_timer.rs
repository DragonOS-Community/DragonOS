use x86::time::rdtsc;
// use x86::cpuid::CpuId
use x86::msr::{rdmsr, wrmsr};
use ::core::arch::asm;
use crate::kerror;
use crate::driver::interrupt::apic::xapic::XApic;
use crate::exception::InterruptArch;
use crate::arch::CurrentIrqArch;
pub use drop;

// pub enum ApicTimerMode {
//     OneShot {
//         enabled: bool,
//     },
//     Periodic {
//         enabled: bool,
//     },
//     TSCDeadline {
//         enabled: bool,
//     },
// }

// impl ApicTimerMode {
//     pub fn start_timer(&self, duration: u64) {
//         match self {
//             ApicTimerMode::OneShot { enabled } => {
                
//             }
//             ApicTimerMode::Periodic { enabled } => {
                
//             }
//             ApicTimerMode::TSCDeadline { enabled } => {
                
               
//             }
//         }
//     }
// }

// match TODO
#[inline(always)]
pub unsafe fn cpuid(eax: &mut u32, ebx: &mut u32, ecx: &mut u32, edx: &mut u32) {
    /* EBX is used internally by LLVM */
    asm!(
        "   xchg rdi, rbx
            cpuid
            xchg rdi, rbx
        ",
        inout("eax") * eax,
        inout("ecx") * ecx,
        out("rdi") * ebx,
        out("edx") * edx
    );
}
pub struct LocalApicTimer {
    is_deadline_mode_enabled: bool,
    frequency: usize,
    reload_value: u64,
    is_interrupt_enabled: bool,

    is_periodic_mode_enabled: bool,
    periodic_interval: u64,

    is_oneshot_mode_enabled: bool,
    oneshot_interval: u64,
    oneshot_triggered: bool,
}

impl LocalApicTimer {
    const TSC_DEADLINE_MSR: u32 = 0x6E0;

    /// IoApicManager 初值为0或false
    pub const fn new() -> Self {
        Self {
            is_deadline_mode_enabled: false,
            frequency: 0,
            reload_value: 0,
            is_interrupt_enabled: false,
            is_periodic_mode_enabled: false,
            periodic_interval: 0,
            is_oneshot_mode_enabled: false,
            oneshot_interval: 0,
            oneshot_triggered: false,
        }
    }

    /// Init this manager.
    ///
    /// At this time, it does nothing.
    pub fn init(&mut self) {}

    /// 检查是否支持TSC-Deadline
    ///
    /// 此函数调用cpuid，请避免多次调用此函数。
    /// 如果支持TSC-Deadline模式，则除非TSC为常数，否则不会启用该模式。
    pub fn is_deadline_mode_supported(&self) -> bool {
        let mut eax = 1u32;
        let mut ebx = 0u32;
        let mut ecx = 0u32;
        let mut edx = 0u32;
        unsafe { cpuid(&mut eax, &mut ebx, &mut ecx, &mut edx) };
        ecx & (1 << 24) != 0
    }

    fn calculate_next_reload_value(&self, ms: u64) -> (u64, bool) {
        self.reload_value
            .overflowing_add((self.frequency as u64 / 1000) * ms as u64)
    }

    /// 重置下一次中断的计时器截止时间 
    ///
    /// 此函数是从中断处理程序调用的。
    /// Set [`Self::reload_value`] += TIMER_INTERVAL
    /// 该函数将会返回false
    /// 当[`Self::is_deadline_mode_enabled`] 为true
    /// 重置reload_value大于当前tsc
    fn update_deadline_and_compare_with_current_tsc(&mut self, ms: u64) -> bool {
        if self.is_deadline_mode_enabled {
            unsafe {
                let irq_guard = CurrentIrqArch::save_and_disable_irq();
                let (reload_value, overflowed) = self.calculate_next_reload_value(ms);
                let old_value = self.reload_value;
                self.reload_value = reload_value;
                drop(irq_guard);
            let current = unsafe { rdtsc() };
            if overflowed {
                (old_value > current) && (self.reload_value <= current)
            } else {
                self.reload_value <= current
            }
        }
        } else {
            false
        }
    }

    /// Set [`Self::reload_value`] to TSC_DEADLINE_MSR.
    ///
    /// 检查TSC-Deadline是否已启用，并设置新的ddl（毫秒）。
    /// 如果未启用该模式，则返回false
    fn write_deadline(&self) -> bool {
        if !self.is_deadline_mode_enabled || self.frequency == 0 {
            return false;
        }
        unsafe { wrmsr(Self::TSC_DEADLINE_MSR, self.reload_value) };
        return true;
    }

    /// 启用TSC-Deadline模式。
    ///
    /// 检查三点：是否支持TSC截止日期模式？，
    /// 它是否能够得到TSC的频率，以及它是否是不变的。
    /// 之后，此函数设置寄存器以启用它。
    /// 如果启用，则当前值将永久为零。
    pub fn enable_deadline_mode(
        &mut self,
        vector: u16,
        local_apic_manager: &mut XApic,
    ) -> bool {
        if !self.is_deadline_mode_supported() {
            return false;
        }
        let is_invariant_tsc = unsafe {
            let mut eax = 0x80000007u32;
            let mut ebx = 0;
            let mut edx = 0;
            let mut ecx = 0;
            cpuid(&mut eax, &mut ebx, &mut ecx, &mut edx);
            (edx & (1 << 8)) != 0
        };
        if !is_invariant_tsc {
            kerror!("TSC is not invariant.");
            return false;
        }

        unsafe { 
            let irq_guard = CurrentIrqArch::save_and_disable_irq();

            self.frequency = (( rdmsr(0xce) as usize >> 8) & 0xff) * 100 * 1000000;
            /* Frequency = MSRS(0xCE)[15:8] * 100MHz
             * 2.12 MSRS IN THE 3RD GENERATION INTEL(R) CORE(TM) PROCESSOR FAMILY
             * (BASED ON INTEL® MICROARCHITECTURE CODE NAME IVY BRIDGE) Intel SDM Vol.4 2-198 */
             /* TODO */
            if self.frequency == 0 {
                drop(irq_guard);
                kerror!("Cannot get the frequency of TSC.");
                return false;
            }

            local_apic_manager.write(
                //LvtTimer,
                0x32,
                (0b101 << 16) | (vector as u32),
        );
        self.is_deadline_mode_enabled = true;
        drop(irq_guard);
        return true;
        }
    }

    /// 启用Periodic模式
    ///
    /// interval_ms 表示触发中断的间隔时间
    /// local_apic 是 XApic 的实例，用于读写 Local APIC 寄存器
    pub fn enable_periodic_mode(&mut self, interval_ms: u64, local_apic: &mut XApic, vector: u16) -> bool {
        if self.is_periodic_mode_enabled || self.frequency == 0 {
            return false;
        }
        unsafe{
        let irq_guard = CurrentIrqArch::save_and_disable_irq();

        local_apic.write(0x3e, 0b1011);
        // TimerDivide 0x3e
        local_apic.write(0x32, (0b001 << 16) | vector as u32); 
        /*Masked*/
        // LvtTimer 0x32

        // 设置Periodic模式的重载值
        self.periodic_interval = (self.frequency / 1000) as u64 * interval_ms;
        local_apic.write(
            //TimerInitialCount,
            0x38,
            self.periodic_interval as u32,
        );

        self.is_periodic_mode_enabled = true;

        drop(irq_guard);
        true
    }
    }

    /// 设置oneshot模式
    ///
    /// interval_ms 表示触发中断的间隔时间
    /// local_apic 是 XApic 的实例，用于读写 Local APIC 寄存器
    pub fn enable_oneshot_mode(&mut self, interval_ms: u64, local_apic: &mut XApic, vector: u32) -> bool {
        if self.is_oneshot_mode_enabled || self.frequency == 0 {
            return false;
        }
        unsafe{
        let irq_guard = CurrentIrqArch::save_and_disable_irq();
        
        local_apic.write(0x3e, 0b1011);
        // TimerDivide 0x3e
        local_apic.write(0x32, (0b001 << 16) | vector as u32); /*Masked*/
        // LvtTimer 0x32

        // 设置单次触发模式的重载值
        self.oneshot_interval = (self.frequency / 1000) as u64 * interval_ms;
        self.oneshot_triggered = false;
        self.is_oneshot_mode_enabled = true;

        drop(irq_guard);
        true
        }
    }

      /// 启动定时器中断
    pub fn start_interrupt_oneshot(&mut self, local_apic: &mut XApic) -> bool {
        unsafe{
        let irq_guard = CurrentIrqArch::save_and_disable_irq();
        if self.is_interrupt_enabled || self.frequency == 0 {
            drop(irq_guard);
            return false;
        }
    

        if self.is_oneshot_mode_enabled {
            let mut lvt = local_apic.read(0x32);
            // LvtTimer 0x32
            lvt &= !(0b111 << 16);
            lvt |= 0b01 << 17;
            local_apic.write(0x32, lvt);
            // LvtTimer 0x32
            
            // 设置单次触发模式的重载值
            local_apic.write(
                //TimerInitialCount,
                0x38,
                self.oneshot_interval as u32,
            );

            self.oneshot_triggered = false;
        } 

        self.is_interrupt_enabled = true;
        drop(irq_guard);
        true
    }
    }

    
    /// 设置定时器的中断。
    ///
    /// 此功能调用其他计时器来计算计时器的频率。
    /// 如果已经设置了中断，则返回false。
    ///
    /// *vector：用于设置计时器的IDT矢量表的索引
    /// *local_apic:XApic，用于读取/写入本地apic。
    /// *timer：满足timer特性的结构体。它必须提供busy_wait_ms。
    ///
    /// 这不会设置中断管理器，必须手动设置。
    /// 之后要开始中断，执行[`Self:：start_interrupt`]。
    pub fn start_interrupt_periodic<T: Timer>(
        &mut self,
        vector: u16,
        local_apic: &mut XApic,
        timer: &T,
    ) -> bool {
        if self.frequency != 0 {
            return false;
        }
        unsafe{
        let irq_guard = CurrentIrqArch::save_and_disable_irq();

        local_apic.write(0x3e, 0b1011); //TimerDivide 0x3e
        local_apic.write(0x32, (0b001 << 16) | vector as u32); /*LvtTimer Masked*/
        self.reload_value = u32::MAX as u64;
        local_apic.write(0x38, u32::MAX); //TimerInitialCount 0x38
        timer.busy_wait_ms(50);
        let end = local_apic.read(0x39); //TimerCurrentCount 0x39
        let difference = self.get_difference(u32::MAX as usize, end as usize);
        self.frequency = difference * 20;
        drop(irq_guard);
        return true;
        }
    }

    /// 将寄存器设置为开始中断。
    ///
    /// 在调用之前确保已设置中断。
    /// 目前，该函数将1000ms设置为间隔，是可变的
    pub fn start_interrupt_deadline(&mut self, local_apic: &mut XApic) -> bool {
        unsafe{
        let irq_guard = CurrentIrqArch::save_and_disable_irq();
        if self.is_interrupt_enabled || self.frequency == 0 {
            drop(irq_guard);
            return false;
        }

        if self.is_deadline_mode_enabled {
            let mut lvt = local_apic.read(0x32);// LvtTimer寄存器0x32
            lvt &= !(0b1 << 16);
            local_apic.write(0x32, lvt); //LvtTimer寄存器0x32
            self.reload_value = unsafe { rdtsc() };
            self.reload_value = self
                .calculate_next_reload_value(10) // TIMER_INTERVAL_MS值设置为10
                .0;
            self.write_deadline();
        } else {
            let mut lvt = local_apic.read(0x32);// LvtTimer
            lvt &= !(0b111 << 16);
            lvt |= 0b01 << 17;
            local_apic.write(0x32, lvt);// LvtTimer
            self.set_interval(10, local_apic); // TIMER_INTERVAL_MS
        }
        self.is_interrupt_enabled = true;
        drop(irq_guard);
        return true;
    }
    }

    ///设置间隔模式的重载值
    ///
    ///设置[`Self:：reload_value`]并设置到本地APIC寄存器中。
    ///如果启用了TSCDeadline模式，则此操作将无效。
    ///此函数假定[`Self:：lock`]已锁定。
    fn set_interval(&mut self, interval_ms: u64, local_apic: &mut XApic) -> bool {
        if self.is_deadline_mode_enabled || self.frequency == 0 {
            return false;
        }
        self.reload_value = (self.frequency / 1000) as u64 * interval_ms;
        unsafe{
        local_apic.write(
            0x38,
            self.reload_value as u32,
        );
    }
        return true;
    }
}

impl LocalApicTimer {
    fn get_count(&self) -> usize {
        if self.is_periodic_mode_enabled {
            let current_count = false;
            let remaining_count = self.get_difference(self.periodic_interval as usize, current_count as usize);
            remaining_count
        } else if self.is_deadline_mode_enabled {
            unsafe { rdtsc() as usize }
        } 
        else{
            0
        }
    }

    fn get_frequency_hz(&self) -> usize {
        self.frequency
    }

    fn is_count_up_timer(&self) -> bool {
        true
    }

    fn get_difference(&self, earlier: usize, later: usize) -> usize {
        assert_eq!(self.is_deadline_mode_enabled, false);
        if earlier <= later {
            earlier + (self.reload_value as usize - later)
        } else {
            earlier - later
        }
    }

    fn get_ending_count_value(&self, _start: usize, _difference: usize) -> usize {
        unimplemented!()
    }

    fn get_max_counter_value(&self) -> usize {
        u32::MAX as usize
    }
}

// 定义了一个通用的计时器接口，用于进行定时操作，包括忙等待和等待计数器达到特定值
pub trait Timer {
    fn get_count(&self) -> usize;
    fn get_frequency_hz(&self) -> usize;
    fn is_count_up_timer(&self) -> bool;
    fn get_difference(&self, earlier: usize, later: usize) -> usize;
    fn get_ending_count_value(&self, start: usize, difference: usize) -> usize;
    fn get_max_counter_value(&self) -> usize;

    #[inline(always)]
    fn busy_wait_ms(&self, ms: usize) {
        let start = self.get_count();
        let difference = self.get_frequency_hz() * ms / 1000;
        if difference > self.get_max_counter_value() {
            panic!("Cannot count more than max_counter_value");
        }
        let end = self.get_ending_count_value(start, difference);
        self.wait_until(start, end);
    }

    #[inline(always)]
    fn busy_wait_us(&self, us: usize) {
        let start = self.get_count();
        let difference = self.get_frequency_hz() * us / 1000000;
        if difference > self.get_max_counter_value() {
            panic!("Cannot count more than max_counter_value");
        } else if difference == 0 {
            panic!("Cannot count less than the resolution");
        }
        let end = self.get_ending_count_value(start, difference);
        self.wait_until(start, end);
    }

    #[inline(always)]
    fn wait_until(&self, start_counter_value: usize, end_counter_value: usize) {
        use core::hint::spin_loop;
        if self.is_count_up_timer() {
            if start_counter_value > end_counter_value {
                while self.get_count() >= start_counter_value {
                    spin_loop();
                }
            }
            while self.get_count() < end_counter_value {
                spin_loop();
            }
        } else {
            if start_counter_value < end_counter_value {
                while self.get_count() <= start_counter_value {
                    spin_loop();
                }
            }
            while self.get_count() > end_counter_value {
                spin_loop();
            }
        }
    }
}