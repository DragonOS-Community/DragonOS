#[derive(Debug, Clone, Copy)]
pub struct CgroupCpuState {
    /// Canonical unscaled CFS shares; weight and nice are views of this value.
    shares: u64,
    idle: bool,
    max_quota: Option<u64>,
    max_period_us: u64,
    burst_us: u64,
}

impl Default for CgroupCpuState {
    fn default() -> Self {
        Self {
            shares: 1024,
            idle: false,
            max_quota: None,
            max_period_us: 100_000,
            burst_us: 0,
        }
    }
}

impl CgroupCpuState {
    pub fn weight(&self) -> u64 {
        (self.shares * 100 + 512) / 1024
    }

    pub fn set_weight(&mut self, weight: u64) {
        self.shares = ((weight * 1024 + 50) / 100).clamp(2, 262144);
    }

    pub(crate) fn shares(&self) -> u64 {
        self.shares
    }

    pub(crate) fn nice(&self) -> i32 {
        let mut best = 0;
        let mut delta = u64::MAX;
        for (index, &weight) in crate::sched::LoadWeight::SCHED_PRIO_TO_WEIGHT
            .iter()
            .enumerate()
        {
            let next = weight.abs_diff(self.shares);
            if next >= delta {
                break;
            }
            best = index;
            delta = next;
        }
        best as i32 - 20
    }

    pub(crate) fn set_nice(&mut self, nice: i32) {
        self.shares = crate::sched::LoadWeight::SCHED_PRIO_TO_WEIGHT[(nice + 20) as usize];
    }

    pub(crate) fn idle(&self) -> bool {
        self.idle
    }

    pub(crate) fn set_idle(&mut self, idle: bool) {
        if self.idle != idle {
            self.idle = idle;
            self.shares = if idle { 3 } else { 1024 };
        }
    }

    pub(crate) fn burst(&self) -> u64 {
        self.burst_us
    }

    pub(crate) fn set_burst(&mut self, burst_us: u64) {
        self.burst_us = burst_us;
    }

    pub(crate) fn validate_bandwidth(&self) -> Result<(), system_error::SystemError> {
        // Linux BW_SHIFT=20 limits quota+burst to MAX_BW microseconds.
        const MAX_RUNTIME_US: u64 = (1u64 << 44) - 1;
        if !(1_000..=1_000_000).contains(&self.max_period_us)
            || self.burst_us > u64::MAX / 1_000
            || self.max_quota.is_some_and(|quota| {
                !(1_000..=MAX_RUNTIME_US).contains(&quota)
                    || self.burst_us > quota
                    || quota.saturating_add(self.burst_us) > MAX_RUNTIME_US
            })
        {
            return Err(system_error::SystemError::EINVAL);
        }
        Ok(())
    }

    pub fn max(&self) -> (Option<u64>, u64) {
        (self.max_quota, self.max_period_us)
    }

    pub fn set_max(&mut self, quota: Option<u64>, period_us: u64) {
        self.max_quota = quota;
        self.max_period_us = period_us;
    }
}

#[cfg(test)]
mod cpu_tests {
    use super::CgroupCpuState;

    #[test]
    fn weight_roundtrips_and_nice_is_a_view_of_shares() {
        let mut state = CgroupCpuState::default();
        for weight in 1..=10_000 {
            state.set_weight(weight);
            assert_eq!(state.weight(), weight);
        }
        for nice in -20..=19 {
            state.set_nice(nice);
            assert_eq!(state.nice(), nice);
        }
    }

    #[test]
    fn leaving_idle_resets_shares_not_the_old_weight() {
        let mut state = CgroupCpuState::default();
        state.set_weight(500);
        state.set_idle(true);
        assert_eq!(state.shares(), 3);
        assert_eq!(state.weight(), 0);
        state.set_idle(false);
        assert_eq!(state.weight(), 100);
    }

    #[test]
    fn bandwidth_validates_linux_bounds_without_parent_quota_admission() {
        let mut state = CgroupCpuState::default();
        state.set_max(Some(1000), 1000);
        state.set_burst(1000);
        assert!(state.validate_bandwidth().is_ok());
        state.set_burst(1001);
        assert!(state.validate_bandwidth().is_err());
        state.set_burst(0);
        state.set_max(Some(999), 1000);
        assert!(state.validate_bandwidth().is_err());
        state.set_max(Some((1u64 << 44) - 1), 1000);
        assert!(state.validate_bandwidth().is_ok());
        state.set_burst(1);
        assert!(state.validate_bandwidth().is_err());
        state.set_max(None, 1_000_000);
        assert!(state.validate_bandwidth().is_ok());
        state.set_max(None, 1_000_001);
        assert!(state.validate_bandwidth().is_err());
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
