use core::intrinsics::log2f32;

use alloc::{
    boxed::Box,
    collections::LinkedList,
    string::{String, ToString},
    sync::{Arc, Weak},
};
use lazy_static::__Deref;

use crate::{kdebug, kinfo, libs::spinlock::SpinLock, syscall::SystemError};

use super::{
    timer::{clock, Timer, TimerFunction},
    NSEC_PER_SEC,
};

lazy_static! {
    /// linked list with the registered clocksources
    pub static ref CLOCKSOURCE_LIST: SpinLock<LinkedList<Arc<dyn Clocksource>>> =
        SpinLock::new(LinkedList::new());
    /// 被监视中的时钟源
    pub static ref WATCHDOG_LIST: SpinLock<LinkedList<Arc<dyn Clocksource>>> =
        SpinLock::new(LinkedList::new());

    pub static ref CLOCKSOUCE_WATCHDOG:SpinLock<ClocksouceWatchdog>  = SpinLock::new(ClocksouceWatchdog::new());

    pub static ref OVERRIDE_NAME: SpinLock<String> = SpinLock::new(String::from(""));

}
//一些应该放在jiffies里里面的常量 暂时先放一下
pub const CLOCK_TICK_RATE: u32 = HZ as u32 * 100000;
pub const JIFFIES_SHIFT: u32 = 8;
pub const LATCH: u32 = ((CLOCK_TICK_RATE + (HZ as u32) / 2) / HZ as u32) as u32;
pub const ACTHZ: u32 = sh_div(CLOCK_TICK_RATE, LATCH, 8);
pub const NSEC_PER_JIFFY: u32 = ((NSEC_PER_SEC << 8) / ACTHZ) as u32;

/// 正在被使用时钟源
pub static CUR_CLOCKSOURCE: SpinLock<Option<Arc<dyn Clocksource>>> = SpinLock::new(None);
/// 是否完成加载
pub static FINISHED_BOOTING: SpinLock<bool> = SpinLock::new(false);

// Interval: 0.5sec Threshold: 0.0625s
// BUG HZ无法获取
pub const HZ: u64 = 1;
pub const WATCHDOG_INTERVAL: u64 = HZ >> 1;
pub const WATCHDOG_THRESHOLD: u32 = NSEC_PER_SEC >> 4;

// 时钟周期数
#[derive(Debug, Clone, Copy)]
#[repr(transparent)]
pub struct CycleNum(u64);
impl CycleNum {
    #[inline(always)]
    pub fn new(cycle: u64) -> Self {
        Self(cycle)
    }
    #[inline(always)]
    pub fn data(&self) -> u64 {
        self.0
    }
    #[inline(always)]
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
    pub struct ClocksourceMask:u64{

    }
    /// 时钟状态标记
    #[derive(Default)]
    pub struct ClocksourceFlags:u64{
        const CLOCK_SOURCE_IS_CONTINUOUS  =0x01;
        const CLOCK_SOURCE_MUST_VERIFY = 0x02;
        const CLOCK_SOURCE_WATCHDOG = 0x10;
        const CLOCK_SOURCE_VALID_FOR_HRES = 0x20;
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
pub struct ClocksouceWatchdog {
    /// 监视器
    watchdog: Option<Arc<dyn Clocksource>>,
    /// 检查器是否在工作的标志
    is_running: bool,
    /// 上一次检查的时刻
    last_check: CycleNum,
    /// 定时监视器的过期时间
    timer_expires: u64,
}
impl ClocksouceWatchdog {
    pub fn new() -> Self {
        Self {
            watchdog: None,
            is_running: false,
            last_check: CycleNum(0),
            timer_expires: 0,
        }
    }
    fn get_watchdog(&mut self) -> &mut Option<Arc<dyn Clocksource>> {
        &mut self.watchdog
    }
}
/// 定时检查器
pub struct WatchdogTimerFunc;
impl TimerFunction for WatchdogTimerFunc {
    fn run(&mut self) -> Result<(), SystemError> {
        return clocksource_watchdog();
    }
}
/// 时钟源的特性
pub trait Clocksource: Send + Sync {
    // TODO 返回值类型可能需要改变
    /// returns a cycle value, passes clocksource as argument
    fn read(&self) -> CycleNum;
    /// optional function to enable the clocksource
    fn enable(&self) -> Result<i32, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    /// optional function to disable the clocksource
    fn disable(&self) -> Result<(), SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    /// vsyscall based read
    fn vread(&self) -> Result<CycleNum, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    /// suspend function for the clocksource, if necessary
    fn suspend(&self) -> Result<(), SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    /// resume function for the clocksource, if necessary
    fn resume(&self) -> Result<(), SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    // 获取时钟源数据
    fn clocksource_data(&self) -> ClocksourceData;

