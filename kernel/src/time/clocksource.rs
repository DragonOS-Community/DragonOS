use core::{
    fmt::Debug,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use alloc::{
    collections::LinkedList,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use log::{debug, info, warn};
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    arch::CurrentIrqArch,
    debug::klog::loglevel::LogLevel,
    exception::InterruptArch,
    init::initcall::INITCALL_LATE,
    libs::{printk::Logger, spinlock::SpinLock},
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessControlBlock, ProcessManager,
    },
    sched::{schedule, SchedMode},
};

use super::{timekeeping, timer::clock, NSEC_PER_SEC, NSEC_PER_USEC};

kernel_cmdline_param_kv!(CLOCKSOURCE_PARAM, clocksource, "");

lazy_static! {
    /// linked list with the registered clocksources
    pub static ref CLOCKSOURCE_LIST: SpinLock<LinkedList<Arc<dyn Clocksource>>> =
        SpinLock::new(LinkedList::new());
    /// 被监视中的时钟源
    pub static ref WATCHDOG_LIST: SpinLock<LinkedList<Arc<dyn Clocksource>>> =
        SpinLock::new(LinkedList::new());

    pub static ref CLOCKSOURCE_WATCHDOG:SpinLock<ClocksourceWatchdog>  = SpinLock::new(ClocksourceWatchdog::new());

    pub static ref OVERRIDE_NAME: SpinLock<String> = SpinLock::new(String::from(""));


}

pub fn handle_clocksource_cmdline_param() {
    if !CLOCKSOURCE_PARAM.was_supplied() {
        return;
    }

    let Some(name) = CLOCKSOURCE_PARAM.value_str() else {
        return;
    };
    if name.is_empty() {
        return;
    }

    *OVERRIDE_NAME.lock() = String::from(name);
    debug!("clocksource: boot override set to {}", name);
}

static mut WATCHDOG_KTHREAD: Option<Arc<ProcessControlBlock>> = None;

/// 正在被使用时钟源
static CLOCKSOURCE_CONTROL_LOCK: SpinLock<()> = SpinLock::new(());
/// 是否完成加载
pub static FINISHED_BOOTING: AtomicBool = AtomicBool::new(false);
static WATCHDOG_MAX_INTERVAL_NS_SEEN: AtomicU64 = AtomicU64::new(WATCHDOG_INTERVAL_MAX_NS);

/// Interval: 0.5sec Threshold: 0.0625s
/// 系统节拍率
pub const HZ: u64 = 250;
// 参考：https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/time/clocksource.c#101
/// watchdog检查间隔
pub const WATCHDOG_INTERVAL: u64 = HZ >> 1;
pub const WATCHDOG_INTERVAL_MAX_NS: u64 = (2 * WATCHDOG_INTERVAL) * (NSEC_PER_SEC as u64 / HZ);
// 参考：https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/time/clocksource.c#108
/// 最大能接受的误差大小
pub const WATCHDOG_THRESHOLD: u32 = NSEC_PER_SEC >> 4;

pub const MAX_SKEW_USEC: u64 = 125 * WATCHDOG_INTERVAL / HZ;
pub const WATCHDOG_MAX_SKEW: u32 = MAX_SKEW_USEC as u32 * NSEC_PER_USEC;

#[inline]
fn watchdog_next_expiry(now: u64) -> u64 {
    now.saturating_add(WATCHDOG_INTERVAL)
}

#[inline]
fn watchdog_has_sources(
    reference: &Option<Arc<dyn Clocksource>>,
    list: &LinkedList<Arc<dyn Clocksource>>,
) -> bool {
    reference.as_ref().is_some_and(|watchdog| {
        list.iter()
            .any(|candidate| !Arc::ptr_eq(candidate, watchdog))
    })
}

fn should_report_long_watchdog_interval(interval: u64) -> bool {
    loop {
        let max_interval = WATCHDOG_MAX_INTERVAL_NS_SEEN.load(Ordering::Relaxed);
        if interval <= max_interval.saturating_mul(2) {
            return false;
        }

        if WATCHDOG_MAX_INTERVAL_NS_SEEN
            .compare_exchange(max_interval, interval, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return true;
        }
    }
}

// 时钟周期数
#[derive(Debug, Clone, Copy)]
pub struct CycleNum(u64);

#[allow(dead_code)]
impl CycleNum {
    #[inline(always)]
    pub const fn new(cycle: u64) -> Self {
        Self(cycle)
    }
    #[inline(always)]
    pub const fn data(&self) -> u64 {
        self.0
    }
    #[inline(always)]
    #[allow(dead_code)]
    pub fn add(&self, other: CycleNum) -> CycleNum {
        CycleNum(self.data() + other.data())
    }
    #[inline(always)]
    pub fn div(&self, other: CycleNum) -> CycleNum {
        CycleNum(self.data() - other.data())
    }
}

bitflags! {

    #[derive(Default)]
    pub struct ClocksourceMask: u64 {
    }
    /// 时钟状态标记
    #[derive(Default)]
    pub struct ClocksourceFlags: u64 {
        /// 表示时钟设备是连续的
        const CLOCK_SOURCE_IS_CONTINUOUS = 0x01;
        /// 表示该时钟源需要经过watchdog检查
        const CLOCK_SOURCE_MUST_VERIFY = 0x02;
        /// 表示该时钟源是watchdog
        const CLOCK_SOURCE_WATCHDOG = 0x10;
        /// 表示该时钟源是高分辨率的
        const CLOCK_SOURCE_VALID_FOR_HRES = 0x20;
        /// 表示该时钟源误差过大
        const CLOCK_SOURCE_UNSTABLE = 0x40;
    }
}
impl From<u64> for ClocksourceMask {
    fn from(value: u64) -> Self {
        if value < 64 {
            return Self::from_bits_truncate((1 << value) - 1);
        }
        return Self::from_bits_truncate(u64::MAX);
    }
}
impl ClocksourceMask {
    pub fn new(b: u64) -> Self {
        Self { bits: b }
    }
}
impl ClocksourceFlags {
    pub fn new(b: u64) -> Self {
        Self { bits: b }
    }
}

#[derive(Debug)]
pub struct ClocksourceWatchdog {
    /// 监视器
    watchdog: Option<Arc<dyn Clocksource>>,
    /// 检查器是否在工作的标志
    is_running: bool,
    /// 定时监视器的过期时间
    timer_expires: u64,
}
impl ClocksourceWatchdog {
    pub fn new() -> Self {
        Self {
            watchdog: None,
            is_running: false,
            timer_expires: 0,
        }
    }

