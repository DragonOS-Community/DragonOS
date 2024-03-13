use core::sync::atomic::{AtomicUsize, Ordering};

use alloc::vec::Vec;

pub static mut CPU_IQR_TIME: Option<Vec<&'static mut IrqTime>> = None;

#[inline]
pub fn cpu_irq_time(cpu: usize) -> &'static mut IrqTime {
    unsafe { CPU_IQR_TIME.as_mut().unwrap()[cpu] }
}

#[inline]
pub fn irq_time_read(cpu: usize) -> u64 {
    let irqtime = cpu_irq_time(cpu);

    let mut total;

    loop {
        let seq = irqtime.sync.load(Ordering::SeqCst);
        total = irqtime.total;

        if seq == irqtime.sync.load(Ordering::SeqCst) {
            break;
        }
    }

    total
}

#[derive(Debug, Default)]
pub struct IrqTime {
    pub total: u64,
    pub tick_delta: u64,
    pub irq_start_time: u64,
    pub sync: AtomicUsize,
}

impl IrqTime {
    pub fn account_delta(&mut self, delta: u64) {
        // 开始更改时增加序列号
        self.sync.fetch_add(1, Ordering::SeqCst);
        self.total += delta;
        self.tick_delta += delta;
    }
}
