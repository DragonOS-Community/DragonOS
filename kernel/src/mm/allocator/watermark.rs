//! Fixed managed-memory watermarks; changing free inventory never changes them.
use crate::mm::PhysAddr;

#[derive(Clone, Copy, Debug, Default)]
pub struct ZoneWatermarks {
    pub min: usize,
    pub low: usize,
    pub high: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ZonePressureSnapshot {
    pub dma32_deficit: usize,
    pub normal_deficit: usize,
}

impl ZonePressureSnapshot {
    pub fn total_deficit(self) -> usize {
        self.dma32_deficit.saturating_add(self.normal_deficit)
    }

    pub fn needs_reclaim(self, physical: PhysAddr) -> bool {
        if physical.data() <= u32::MAX as usize {
            self.dma32_deficit != 0
        } else {
            self.normal_deficit != 0
        }
    }
}

pub(super) fn calculate(managed: [usize; 2], page_size: usize) -> [ZoneWatermarks; 2] {
    let total = managed[0].saturating_add(managed[1]);
    if total == 0 {
        return [ZoneWatermarks::default(); 2];
    }
    let kib = total.saturating_mul(page_size / 1024);
    let min_kib = integer_sqrt(kib.saturating_mul(16)).clamp(128, 262144);
    let min_pages = min_kib / (page_size / 1024);
    managed.map(|pages| {
        let min = ((min_pages as u128 * pages as u128) / total as u128) as usize;
        let gap = (min / 4).max(pages / 1000);
        ZoneWatermarks {
            min,
            low: min.saturating_add(gap),
            high: min.saturating_add(gap.saturating_mul(2)),
        }
    })
}

fn integer_sqrt(n: usize) -> usize {
    if n < 2 {
        return n;
    }
    let mut lo = 1usize;
    let mut hi = n / 2 + 1;
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if mid <= n / mid {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}