    fn is_due(&self, now: u64) -> bool {
        self.is_running && self.watchdog.is_some() && now >= self.timer_expires
    }

    /// 启用检查器
    pub fn clocksource_start_watchdog(&mut self, has_sources: bool) {
        // 如果watchdog未被设置或者已经启用了就退出
        if self.is_running || self.watchdog.is_none() || !has_sources {
            return;
        }
        // A restart must use an absolute jiffies deadline based
        // on the current clock, not on a stale deadline left by the previous
        // watchdog generation.
        self.timer_expires = watchdog_next_expiry(clock());
        let Some(watchdog) = self.watchdog.as_ref().cloned() else {
            return;
        };
        let watchdog_last = watchdog.read();
        if let Err(error) =
            watchdog.update_clocksource_data(ClocksourceUpdate::SetWatchdogLast(watchdog_last))
        {
            warn!("clocksource watchdog could not initialize reference state: {error:?}");
            return;
        }
        self.is_running = true;
    }

    /// 停止检查器
    pub fn clocksource_stop_watchdog(&mut self, has_sources: bool) {
        if !self.is_running || (self.watchdog.is_some() && has_sources) {
            return;
        }
        self.is_running = false;
    }
}

/// Closed set of metadata transitions allowed after construction. This keeps
/// watchdog/rating updates from accidentally overwriting immutable conversion
/// parameters with a stale cloned `ClocksourceData`.
#[derive(Debug, Clone)]
pub enum ClocksourceUpdate {
    RestoreBeforeRegistration(ClocksourceData),
    SetConversion {
        mult: u32,
        shift: u32,
    },
    SetUncertaintyAndAdjustment {
        uncertainty_margin: u32,
        maxadj: u32,
    },
    SetMaxAdjustment(u32),
    HalveConversion,
    SetDeferment {
        max_cycles: u64,
        max_idle_ns: u64,
    },
    ResetWatchdog,
    SetWatchdogLast(CycleNum),
    BeginWatchdog {
        watchdog_last: CycleNum,
        cs_last: CycleNum,
    },
    UpdateWatchdogSamples {
        watchdog_last: CycleNum,
        cs_last: CycleNum,
    },
    MarkValidForHres,
    MarkValidForHresIfStable,
    MarkUnstable,
    SetRating(i32),
}

impl ClocksourceUpdate {
    pub fn apply(self, data: &mut ClocksourceData) {
        match self {
            Self::RestoreBeforeRegistration(original) => *data = original,
            Self::SetConversion { mult, shift } => {
                data.mult = mult;
                data.shift = shift;
            }
            Self::SetUncertaintyAndAdjustment {
                uncertainty_margin,
                maxadj,
            } => {
                data.uncertainty_margin = uncertainty_margin;
                data.maxadj = maxadj;
            }
            Self::SetMaxAdjustment(maxadj) => data.maxadj = maxadj,
            Self::HalveConversion => {
                data.mult >>= 1;
                data.shift -= 1;
            }
            Self::SetDeferment {
                max_cycles,
                max_idle_ns,
            } => {
                data.max_cycles = max_cycles;
                data.max_idle_ns = max_idle_ns;
            }
            Self::ResetWatchdog => {
                data.flags.remove(ClocksourceFlags::CLOCK_SOURCE_WATCHDOG);
                data.watchdog_last = CycleNum::new(0);
                data.cs_last = CycleNum::new(0);
            }
            Self::SetWatchdogLast(value) => data.watchdog_last = value,
            Self::BeginWatchdog {
                watchdog_last,
                cs_last,
            } => {
                if !data.flags.contains(ClocksourceFlags::CLOCK_SOURCE_UNSTABLE) {
                    data.flags.insert(ClocksourceFlags::CLOCK_SOURCE_WATCHDOG);
                }
                data.watchdog_last = watchdog_last;
                data.cs_last = cs_last;
            }
            Self::UpdateWatchdogSamples {
                watchdog_last,
                cs_last,
            } => {
                data.watchdog_last = watchdog_last;
                data.cs_last = cs_last;
            }
            Self::MarkValidForHres => {
                data.flags
                    .insert(ClocksourceFlags::CLOCK_SOURCE_VALID_FOR_HRES);
            }
            Self::MarkValidForHresIfStable => {
                if !data.flags.contains(ClocksourceFlags::CLOCK_SOURCE_UNSTABLE) {
                    data.flags
                        .insert(ClocksourceFlags::CLOCK_SOURCE_VALID_FOR_HRES);
                }
            }
            Self::MarkUnstable => {
                data.flags.remove(
                    ClocksourceFlags::CLOCK_SOURCE_VALID_FOR_HRES
                        | ClocksourceFlags::CLOCK_SOURCE_WATCHDOG,
                );
                data.flags.insert(ClocksourceFlags::CLOCK_SOURCE_UNSTABLE);
            }
            Self::SetRating(rating) => data.rating = rating,
        }
    }
}

/// 时钟源的特性
pub trait Clocksource: Send + Sync + Debug {
    // TODO 返回值类型可能需要改变
    /// returns a cycle value, passes clocksource as argument
    fn read(&self) -> CycleNum;
    /// optional function to enable the clocksource
    fn enable(&self) -> Result<i32, SystemError> {
        return Err(SystemError::ENOSYS);
    }
    /// optional function to disable the clocksource
    #[allow(dead_code)]
    fn disable(&self) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }
    /// vsyscall based read
    #[allow(dead_code)]
    fn vread(&self) -> Result<CycleNum, SystemError> {
        return Err(SystemError::ENOSYS);
    }
    /// suspend function for the clocksource, if necessary
    fn suspend(&self) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }
    /// resume function for the clocksource, if necessary
    fn resume(&self) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }
    // 获取时钟源数据
    fn clocksource_data(&self) -> ClocksourceData;

    fn update_clocksource_data(&self, _update: ClocksourceUpdate) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }
    // 获取时钟源
    fn clocksource(&self) -> Arc<dyn Clocksource>;
}

