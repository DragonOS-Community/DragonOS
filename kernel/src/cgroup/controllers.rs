#[derive(Debug, Clone, Copy)]
pub struct CgroupCpuState {
    weight: u64,
    max_quota: Option<u64>,
    max_period_us: u64,
}

impl Default for CgroupCpuState {
    fn default() -> Self {
        Self {
            weight: 100,
            max_quota: None,
            max_period_us: 100_000,
        }
    }
}

impl CgroupCpuState {
    pub fn weight(&self) -> u64 {
        self.weight
    }

    pub fn set_weight(&mut self, weight: u64) {
        self.weight = weight;
    }

    pub fn max(&self) -> (Option<u64>, u64) {
        (self.max_quota, self.max_period_us)
    }

    pub fn set_max(&mut self, quota: Option<u64>, period_us: u64) {
        self.max_quota = quota;
        self.max_period_us = period_us;
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CgroupMemoryState {
    min: Option<u64>,
    low: Option<u64>,
    high: Option<u64>,
    max: Option<u64>,
    swap_high: Option<u64>,
    swap_max: Option<u64>,
}

impl Default for CgroupMemoryState {
    fn default() -> Self {
        Self {
            min: Some(0),
            low: Some(0),
            high: None,
            max: None,
            swap_high: None,
            swap_max: None,
        }
    }
}

impl CgroupMemoryState {
    pub fn min(&self) -> Option<u64> {
        self.min
    }

    pub fn set_min(&mut self, value: Option<u64>) {
        self.min = value;
    }

    pub fn low(&self) -> Option<u64> {
        self.low
    }

    pub fn set_low(&mut self, value: Option<u64>) {
        self.low = value;
    }

    pub fn high(&self) -> Option<u64> {
        self.high
    }

    pub fn set_high(&mut self, value: Option<u64>) {
        self.high = value;
    }

    pub fn max(&self) -> Option<u64> {
        self.max
    }

    pub fn set_max(&mut self, value: Option<u64>) {
        self.max = value;
    }

    pub fn swap_high(&self) -> Option<u64> {
        self.swap_high
    }

    pub fn set_swap_high(&mut self, value: Option<u64>) {
        self.swap_high = value;
    }

    pub fn swap_max(&self) -> Option<u64> {
        self.swap_max
    }

    pub fn set_swap_max(&mut self, value: Option<u64>) {
        self.swap_max = value;
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CgroupFreezerState {
    freeze_requested: bool,
}

impl CgroupFreezerState {
    pub fn freeze_requested(&self) -> bool {
        self.freeze_requested
    }

    pub fn set_freeze_requested(&mut self, value: bool) {
        self.freeze_requested = value;
    }
}