    fn update_clocksource_data(&self, _data: ClocksourceData) -> Result<(), SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    // 获取时钟源
    fn clocksource(&self) -> Arc<dyn Clocksource>;
}

impl dyn Clocksource {
    // BUG 可能会出现格式转换导致结果错误的问题
    pub fn clocksource_max_deferment(&self) -> u64 {
        let cs_data_guard = self.clocksource_data();
        let max_nsecs: u64;
        let mut max_cycles: u64;
        max_cycles =
            (1 << (63 - (unsafe { log2f32(cs_data_guard.mult as f32) } as u32 + 1))) as u64;
        max_cycles = max_cycles.min(cs_data_guard.mask.bits);
        max_nsecs = clocksource_cyc2ns(
            CycleNum(max_cycles),
            cs_data_guard.mult,
            cs_data_guard.shift,
        );
        return max_nsecs - (max_nsecs >> 5);
    }

    pub fn register(&self) -> Result<(), SystemError> {
        let ns = self.clocksource_max_deferment();
        let mut cs_data = self.clocksource_data();

        cs_data.max_idle_ns = ns as u32;
        self.update_clocksource_data(cs_data)?;
        self.clocksource_enqueue();
        self.clocksource_enqueue_watchdog();
        return Ok(());
    }
    /// 将时间源插入时间源队列
    pub fn clocksource_enqueue(&self) {
        // 根据rating由大到小排序
        let cs_data = self.clocksource_data();
        let list_guard = &mut CLOCKSOURCE_LIST.lock();
        let mut spilt_pos: usize = 0;
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
    }

    /// 将时间源插入监控队列
    pub fn clocksource_enqueue_watchdog(&self) -> Result<(), SystemError> {
        // BUG 可能需要lock irq
        let mut cs_data = self.clocksource_data();
        let list_guard = &mut WATCHDOG_LIST.lock();
        let cs = self.clocksource();
        if cs_data
            .flags
            .contains(ClocksourceFlags::CLOCK_SOURCE_MUST_VERIFY)
        {
            // cs是被监视的
            cs_data
                .flags
                .remove(ClocksourceFlags::CLOCK_SOURCE_WATCHDOG);
            cs.update_clocksource_data(cs_data)?;
            list_guard.push_back(cs);
        } else {
            // cs是监视器
            if cs_data
                .flags
                .contains(ClocksourceFlags::CLOCK_SOURCE_IS_CONTINUOUS)
            {
                cs_data
                    .flags
                    .insert(ClocksourceFlags::CLOCK_SOURCE_VALID_FOR_HRES);
                cs.update_clocksource_data(cs_data.clone())?;
            }
            // 选择一个最优的监视器
            let cs_watchdog = &mut CLOCKSOUCE_WATCHDOG.lock();
            if cs_watchdog.watchdog.is_none()
                || cs_data.rating
                    > cs_watchdog
                        .watchdog
                        .clone()
                        .unwrap()
                        .clocksource_data()
                        .rating
            {
                // 替换监视器
                cs_watchdog.watchdog.replace(cs);
                drop(cs_watchdog);
                drop(list_guard);
                self.clocksource_reset_watchdog();
            }
            self.clocksource_start_watchdog();
        }
        return Ok(());
    }

    pub fn clocksource_reset_watchdog(&self) {
        let list_guard = &mut WATCHDOG_LIST.lock();
        for ele in list_guard.iter() {
            ele.clocksource_data()
                .flags
                .remove(ClocksourceFlags::CLOCK_SOURCE_WATCHDOG);
        }
    }
    /// 启用检查器
    pub fn clocksource_start_watchdog(&self) {
        let cs_watchdog = &mut CLOCKSOUCE_WATCHDOG.lock();
        // 如果watchdog未被设置或者已经启用了就退出
        let watchdog_list = &WATCHDOG_LIST.lock();
        if cs_watchdog.is_running || cs_watchdog.watchdog.is_none() || watchdog_list.is_empty() {
            return;
        }
        // 生成一个定时器
        let wd_timer_func: Box<WatchdogTimerFunc> = Box::new(WatchdogTimerFunc {});
        cs_watchdog.timer_expires += clock() + WATCHDOG_INTERVAL;
        cs_watchdog.last_check = cs_watchdog.watchdog.as_ref().unwrap().clone().read();
        let wd_timer = Timer::new(wd_timer_func, cs_watchdog.timer_expires);
        wd_timer.activate();
        cs_watchdog.is_running = true;
    }