fn validate_clocksource_conversion(
    data: &ClocksourceData,
    require_deferment: bool,
) -> Result<(), SystemError> {
    let mask = data.mask.bits();
    let contiguous_mask = mask != 0 && (mask & mask.wrapping_add(1)) == 0;
    if !contiguous_mask || data.mult == 0 || data.shift >= 64 {
        return Err(SystemError::EINVAL);
    }
    if require_deferment
        && (data.maxadj >= data.mult || data.max_cycles == 0 || data.max_cycles > mask)
    {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

fn remove_clocksource_identity(
    list: &mut LinkedList<Arc<dyn Clocksource>>,
    target: &Arc<dyn Clocksource>,
) -> bool {
    let mut retained = LinkedList::new();
    let mut removed = false;
    while let Some(candidate) = list.pop_front() {
        if !removed && Arc::ptr_eq(&candidate, target) {
            removed = true;
        } else {
            retained.push_back(candidate);
        }
    }
    *list = retained;
    removed
}

fn watchdog_replacement_for(
    target: &Arc<dyn Clocksource>,
) -> Result<Option<Arc<dyn Clocksource>>, SystemError> {
    let is_watchdog = CLOCKSOURCE_WATCHDOG
        .lock_irqsave()
        .watchdog
        .as_ref()
        .is_some_and(|watchdog| Arc::ptr_eq(watchdog, target));
    if !is_watchdog {
        return Ok(None);
    }

    select_watchdog_replacement(&CLOCKSOURCE_LIST.lock(), target)
        .map(Some)
        .ok_or(SystemError::EBUSY)
}

fn select_watchdog_replacement(
    list: &LinkedList<Arc<dyn Clocksource>>,
    target: &Arc<dyn Clocksource>,
) -> Option<Arc<dyn Clocksource>> {
    list.iter()
        .filter(|candidate| {
            if Arc::ptr_eq(candidate, target) {
                return false;
            }
            let flags = candidate.clocksource_data().flags;
            !flags.intersects(
                ClocksourceFlags::CLOCK_SOURCE_MUST_VERIFY
                    | ClocksourceFlags::CLOCK_SOURCE_UNSTABLE,
            )
        })
        .max_by_key(|candidate| {
            let data = candidate.clocksource_data();
            (
                independent_watchdog_preferred(&data.name) as i32,
                data.rating,
            )
        })
        .cloned()
}

/// watchdog 参考源是否为独立硬件时基。参考源会被无条件信任，因此它必须不依赖
/// 虚拟化平台的行为（HPET / ACPI PM Timer）；若让 paravirt 时钟（kvm-clock）凭
/// rating 当参考，宿主侧 pvti 一旦失谐（实测 kvm-clock 以 ~26% 速率慢走），
/// 健康硬件源反而会被判为"偏快"并错误标记 unstable。对齐 Linux 对 watchdog
/// 参考源的选择意图。
fn independent_watchdog_preferred(name: &str) -> bool {
    name == "hpet" || name == "acpi-pm" || name == "acpi_pm"
}

/// 参考源替换判定：独立硬件源优先于 paravirt 源；同类内按 rating。
fn watchdog_reference_better(
    new_name: &str,
    new_rating: i32,
    cur_name: &str,
    cur_rating: i32,
) -> bool {
    let new_indep = independent_watchdog_preferred(new_name);
    let cur_indep = independent_watchdog_preferred(cur_name);
    if new_indep != cur_indep {
        return new_indep;
    }
    new_rating > cur_rating
}

fn reset_watchdog_state(list: &LinkedList<Arc<dyn Clocksource>>) -> Result<(), SystemError> {
    for clocksource in list {
        clocksource.update_clocksource_data(ClocksourceUpdate::ResetWatchdog)?;
    }
    Ok(())
}

fn rollback_watchdog_registration(
    target: &Arc<dyn Clocksource>,
    previous_watchdog: Option<Arc<dyn Clocksource>>,
) {
    let mut watchdog = CLOCKSOURCE_WATCHDOG.lock_irqsave();
    let mut list = WATCHDOG_LIST.lock_irqsave();
    remove_clocksource_identity(&mut list, target);
    watchdog.watchdog = previous_watchdog;
    if let Err(error) = reset_watchdog_state(&list) {
        warn!("clocksource registration rollback could not reset watchdog state: {error:?}");
    }
    if let Err(error) = target.update_clocksource_data(ClocksourceUpdate::ResetWatchdog) {
        warn!("clocksource registration rollback could not reset source: {error:?}");
    }
    let has_sources = watchdog_has_sources(&watchdog.watchdog, &list);
    watchdog.clocksource_stop_watchdog(has_sources);
    watchdog.clocksource_start_watchdog(has_sources);
}

fn remove_unstable_clocksources(
    list: &mut LinkedList<Arc<dyn Clocksource>>,
) -> Vec<Arc<dyn Clocksource>> {
    let mut stable = LinkedList::new();
    let mut unstable = Vec::new();
    while let Some(clocksource) = list.pop_front() {
        if clocksource
            .clocksource_data()
            .flags
            .contains(ClocksourceFlags::CLOCK_SOURCE_UNSTABLE)
        {
            unstable.push(clocksource);
        } else {
            stable.push_back(clocksource);
        }
    }
    *list = stable;
    unstable
}

impl dyn Clocksource {
    /// # 计算时钟源能记录的最大时间跨度
    pub fn clocksource_max_deferment(&self) -> (u64, u64) {
        let data = self.clocksource_data();
        let max_mult = data.mult.saturating_add(data.maxadj).max(1) as u64;
        let max_cycles = (u64::MAX / max_mult).min(data.mask.bits());
        let min_mult = data.mult.saturating_sub(data.maxadj).max(1) as u128;
        let max_idle_ns = (((max_cycles as u128 * min_mult) >> data.shift) >> 1)
            .try_into()
            .unwrap_or(u64::MAX);
        (max_cycles, max_idle_ns)
    }

    /// # 计算时钟源的mult和shift，以便将一个时钟源的频率转换为另一个时钟源的频率
    fn clocks_calc_mult_shift(&self, from: u32, to: u32, maxsec: u32) -> (u32, u32) {
        let mut sftacc: u32 = 32;
        let mut sft = 1;

        // 计算限制转换范围的shift
        let mut mult = (maxsec as u64 * from as u64) >> 32;
        while mult != 0 {
            mult >>= 1;
            sftacc -= 1;
        }

        // 找到最佳的mult和shift
        for i in (1..=32).rev() {
            sft = i;
            mult = (to as u64) << sft;
            mult += from as u64 / 2;
            mult /= from as u64;
            if (mult >> sftacc) == 0 {
                break;
            }
        }

        return (mult as u32, sft);
    }

    /// # 计算时钟源可以进行的最大调整量
    fn clocksource_max_adjustment(&self) -> u32 {
        let cs_data = self.clocksource_data();
        let ret = cs_data.mult as u64 * 11 / 100;

        return ret as u32;
    }

    /// # 更新时钟源频率，初始化mult/shift 和 max_idle_ns
    fn clocksource_update_freq_scale(&self, scale: u32, freq: u32) -> Result<(), SystemError> {
        if freq != 0 && scale == 0 {
            return Err(SystemError::EINVAL);
        }

        if freq != 0 {
            let cs_data = self.clocksource_data();
            let mut sec: u64 = cs_data.mask.bits();

            sec /= freq as u64;
            sec /= scale as u64;
            if sec == 0 {
                sec = 1;
            } else if sec > 600 && cs_data.mask.bits() > u32::MAX as u64 {
                sec = 600;
            }

            let (mult, shift) =
                self.clocks_calc_mult_shift(freq, NSEC_PER_SEC / scale, sec as u32 * scale);
            self.update_clocksource_data(ClocksourceUpdate::SetConversion { mult, shift })?;
        }

        validate_clocksource_conversion(&self.clocksource_data(), false)?;

        let mut cs_data = self.clocksource_data();
        if scale != 0 && freq != 0 && cs_data.uncertainty_margin == 0 {
            let scaled_frequency = (scale as u64).saturating_mul(freq as u64);
            cs_data.uncertainty_margin = ((NSEC_PER_SEC as u64) / scaled_frequency)
                .try_into()
                .unwrap_or(u32::MAX);
            if cs_data.uncertainty_margin < 2 * WATCHDOG_MAX_SKEW {
                cs_data.uncertainty_margin = 2 * WATCHDOG_MAX_SKEW;
            }
        } else if cs_data.uncertainty_margin == 0 {
            cs_data.uncertainty_margin = WATCHDOG_THRESHOLD;
        }

        // 确保时钟源没有太大的mult值造成溢出
        let uncertainty_margin = cs_data.uncertainty_margin;
        let maxadj = self.clocksource_max_adjustment();
        self.update_clocksource_data(ClocksourceUpdate::SetUncertaintyAndAdjustment {
            uncertainty_margin,
            maxadj,
        })?;
        while freq != 0
            && (self.clocksource_data().mult + self.clocksource_data().maxadj
                < self.clocksource_data().mult
                || self.clocksource_data().mult - self.clocksource_data().maxadj
                    > self.clocksource_data().mult)
        {
            self.update_clocksource_data(ClocksourceUpdate::HalveConversion)?;
            let maxadj = self.clocksource_max_adjustment();
            self.update_clocksource_data(ClocksourceUpdate::SetMaxAdjustment(maxadj))?;
        }

        let adjusted_data = self.clocksource_data();
        if adjusted_data.maxadj >= adjusted_data.mult {
            return Err(SystemError::EINVAL);
        }

        let (max_cycles, max_idle_ns) = self.clocksource_max_deferment();
        self.update_clocksource_data(ClocksourceUpdate::SetDeferment {
            max_cycles,
            max_idle_ns,
        })?;

        return Ok(());
    }

    /// # 注册时钟源
    ///
    /// ## 参数
    ///
    /// - scale: 如果freq单位为0或hz，此值为1，如果为khz,此值为1000
    /// - freq: 时钟源的频率，jiffies注册时此值为0
    ///
    /// ## 返回值
    ///
    /// * `Ok(0)` - 时钟源注册成功。
    /// * `Err(SystemError)` - 时钟源注册失败。
    pub fn register(&self, scale: u32, freq: u32) -> Result<(), SystemError> {
        let _control_guard = CLOCKSOURCE_CONTROL_LOCK.lock_irqsave();
        let this = self.clocksource();
        let original_data = self.clocksource_data();
        if CLOCKSOURCE_LIST
            .lock()
            .iter()
            .any(|registered| Arc::ptr_eq(registered, &this))
        {
            return Err(SystemError::EEXIST);
        }

        if let Err(error) = self.clocksource_update_freq_scale(scale, freq) {
            let _ = self.update_clocksource_data(ClocksourceUpdate::RestoreBeforeRegistration(
                original_data.clone(),
            ));
            return Err(error);
        }
        if let Err(error) = validate_clocksource_conversion(&self.clocksource_data(), true) {
            let _ = self.update_clocksource_data(ClocksourceUpdate::RestoreBeforeRegistration(
                original_data.clone(),
            ));
            return Err(error);
        }

        // 将时钟源加入到时钟源队列中
        self.clocksource_enqueue();
        // 将时钟源加入到监视队列中
        let previous_watchdog = CLOCKSOURCE_WATCHDOG.lock_irqsave().watchdog.clone();
        if let Err(error) = self.clocksource_enqueue_watchdog() {
            // enqueue_watchdog may have published list membership and a new
            // reference before resetting every sample. Roll the complete
            // watchdog transaction back, not only the main registry entry.
            rollback_watchdog_registration(&this, previous_watchdog.clone());
            remove_clocksource_identity(&mut CLOCKSOURCE_LIST.lock(), &this);
            let _ = self.update_clocksource_data(ClocksourceUpdate::RestoreBeforeRegistration(
                original_data.clone(),
            ));
            return Err(error);
        }
        // 选择一个最好的时钟源
        if let Err(error) = clocksource_select_locked() {
            rollback_watchdog_registration(&this, previous_watchdog);
            remove_clocksource_identity(&mut CLOCKSOURCE_LIST.lock(), &this);
            let _ = self.update_clocksource_data(ClocksourceUpdate::RestoreBeforeRegistration(
                original_data.clone(),
            ));
            return Err(error);
        }
        debug!("clocksource_register successfully");
        return Ok(());
    }

    /// # 将时钟源插入时钟源队列
    pub fn clocksource_enqueue(&self) {
        // 根据rating由大到小排序
        let cs_data = self.clocksource_data();
        let mut list_guard = CLOCKSOURCE_LIST.lock();
        let mut spilt_pos: usize = list_guard.len();
        for (pos, ele) in list_guard.iter().enumerate() {
            if ele.clocksource_data().rating < cs_data.rating {
                spilt_pos = pos;
                break;
            }
        }
        let mut temp_list = list_guard.split_off(spilt_pos);
        let cs = self.clocksource();
        list_guard.push_back(cs);
        list_guard.append(&mut temp_list);
        // debug!(
        //     "CLOCKSOURCE_LIST len = {:?},clocksource_enqueue sccessfully",
        //     list_guard.len()
        // );
    }

    /// # 将时间源插入监控队列
    ///
    /// ## 返回值
    ///
    /// * `Ok(0)` - 时间源插入监控队列成功
    /// * `Err(SystemError)` - 时间源插入监控队列失败
    pub fn clocksource_enqueue_watchdog(&self) -> Result<i32, SystemError> {
        let cs_data = self.clocksource_data();
        let cs = self.clocksource();

        // Reference publication, membership and sample reset are one
        // watchdog transaction.  The watchdog callback holds the same lock
        // while sampling, so it can never combine samples from two reference
        // generations.
        let mut cs_watchdog = CLOCKSOURCE_WATCHDOG.lock_irqsave();
        let mut list_guard = WATCHDOG_LIST.lock_irqsave();
        if cs_data
            .flags
            .contains(ClocksourceFlags::CLOCK_SOURCE_MUST_VERIFY)
        {
            // cs是被监视的
            cs.update_clocksource_data(ClocksourceUpdate::ResetWatchdog)?;
            list_guard.push_back(cs);
        } else {
            // cs是监视器
            if cs_data
                .flags
                .contains(ClocksourceFlags::CLOCK_SOURCE_IS_CONTINUOUS)
            {
                // 如果时钟设备是连续的
                cs.update_clocksource_data(ClocksourceUpdate::MarkValidForHres)?;
            }
            // 将时钟源加入到监控队列中
            list_guard.push_back(cs.clone());

            // 对比当前注册的时间源的精度和监视器的精度
            let mut replaced = false;
            if cs_watchdog.watchdog.is_none()
                || cs_watchdog.watchdog.as_ref().is_some_and(|watchdog| {
                    let wd_data = watchdog.clocksource_data();
                    watchdog_reference_better(
                        &cs_data.name,
                        cs_data.rating,
                        &wd_data.name,
                        wd_data.rating,
                    )
                })
            {
                // 参考源替换：独立硬件时基（hpet/acpi_pm）优先于 paravirt 源，
                // 同类内按 rating（见 independent_watchdog_preferred）。
                cs_watchdog.watchdog.replace(cs);
                replaced = true;
            }
            if replaced {
                reset_watchdog_state(&list_guard)?;
            }
        }

        let has_sources = watchdog_has_sources(&cs_watchdog.watchdog, &list_guard);
        drop(list_guard);
        // This common tail is required when a MUST_VERIFY source is added
        // after its reference (the normal jiffies -> TSC registration order).
        cs_watchdog.clocksource_start_watchdog(has_sources);
        return Ok(0);
    }

    /// Mark a source unstable while the caller already serializes the
    /// watchdog generation.  This deliberately does not acquire the control
    /// lock: the watchdog timer holds `CLOCKSOURCE_WATCHDOG`, and taking
    /// control here would invert the control -> watchdog order used by
    /// registration and removal.
    fn mark_unstable_from_watchdog(&self, delta: i64) -> Result<i32, SystemError> {
        let cs_data = self.clocksource_data();
        // 打印出unstable的时钟源信息
        Logger.try_log_fmt(
            LogLevel::DEBUG,
            format_args!(
                "clocksource :{:?} is unstable, its delta is {:?}",
                cs_data.name, delta
            ),
        );
        self.update_clocksource_data(ClocksourceUpdate::MarkUnstable)?;

        // 启动watchdog线程 进行后续处理
        if FINISHED_BOOTING.load(Ordering::Relaxed) {
            // TODO 在实现了工作队列后，将启动线程换成schedule work
            run_watchdog_kthread();
        }
        return Ok(0);
    }

    /// # 将时间源从监视链表中弹出
    fn clocksource_dequeue_watchdog(
        &self,
        replacement: Option<Arc<dyn Clocksource>>,
    ) -> Result<(), SystemError> {
        let this = self.clocksource();
        let mut locked_watchdog = CLOCKSOURCE_WATCHDOG.lock_irqsave();
        let is_watchdog = locked_watchdog
            .watchdog
            .as_ref()
            .is_some_and(|watchdog| Arc::ptr_eq(watchdog, &this));
        if is_watchdog && replacement.is_none() {
            return Err(SystemError::EBUSY);
        }

        let mut list = WATCHDOG_LIST.lock_irqsave();
        if is_watchdog {
            reset_watchdog_state(&list)?;
        }
        self.update_clocksource_data(ClocksourceUpdate::ResetWatchdog)?;
        remove_clocksource_identity(&mut list, &this);
        if is_watchdog {
            locked_watchdog.watchdog = replacement;
        }
        let has_sources = watchdog_has_sources(&locked_watchdog.watchdog, &list);
        locked_watchdog.clocksource_stop_watchdog(has_sources);
        locked_watchdog.clocksource_start_watchdog(has_sources);
        Ok(())
    }

    /// # 将时钟源从时钟源链表中弹出
    fn clocksource_dequeue(&self) {
        let mut list = CLOCKSOURCE_LIST.lock();
        remove_clocksource_identity(&mut list, &self.clocksource());
    }

    /// # 注销时钟源
    #[allow(dead_code)]
    pub fn unregister(&self) -> Result<(), SystemError> {
        let _control_guard = CLOCKSOURCE_CONTROL_LOCK.lock_irqsave();
        let this = self.clocksource();
        let registered = CLOCKSOURCE_LIST
            .lock()
            .iter()
            .any(|candidate| Arc::ptr_eq(candidate, &this));
        if !registered {
            return Err(SystemError::ENOENT);
        }
        let watchdog_replacement = watchdog_replacement_for(&this)?;
        let is_active = timekeeping::timekeeping_is_initialized()
            && timekeeping::timekeeper()
                .current_clocksource()
                .is_some_and(|current| Arc::ptr_eq(&current, &this));
        if is_active {
            let alternative = CLOCKSOURCE_LIST
                .lock()
                .iter()
                .find(|candidate| {
                    !Arc::ptr_eq(candidate, &this)
                        && !candidate
                            .clocksource_data()
                            .flags
                            .contains(ClocksourceFlags::CLOCK_SOURCE_UNSTABLE)
                })
                .cloned()
                .ok_or(SystemError::EBUSY)?;
            // Switch while the old source is still registered and owned.  A
            // failed validation leaves both registry and timekeeper intact.
            timekeeping::install_clocksource(alternative)?;
        }
        // 将时钟源从监视链表中弹出
        self.clocksource_dequeue_watchdog(watchdog_replacement)?;
        // 将时钟源从时钟源链表中弹出
        self.clocksource_dequeue();
        // 检查是否有更好的时钟源
        clocksource_select_locked()?;
        Ok(())
    }
    /// # 修改时钟源的精度
    ///
    /// ## 参数
    ///
    /// * `rating` - 指定的时钟精度
    ///
    /// Reorder a registered source while the caller holds
    /// `CLOCKSOURCE_CONTROL_LOCK`.
    fn clocksource_change_rating_locked(&self, rating: i32) -> Result<(), SystemError> {
        // 将时钟源从链表中弹出
        self.clocksource_dequeue();
        if let Err(error) = self.update_clocksource_data(ClocksourceUpdate::SetRating(rating)) {
            // Preserve registry membership if a driver rejects the update.
            self.clocksource_enqueue();
            return Err(error);
        }
        // 插入时钟源到时钟源链表中
        self.clocksource_enqueue();
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ClocksourceData {
    /// 时钟源名字
    pub name: Arc<str>,
    /// 时钟精度
    pub rating: i32,
    pub mask: ClocksourceMask,
    pub mult: u32,
    pub shift: u32,
    pub max_cycles: u64,
    pub max_idle_ns: u64,
    pub flags: ClocksourceFlags,
    pub watchdog_last: CycleNum,
    /// 用于watchdog机制中的字段，记录主时钟源上一次被读取的周期数
    pub cs_last: CycleNum,
    // 用于描述时钟源的不确定性边界，时钟源读取的时间可能存在的不确定性和误差范围
    pub uncertainty_margin: u32,
    // 最大的时间调整量
    pub maxadj: u32,
}

impl ClocksourceData {
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        rating: i32,
        mask: ClocksourceMask,
        mult: u32,
        shift: u32,
        max_idle_ns: u64,
        flags: ClocksourceFlags,
        uncertainty_margin: u32,
        maxadj: u32,
    ) -> Self {
        let csd = ClocksourceData {
            name: name.into(),
            rating,
            mask,
            mult,
            shift,
            max_cycles: 0,
            max_idle_ns,
            flags,
            watchdog_last: CycleNum(0),
            cs_last: CycleNum(0),
            uncertainty_margin,
            maxadj,
        };
        return csd;
    }
}

///  converts clocksource cycles to nanoseconds
///
pub fn clocksource_cyc2ns(cycles: CycleNum, mult: u32, shift: u32) -> u64 {
    if shift >= 128 {
        return 0;
    }
    let nanoseconds = ((cycles.data() as u128) * (mult as u128)) >> shift;
    nanoseconds.min(u64::MAX as u128) as u64
}

/// # 重启所有的时间源
#[allow(dead_code)]
pub fn clocksource_resume() {
    let list = CLOCKSOURCE_LIST.lock();
    for ele in list.iter() {
        let data = ele.clocksource_data();
        match ele.resume() {
            Ok(_) => continue,
            Err(_) => {
                debug!("clocksource {:?} resume failed", data.name);
            }
        }
    }
    clocksource_resume_watchdog();
}

/// # 暂停所有的时间源
#[allow(dead_code)]
pub fn clocksource_suspend() {
    let list = CLOCKSOURCE_LIST.lock();
    for ele in list.iter() {
        let data = ele.clocksource_data();
        match ele.suspend() {
            Ok(_) => continue,
            Err(_) => {
                debug!("clocksource {:?} suspend failed", data.name);
            }
        }
    }
}

/// Cheap bounded check used by the existing timer IRQ/softirq path. A busy
/// watchdog transaction leaves the deadline armed for a later tick.
pub(super) fn watchdog_due(now: u64) -> bool {
    CLOCKSOURCE_WATCHDOG
        .try_lock_irqsave()
        .is_ok_and(|state| state.is_due(now))
}

pub(super) fn run_due_watchdog() {
    let Ok(mut state) = CLOCKSOURCE_WATCHDOG.try_lock_irqsave() else {
        return;
    };
    if state.is_due(clock()) {
        if let Err(error) = run_watchdog(&mut state, &WATCHDOG_LIST.lock_irqsave()) {
            Logger.try_log_fmt(
                LogLevel::WARN,
                format_args!("clocksource watchdog sample failed: {error:?}"),
            );
        }
    }
}

fn run_watchdog(
    state: &mut ClocksourceWatchdog,
    list: &LinkedList<Arc<dyn Clocksource>>,
) -> Result<(), SystemError> {
    if !state.is_running {
        return Ok(());
    }
    let Some(watchdog) = &state.watchdog else {
        return Ok(());
    };
    let result = sample_watchdog(watchdog, list);
    // Errors must not silently stop a periodic service. Serialize rearm with
    // start/stop/reference replacement in the original watchdog transaction.
    state.timer_expires = watchdog_next_expiry(clock());
    result
}

fn sample_watchdog(
    watchdog: &Arc<dyn Clocksource>,
    watchdog_list: &LinkedList<Arc<dyn Clocksource>>,
) -> Result<(), SystemError> {
    let wd_now_data = watchdog.clocksource_data();
    for cs in watchdog_list.iter() {
        if Arc::ptr_eq(cs, watchdog) {
            continue;
        }
        let cs_data = cs.clocksource_data();
        // 判断时钟源是否已经被标记为不稳定
        if cs_data
            .flags
            .contains(ClocksourceFlags::CLOCK_SOURCE_UNSTABLE)
        {
            // debug!("clocksource_watchdog unstable");
            // 启动watchdog_kthread
            if FINISHED_BOOTING.load(Ordering::Relaxed) {
                // TODO 在实现了工作队列后，将启动线程换成schedule work
                run_watchdog_kthread();
            }
            continue;
        }

        // 读取时钟源现在的时间
        let cs_now_clock = cs.read();
        // 读取watchdog现在的时间
        let wd_now_clock = watchdog.read().data();

        // info!("cs_name = {:?}", cs_data.name);
        // info!("cs_last = {:?}", cs_data.cs_last);
        // info!("cs_now_clock = {:?}", cs_now_clock);
        // info!("wd_name");
        // info!("wd_last = {:?}", cs_data.watchdog_last);
        // info!("wd_now_clock = {:?}", wd_now_clock);

        // 如果时钟源没有被监视，则开始监视他
        if !cs_data
            .flags
            .contains(ClocksourceFlags::CLOCK_SOURCE_WATCHDOG)
        {
            // debug!("clocksource_watchdog start watch");
            cs.update_clocksource_data(ClocksourceUpdate::BeginWatchdog {
                watchdog_last: CycleNum::new(wd_now_clock),
                cs_last: cs_now_clock,
            })?;
            continue;
        }

        let wd_dev_nsec = clocksource_cyc2ns(
            CycleNum(
                wd_now_clock.wrapping_sub(cs_data.watchdog_last.data()) & wd_now_data.mask.bits,
            ),
            wd_now_data.mult,
            wd_now_data.shift,
        );

        let cs_dev_nsec = clocksource_cyc2ns(
            CycleNum(cs_now_clock.data().wrapping_sub(cs_data.cs_last.data()) & cs_data.mask.bits),
            cs_data.mult,  // 2343484437
            cs_data.shift, // 23
        );
        // 记录此次检查的时刻
        cs.update_clocksource_data(ClocksourceUpdate::UpdateWatchdogSamples {
            watchdog_last: CycleNum::new(wd_now_clock),
            cs_last: cs_now_clock,
        })?;

        // 长间隔不豁免比对：watchdog tick 被延迟时（系统降级本身就会延迟它），
        // 健康源两侧 delta 会同步变大、差值仍≈0；而慢速/倒走源的差值只会更醒目。
        // 跳过比对等于在本该抓到病灶的降级场景自我失明（实测 kvm-clock 慢 5 倍时
        // cs=504ms vs wd=2496ms 被这一分支放过，tick 空转风暴因此不被纠正）。
        // 与 Linux 6.6 的 watchdog 一致：只告警，不跳过。
        if wd_dev_nsec > WATCHDOG_INTERVAL_MAX_NS
            && FINISHED_BOOTING.load(Ordering::Relaxed)
            && should_report_long_watchdog_interval(wd_dev_nsec)
        {
            Logger.try_log_fmt(
                LogLevel::WARN,
                format_args!(
                    "clocksource watchdog: long readout interval: cs_nsec={} wd_nsec={}",
                    cs_dev_nsec, wd_dev_nsec
                ),
            );
        }

        if cs_dev_nsec.abs_diff(wd_dev_nsec) > WATCHDOG_THRESHOLD.into() {
            // debug!("set_unstable");
            // 误差过大，标记为unstable
            Logger.try_log_fmt(
                LogLevel::INFO,
                format_args!(
                    "clocksource watchdog skew: cs_dev_nsec={} wd_dev_nsec={}",
                    cs_dev_nsec, wd_dev_nsec
                ),
            );
            cs.mark_unstable_from_watchdog(
                cs_dev_nsec
                    .abs_diff(wd_dev_nsec)
                    .try_into()
                    .unwrap_or(i64::MAX),
            )?;
            continue;
        }

        // 判断是否要切换为高精度模式
        if !cs_data
            .flags
            .contains(ClocksourceFlags::CLOCK_SOURCE_VALID_FOR_HRES)
            && cs_data
                .flags
                .contains(ClocksourceFlags::CLOCK_SOURCE_IS_CONTINUOUS)
            && wd_now_data
                .flags
                .contains(ClocksourceFlags::CLOCK_SOURCE_IS_CONTINUOUS)
        {
            cs.update_clocksource_data(ClocksourceUpdate::MarkValidForHresIfStable)?;
            // TODO 通知tick机制 切换为高精度模式
        }
    }
    Ok(())
}

/// 活跃时钟源健康自检：间隔 ~1ms 读取两次单调钟，倒退、冻结或明显慢速都判为
/// 不可信。仅 x86_64（本缺陷路径），其他架构恒为健康。
#[cfg(target_arch = "x86_64")]
fn active_clocksource_regressed() -> bool {
    let khz = crate::arch::driver::tsc::TSCManager::cpu_khz();
    if khz == 0 {
        return false;
    }
    // 交叉验证（对齐 Linux watchdog 的"独立参考"原则）：受检的是单调钟自身，
    // 倒走时它的两次读数依然"自洽"，自一致性检查恒过。改用独立硬件参考：
    // rdtsc 是纯硬件单调计数器，不受 pvclock 倒走影响。1ms 内 TSC 前进
    // ≥0.9ms，而单调钟停滞（≤50µs 推进）、倒退或慢速（<75% 参考速率）
    // → 不可信。
    let tsc0 = unsafe { x86::time::rdtsc() };
    let a = crate::time::deadline::now_ns();
    let span = khz.saturating_mul(1000);
    while unsafe { x86::time::rdtsc() }.wrapping_sub(tsc0) < span {
        core::hint::spin_loop();
    }
    let tsc1 = unsafe { x86::time::rdtsc() };
    let b = crate::time::deadline::now_ns();
    let tsc_cycles = tsc1.wrapping_sub(tsc0);
    // TSC 参考间隔换算为纳秒（~1ms 窗口，u64 不会溢出）。
    let tsc_ns = tsc_cycles / khz * 1_000_000;
    let mono_ns = b.saturating_sub(a);
    // 倒退/冻结（≤50µs 推进）或慢速（<75% TSC 参考速率）都判不可信：
    // pvclock 平滑倒走被单调钳制后的典型形态是"慢速"而非冻结，仅冻结检测
    // 会漏掉它。
    tsc_cycles >= khz.saturating_mul(900)
        && (b.saturating_add(50_000) <= a || mono_ns * 4 < tsc_ns * 3)
}

#[cfg(not(target_arch = "x86_64"))]
fn active_clocksource_regressed() -> bool {
    false
}

fn __clocksource_watchdog_kthread() {
    // Registration and removal use control -> watchdog -> list. Keep the
    // unstable-source cleanup in the same transaction so unregister cannot
    // remove a source after it leaves WATCHDOG_LIST and before it is rerated.
    let _control_guard = CLOCKSOURCE_CONTROL_LOCK.lock_irqsave();
    // 活跃时钟源倒走/冻结自检：tick 驱动的常规 watchdog 比对在系统降级
    // （定时器风暴/调度异常）时可能不再运行，这里在 kthread 唤醒时直接检查
    // timekeeper 当前源的健康度。倒走/冻结/慢速 → 标记 unstable 并把 rating
    // 降到 0，随后的重选会切到健康源（TSC/HPET/jiffies）。对齐 Linux
    // clocksource watchdog 对不可信源的处理语义。
    if active_clocksource_regressed() {
        if let Some(active) = crate::time::timekeeping::timekeeper().current_clocksource() {
            Logger.try_log_fmt(
                LogLevel::WARN,
                format_args!(
                    "clocksource: active source '{}' is running backward or frozen, forcing reselection",
                    active.clocksource_data().name
                ),
            );
            let _ = active.update_clocksource_data(ClocksourceUpdate::MarkUnstable);
            if let Err(error) = active.clocksource_change_rating_locked(0) {
                Logger.try_log_fmt(
                    LogLevel::WARN,
                    format_args!("clocksource: forced rerating failed: {error:?}"),
                );
            }
        }
    }
    let mut watchdog = CLOCKSOURCE_WATCHDOG.lock_irqsave();
    let mut wd_list = WATCHDOG_LIST.lock_irqsave();
    let del_clocks = remove_unstable_clocksources(&mut wd_list);

    let unstable_reference = watchdog.watchdog.as_ref().is_some_and(|reference| {
        reference
            .clocksource_data()
            .flags
            .contains(ClocksourceFlags::CLOCK_SOURCE_UNSTABLE)
    });
    if unstable_reference {
        if let Some(old_reference) = watchdog.watchdog.take() {
            watchdog.watchdog = select_watchdog_replacement(&wd_list, &old_reference);
            if let Err(error) = reset_watchdog_state(&wd_list) {
                warn!("clocksource watchdog replacement could not reset state: {error:?}");
            }
        }
    }

    // 检查是否需要停止watchdog
    let has_sources = watchdog_has_sources(&watchdog.watchdog, &wd_list);
    drop(wd_list);
    watchdog.clocksource_stop_watchdog(has_sources);
    drop(watchdog);
    // 将不稳定的时钟源精度都设置为最低，然后删除unstable标记
    for clock in del_clocks.iter() {
        if let Err(error) = clock.clocksource_change_rating_locked(0) {
            warn!("clocksource unstable rating update failed: {:?}", error);
        }
    }
    if let Err(error) = clocksource_select_locked() {
        warn!(
            "clocksource unstable cleanup could not switch source: {:?}",
            error
        );
    }
}

/// # watchdog线程的逻辑，执行unstable的后续操作
pub fn clocksource_watchdog_kthread() -> i32 {
    // return 0;
    loop {
        // debug!("clocksource_watchdog_kthread start");
        __clocksource_watchdog_kthread();
        if KernelThreadMechanism::should_stop(&ProcessManager::current_pcb()) {
            break;
        }
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::mark_sleep(true).expect("clocksource_watchdog_kthread:mark sleep failed");
        drop(irq_guard);
        schedule(SchedMode::SM_NONE);
    }
    return 0;
}

/// # 清空所有时钟源的watchdog标志位
pub fn clocksource_reset_watchdog() {
    let _watchdog = CLOCKSOURCE_WATCHDOG.lock_irqsave();
    let list_guard = WATCHDOG_LIST.lock_irqsave();
    for ele in list_guard.iter() {
        ele.update_clocksource_data(ClocksourceUpdate::ResetWatchdog)
            .expect("clocksource_reset_watchdog: metadata update failed");
    }
}

/// # 重启检查器
pub fn clocksource_resume_watchdog() {
    clocksource_reset_watchdog();
}

/// # 根据精度选择最优的时钟源，或者接受用户指定的时间源
pub fn clocksource_select() {
    let _control_guard = CLOCKSOURCE_CONTROL_LOCK.lock_irqsave();
    if let Err(error) = clocksource_select_locked() {
        warn!("clocksource selection failed: {:?}", error);
    }
}

fn clocksource_select_locked() -> Result<(), SystemError> {
    if !FINISHED_BOOTING.load(Ordering::Relaxed) {
        return Ok(());
    }
    let best = {
        let list_guard = CLOCKSOURCE_LIST.lock();
        if list_guard.is_empty() {
            return Ok(());
        }
        let Some(mut best) = list_guard
            .iter()
            .find(|candidate| {
                !candidate
                    .clocksource_data()
                    .flags
                    .contains(ClocksourceFlags::CLOCK_SOURCE_UNSTABLE)
            })
            .cloned()
        else {
            return Ok(());
        };
        let override_name = OVERRIDE_NAME.lock();
        for ele in list_guard.iter() {
            let data = ele.clocksource_data();
            if data.name.as_ref() == override_name.as_str()
                && !data.flags.contains(ClocksourceFlags::CLOCK_SOURCE_UNSTABLE)
            {
                best = ele.clone();
                break;
            }
        }
        best
    };

    let should_update = timekeeping::timekeeper()
        .current_clocksource()
        .is_none_or(|current| !Arc::ptr_eq(&current, &best));
    if should_update && timekeeping::timekeeping_is_initialized() {
        info!(
            "Switching to the clocksource {:?}\n",
            best.clocksource_data().name
        );
        timekeeping::install_clocksource(best.clone())?;
    }
    debug!("clocksource_select finish, current = {best:?}");
    Ok(())
}

/// # clocksource模块加载完成
pub fn clocksource_boot_finish() {
    FINISHED_BOOTING.store(true, Ordering::Relaxed);
    clocksource_select();
    // 清除不稳定的时钟源
    __clocksource_watchdog_kthread();
    debug!("clocksource_boot_finish");
}

pub(crate) fn run_watchdog_kthread() {
    if let Some(watchdog_kthread) = unsafe { WATCHDOG_KTHREAD.clone() } {
        ProcessManager::wakeup(&watchdog_kthread).ok();
    }
}

#[unified_init(INITCALL_LATE)]
pub fn init_watchdog_kthread() -> Result<(), SystemError> {
    assert!(CurrentIrqArch::is_irq_enabled());
    let closure = KernelThreadClosure::StaticEmptyClosure((
        &(clocksource_watchdog_kthread as fn() -> i32),
        (),
    ));
    let pcb = KernelThreadMechanism::create_and_run(closure, "clocksource watchdog".to_string())
        .ok_or(SystemError::EPERM)?;
    unsafe {
        WATCHDOG_KTHREAD.replace(pcb);
    }

    return Ok(());
}

#[path = "clocksource_selftest.rs"]
mod selftest;

pub(crate) use selftest::run_clocksource_selftests;

#[cfg(feature = "mm_selftest")]
pub(crate) use selftest::heap_denied_watchdog;