    /// 将时钟源设立为unstable
    pub fn set_unstable(&self, delta: i64) -> Result<(), SystemError> {
        let mut cs_data = self.clocksource_data();
        kdebug!(
            "clocksource :{:?} is unstable, its delta is {:?}",
            cs_data.name,
            delta
        );
        cs_data.flags.remove(
            ClocksourceFlags::CLOCK_SOURCE_VALID_FOR_HRES | ClocksourceFlags::CLOCK_SOURCE_WATCHDOG,
        );
        cs_data
            .flags
            .insert(ClocksourceFlags::CLOCK_SOURCE_UNSTABLE);
        self.update_clocksource_data(cs_data)?;

        return Ok(());
    }

    /// 根据精度选择最优的时钟源，或者接受用户指定的时间源
    pub fn clocksource_select(&self) {
        let list_guard = &mut CLOCKSOURCE_LIST.lock();
        if *FINISHED_BOOTING.lock() || list_guard.is_empty() {
            return;
        }
        let mut best = list_guard.front().unwrap().clone();
        let override_name = OVERRIDE_NAME.lock();
        // 判断是否有用户空间指定的时间源
        for ele in list_guard.iter() {
            if ele.clocksource_data().name.eq(override_name.deref()) {
                // TODO 判断是否是高精度模式
                // 如果是高精度模式，但是时钟源不支持高精度模式的话，就要退出循环
                best = ele.clone();
                break;
            }
        }
        let cur_clocksource = CUR_CLOCKSOURCE.lock().as_ref().unwrap().clone();
        let best_name = &best.clocksource_data().name;
        if cur_clocksource.clocksource_data().name.eq(best_name) {
            kinfo!("Switching to clocksource {:?}\n", best_name);
            drop(cur_clocksource);
            CUR_CLOCKSOURCE.lock().replace(best);
            // TODO 通知timerkeeping 切换了时间源
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClocksourceData {
    /// 时钟源名字
    name: String,
    /// 时钟精度
    rating: i32,
    mask: ClocksourceMask,
    mult: u32,
    shift: u32,
    max_idle_ns: u32,
    flags: ClocksourceFlags,
    watchdog_last: CycleNum,
}
impl ClocksourceData {
    pub fn new(
        name: String,
        rating: i32,
        mask: ClocksourceMask,
        mult: u32,
        shift: u32,
        max_idle_ns: u32,
        flags: ClocksourceFlags,
    ) -> Self {
        let csd = ClocksourceData {
            name: name,
            rating: rating,
            mask: mask,
            mult: mult,
            shift: shift,
            max_idle_ns: max_idle_ns,
            flags: flags,
            watchdog_last: CycleNum(0),
        };
        return csd;
    }

    pub fn set_name(&mut self, name: String) {
        self.name = name;
    }
    pub fn set_rating(&mut self, rating: i32) {
        self.rating = rating;
    }
    pub fn set_mask(&mut self, mask: ClocksourceMask) {
        self.mask = mask;
    }
    pub fn set_mult(&mut self, mult: u32) {
        self.mult = mult;
    }
    pub fn set_shift(&mut self, shift: u32) {
        self.shift = shift;
    }
    pub fn set_max_idle_ns(&mut self, max_idle_ns: u32) {
        self.max_idle_ns = max_idle_ns;
    }
    pub fn set_flags(&mut self, flags: ClocksourceFlags) {
        self.flags = flags;
    }
    pub fn remove_flags(&mut self, flags: ClocksourceFlags) {
        self.flags.remove(flags)
    }
    pub fn insert_flags(&mut self, flags: ClocksourceFlags) {
        self.flags.insert(flags)
    }
}

///  converts clocksource cycles to nanoseconds
///
pub fn clocksource_cyc2ns(cycles: CycleNum, mult: u32, shift: u32) -> u64 {
    return (cycles.data() * mult as u64) >> shift;
}
/// 根据watchdog的精度，来检查被监视的时钟源的误差
/// 如果误差过大，时钟源将被标记为不稳定
pub fn clocksource_watchdog() -> Result<(), SystemError> {
    let mut cs_watchdog = CLOCKSOUCE_WATCHDOG.lock();

    // watchdog没有运行的话直接退出
    if !cs_watchdog.is_running || cs_watchdog.watchdog.is_none() {
        return Ok(());
    }
    let cur_watchdog = cs_watchdog.watchdog.as_ref().unwrap().clone();
    let cur_wd_data = cur_watchdog.as_ref().clocksource_data();
    let cur_wd_nowclock = cur_watchdog.as_ref().read().data();

    let wd_last = cs_watchdog.last_check.data();
    let wd_dev_nsec = clocksource_cyc2ns(
        CycleNum((cur_wd_nowclock - wd_last) & cur_wd_data.mask.bits),
        cur_wd_data.mult,
        cur_wd_data.shift,
    );
    cs_watchdog.last_check = CycleNum(cur_wd_nowclock);
    drop(cs_watchdog);
    let watchdog_list = &mut WATCHDOG_LIST.lock();
    for cs in watchdog_list.iter() {
        let mut cs_data = cs.clocksource_data();
        // 判断时钟源是否已经被标记为不稳定
        if cs_data
            .flags
            .contains(ClocksourceFlags::CLOCK_SOURCE_UNSTABLE)
        {
            // TODO 启动wd thread
            continue;
        }
        // 读时钟源现在的时间
        let cs_now_clock = cs.read();

        // 如果时钟源没有被监视，则开始监视他
        if !cs_data
            .flags
            .contains(ClocksourceFlags::CLOCK_SOURCE_WATCHDOG)
        {
            cs_data
                .flags
                .insert(ClocksourceFlags::CLOCK_SOURCE_WATCHDOG);
            // 记录此次检查的时刻
            cs_data.watchdog_last = cs_now_clock;
            cs.update_clocksource_data(cs_data.clone())?;
            continue;
        }

        // 计算时钟源的误差
        let cs_dev_nsec = clocksource_cyc2ns(
            CycleNum(cs_now_clock.div(cs_data.watchdog_last).data() & cs_data.mask.bits),
            cs_data.mult,
            cs_data.shift,
        );
        // 记录此次检查的时刻
        cs_data.watchdog_last = cs_now_clock;
        cs.update_clocksource_data(cs_data.clone())?;
        // 判断误差大小是否符合要求
        if cs_dev_nsec.abs_diff(wd_dev_nsec) > WATCHDOG_THRESHOLD.into() {
            // 误差过大
            cs.set_unstable((cs_dev_nsec - wd_dev_nsec).try_into().unwrap())?;
            continue;
        }
        // 判断是否要切换为高精度模式
        if !cs_data
            .flags
            .contains(ClocksourceFlags::CLOCK_SOURCE_VALID_FOR_HRES)
            && cs_data
                .flags
                .contains(ClocksourceFlags::CLOCK_SOURCE_IS_CONTINUOUS)
            && cur_wd_data
                .flags
                .contains(ClocksourceFlags::CLOCK_SOURCE_IS_CONTINUOUS)
        {
            cs_data
                .flags
                .insert(ClocksourceFlags::CLOCK_SOURCE_VALID_FOR_HRES);
            cs.update_clocksource_data(cs_data)?;
            // TODO 通知tick机制 切换为高精度模式
        }
        let mut cs_watchdog = CLOCKSOUCE_WATCHDOG.lock();
        // FIXME 需要保证所有cpu时间统一
        cs_watchdog.timer_expires += WATCHDOG_INTERVAL;

        let watchdog_func = Box::new(WatchdogTimerFunc {});
        let watchdog_timer = Timer::new(watchdog_func, cs_watchdog.timer_expires);
        watchdog_timer.activate();
    }
    return Ok(());
}

// TODO 应该放在jiffies.rs
pub const fn sh_div(nom: u32, den: u32, lsh: u32) -> u32 {
    (((nom) / (den)) << (lsh)) + ((((nom) % (den)) << (lsh)) + (den) / 2) / (den)
}
pub struct ClocksourceJiffies(SpinLock<InnerJiffies>);
pub struct InnerJiffies {
    data: ClocksourceData,
    self_ref: Weak<ClocksourceJiffies>,
}

impl Clocksource for ClocksourceJiffies {
    fn read(&self) -> CycleNum {
        CycleNum(clock())
    }

    fn clocksource_data(&self) -> ClocksourceData {
        let inner = self.0.lock();
        return inner.data.clone();
    }

    fn clocksource(&self) -> Arc<dyn Clocksource> {
        self.0.lock().self_ref.upgrade().unwrap()
    }
}
impl ClocksourceJiffies {
    pub fn new() -> Arc<Self> {
        let data = ClocksourceData {
            name: "jiffies".to_string(),
            rating: 1,
            mask: ClocksourceMask { bits: 0xffffffff },
            mult: NSEC_PER_JIFFY << JIFFIES_SHIFT,
            shift: JIFFIES_SHIFT,
            max_idle_ns: Default::default(),
            flags: ClocksourceFlags { bits: 0 },
            watchdog_last: CycleNum(0),
        };
        let jieffies = Arc::new(ClocksourceJiffies(SpinLock::new(InnerJiffies {
            data: data,
            self_ref: Default::default(),
        })));
        jieffies.0.lock().self_ref = Arc::downgrade(&jieffies);

        return jieffies;
    }
}
pub fn clocksource_default_clock() -> Arc<ClocksourceJiffies> {
    let jiffies = ClocksourceJiffies::new();
    return jiffies;
}
