use core::{
    fmt::Debug,
    sync::atomic::{AtomicBool, Ordering},
};

use alloc::{
    boxed::Box,
    collections::LinkedList,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use lazy_static::__Deref;
use log::{debug, info};
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    arch::CurrentIrqArch,
    exception::InterruptArch,
    init::initcall::INITCALL_LATE,
    libs::spinlock::SpinLock,
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessControlBlock, ProcessManager,
    },
    sched::{schedule, SchedMode},
};

use super::{
    jiffies::clocksource_default_clock,
    timer::{clock, Timer, TimerFunction},
    NSEC_PER_SEC, NSEC_PER_USEC,
};

lazy_static! {
    /// linked list with the registered clocksources
    pub static ref CLOCKSOURCE_LIST: SpinLock<LinkedList<Arc<dyn Clocksource>>> =
        SpinLock::new(LinkedList::new());
    /// 被监视中的时钟源
    pub static ref WATCHDOG_LIST: SpinLock<LinkedList<Arc<dyn Clocksource>>> =
        SpinLock::new(LinkedList::new());

    pub static ref CLOCKSOURCE_WATCHDOG:SpinLock<ClocksouceWatchdog>  = SpinLock::new(ClocksouceWatchdog::new());

    pub static ref OVERRIDE_NAME: SpinLock<String> = SpinLock::new(String::from(""));


}

static mut WATCHDOG_KTHREAD: Option<Arc<ProcessControlBlock>> = None;

/// 正在被使用时钟源
pub static CUR_CLOCKSOURCE: SpinLock<Option<Arc<dyn Clocksource>>> = SpinLock::new(None);
/// 是否完成加载
pub static FINISHED_BOOTING: AtomicBool = AtomicBool::new(false);

/// Interval: 0.5sec Threshold: 0.0625s
/// 系统节拍率
pub const HZ: u64 = 250;
// 参考：https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/time/clocksource.c#101
/// watchdog检查间隔
pub const WATCHDOG_INTERVAL: u64 = HZ >> 1;
// 参考：https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/time/clocksource.c#108
/// 最大能接受的误差大小
pub const WATCHDOG_THRESHOLD: u32 = NSEC_PER_SEC >> 4;

pub const MAX_SKEW_USEC: u64 = 125 * WATCHDOG_INTERVAL / HZ;
pub const WATCHDOG_MAX_SKEW: u32 = MAX_SKEW_USEC as u32 * NSEC_PER_USEC;

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
pub struct ClocksouceWatchdog {
    /// 监视器
    watchdog: Option<Arc<dyn Clocksource>>,
    /// 检查器是否在工作的标志
    is_running: bool,
    /// 定时监视器的过期时间
    timer_expires: u64,
}
impl ClocksouceWatchdog {
    pub fn new() -> Self {
        Self {
            watchdog: None,
            is_running: false,
            timer_expires: 0,
        }
    }

    /// 获取watchdog
    fn get_watchdog(&mut self) -> &mut Option<Arc<dyn Clocksource>> {
        &mut self.watchdog
    }

    /// 启用检查器
    pub fn clocksource_start_watchdog(&mut self) {
        // 如果watchdog未被设置或者已经启用了就退出
        let watchdog_list = WATCHDOG_LIST.lock_irqsave();
        if self.is_running || self.watchdog.is_none() || watchdog_list.is_empty() {
            return;
        }
        // 生成一个定时器
        let wd_timer_func: Box<WatchdogTimerFunc> = Box::new(WatchdogTimerFunc {});
        self.timer_expires += clock() + WATCHDOG_INTERVAL;
        let mut wd_data = self.watchdog.as_ref().unwrap().clone().clocksource_data();
        wd_data.watchdog_last = self.watchdog.as_ref().unwrap().clone().read();
        self.watchdog
            .as_ref()
            .unwrap()
            .update_clocksource_data(wd_data)
            .expect("clocksource_start_watchdog: failed to update watchdog data");
        let wd_timer = Timer::new(wd_timer_func, self.timer_expires);
        wd_timer.activate();
        self.is_running = true;
    }

    /// 停止检查器
    /// list_len WATCHDOG_LIST长度
    pub fn clocksource_stop_watchdog(&mut self, list_len: usize) {
        if !self.is_running || (self.watchdog.is_some() && list_len != 0) {
            return;
        }
        // TODO 当实现了周期性的定时器后 需要将监视用的定时器删除
        self.is_running = false;
    }
}

/// 定时检查器
#[derive(Debug)]
pub struct WatchdogTimerFunc;
impl TimerFunction for WatchdogTimerFunc {
    fn run(&mut self) -> Result<(), SystemError> {
        return clocksource_watchdog();
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

    fn update_clocksource_data(&self, _data: ClocksourceData) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }
    // 获取时钟源
    fn clocksource(&self) -> Arc<dyn Clocksource>;
}

/// # 实现整数log2的运算
///
/// ## 参数
///
/// * `x` - 要计算的数字
///
/// ## 返回值
///
/// * `u32` - 返回\log_2(x)的值
fn log2(x: u32) -> u32 {
    let mut result = 0;
    let mut x = x;

    if x >= 1 << 16 {
        x >>= 16;
        result |= 16;
    }
    if x >= 1 << 8 {
        x >>= 8;
        result |= 8;
    }
    if x >= 1 << 4 {
        x >>= 4;
        result |= 4;
    }
    if x >= 1 << 2 {
        x >>= 2;
        result |= 2;
    }
    if x >= 1 << 1 {
        result |= 1;
    }

    result
}

impl dyn Clocksource {
    /// # 计算时钟源能记录的最大时间跨度
    pub fn clocksource_max_deferment(&self) -> u64 {
        let cs_data_guard = self.clocksource_data();

        let mut max_cycles: u64;
        max_cycles = (1 << (63 - (log2(cs_data_guard.mult + cs_data_guard.maxadj) + 1))) as u64;
        max_cycles = max_cycles.min(cs_data_guard.mask.bits);
        let max_nsecs = clocksource_cyc2ns(
            CycleNum(max_cycles),
            cs_data_guard.mult - cs_data_guard.maxadj,
            cs_data_guard.shift,
        );
        return max_nsecs - (max_nsecs >> 3);
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
        if freq != 0 {
            let mut cs_data = self.clocksource_data();
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
            cs_data.set_mult(mult);
            cs_data.set_shift(shift);
            self.update_clocksource_data(cs_data)?;
        }

        let mut cs_data = self.clocksource_data();
        if scale != 0 && freq != 0 && cs_data.uncertainty_margin == 0 {
            cs_data.set_uncertainty_margin(NSEC_PER_SEC / (scale * freq));
            if cs_data.uncertainty_margin < 2 * WATCHDOG_MAX_SKEW {
                cs_data.set_uncertainty_margin(2 * WATCHDOG_MAX_SKEW);
            }
        } else if cs_data.uncertainty_margin == 0 {
            cs_data.set_uncertainty_margin(WATCHDOG_THRESHOLD);
        }

        // 确保时钟源没有太大的mult值造成溢出
        cs_data.set_maxadj(self.clocksource_max_adjustment());
        self.update_clocksource_data(cs_data)?;
        while freq != 0
            && (self.clocksource_data().mult + self.clocksource_data().maxadj
                < self.clocksource_data().mult
                || self.clocksource_data().mult - self.clocksource_data().maxadj
                    > self.clocksource_data().mult)
        {
            let mut cs_data = self.clocksource_data();
            cs_data.set_mult(cs_data.mult >> 1);
            cs_data.set_shift(cs_data.shift - 1);
            self.update_clocksource_data(cs_data)?;
            let mut cs_data = self.clocksource_data();
            cs_data.set_maxadj(self.clocksource_max_adjustment());
            self.update_clocksource_data(cs_data)?;
        }

        let mut cs_data = self.clocksource_data();
        let ns = self.clocksource_max_deferment();
        cs_data.set_max_idle_ns(ns as u32);
        self.update_clocksource_data(cs_data)?;

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
        self.clocksource_update_freq_scale(scale, freq)?;

        // 将时钟源加入到时钟源队列中
        self.clocksource_enqueue();
        // 将时钟源加入到监视队列中
        self.clocksource_enqueue_watchdog()
            .expect("register: failed to enqueue watchdog list");
        // 选择一个最好的时钟源
        clocksource_select();
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
        // BUG 可能需要lock irq
        let mut cs_data = self.clocksource_data();

        let cs = self.clocksource();
        if cs_data
            .flags
            .contains(ClocksourceFlags::CLOCK_SOURCE_MUST_VERIFY)
        {
            let mut list_guard = WATCHDOG_LIST.lock_irqsave();
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
                // 如果时钟设备是连续的
                cs_data
                    .flags
                    .insert(ClocksourceFlags::CLOCK_SOURCE_VALID_FOR_HRES);
                cs.update_clocksource_data(cs_data.clone())?;
            }

            // 将时钟源加入到监控队列中
            let mut list_guard = WATCHDOG_LIST.lock_irqsave();
            list_guard.push_back(cs.clone());
            drop(list_guard);

            // 对比当前注册的时间源的精度和监视器的精度
            let mut cs_watchdog = CLOCKSOURCE_WATCHDOG.lock_irqsave();
            if cs_watchdog.watchdog.is_none()
                || cs_data.rating
                    > cs_watchdog
                        .watchdog
                        .clone()
                        .unwrap()
                        .clocksource_data()
                        .rating
            {
                // 当前注册的时间源的精度更高或者没有监视器，替换监视器
                cs_watchdog.watchdog.replace(cs);
                clocksource_reset_watchdog();
            }

            // 启动监视器
            cs_watchdog.clocksource_start_watchdog();
        }
        return Ok(0);
    }

    /// # 将时钟源标记为unstable
    ///
    /// ## 参数
    /// * `delta` - 时钟源误差
    pub fn set_unstable(&self, delta: i64) -> Result<i32, SystemError> {
        let mut cs_data = self.clocksource_data();
        // 打印出unstable的时钟源信息
        debug!(
            "clocksource :{:?} is unstable, its delta is {:?}",
            cs_data.name, delta
        );
        cs_data.flags.remove(
            ClocksourceFlags::CLOCK_SOURCE_VALID_FOR_HRES | ClocksourceFlags::CLOCK_SOURCE_WATCHDOG,
        );
        cs_data
            .flags
            .insert(ClocksourceFlags::CLOCK_SOURCE_UNSTABLE);
        self.update_clocksource_data(cs_data)?;

        // 启动watchdog线程 进行后续处理
        if FINISHED_BOOTING.load(Ordering::Relaxed) {
            // TODO 在实现了工作队列后，将启动线程换成schedule work
            run_watchdog_kthread();
        }
        return Ok(0);
    }

    /// # 将时间源从监视链表中弹出
    fn clocksource_dequeue_watchdog(&self) {
        let data = self.clocksource_data();
        let mut locked_watchdog = CLOCKSOURCE_WATCHDOG.lock_irqsave();
        let watchdog = locked_watchdog
            .get_watchdog()
            .clone()
            .unwrap()
            .clocksource_data();

        let mut list = WATCHDOG_LIST.lock_irqsave();
        let mut size = list.len();

        let mut del_pos: usize = size;
        for (pos, ele) in list.iter().enumerate() {
            let ele_data = ele.clocksource_data();
            if ele_data.name.eq(&data.name) && ele_data.rating.eq(&data.rating) {
                // 记录要删除的时钟源在监视链表中的下标
                del_pos = pos;
            }
        }

        if data
            .flags
            .contains(ClocksourceFlags::CLOCK_SOURCE_MUST_VERIFY)
        {
            // 如果时钟源是需要被检查的，直接删除时钟源
            if del_pos != size {
                let mut temp_list = list.split_off(del_pos);
                temp_list.pop_front();
                list.append(&mut temp_list);
            }
        } else if watchdog.name.eq(&data.name) && watchdog.rating.eq(&data.rating) {
            // 如果要删除的时钟源是监视器，则需要找到一个新的监视器
            // TODO 重新设置时钟源
            // 将链表解锁防止reset中双重加锁 并释放保存的旧的watchdog的数据

            // 代替了clocksource_reset_watchdog()的功能，将所有时钟源的watchdog标记清除
            for ele in list.iter() {
                ele.clocksource_data()
                    .flags
                    .remove(ClocksourceFlags::CLOCK_SOURCE_WATCHDOG);
            }

            // 遍历所有时间源，寻找新的监视器
            let mut clocksource_list = CLOCKSOURCE_LIST.lock();
            let mut replace_pos: usize = clocksource_list.len();
            for (pos, ele) in clocksource_list.iter().enumerate() {
                let ele_data = ele.clocksource_data();

                if ele_data.name.eq(&data.name) && ele_data.rating.eq(&data.rating)
                    || ele_data
                        .flags
                        .contains(ClocksourceFlags::CLOCK_SOURCE_MUST_VERIFY)
                {
                    // 当前时钟源是要被删除的时钟源或没被检查过的时钟源
                    // 不适合成为监视器
                    continue;
                }
                let watchdog = locked_watchdog.get_watchdog().clone();
                if watchdog.is_none()
                    || ele_data.rating > watchdog.unwrap().clocksource_data().rating
                {
                    // 如果watchdog不存在或者当前时钟源的精度高于watchdog的精度，则记录当前时钟源的下标
                    replace_pos = pos;
                }
            }
            // 使用刚刚找到的更好的时钟源替换旧的watchdog
            if replace_pos < clocksource_list.len() {
                let mut temp_list = clocksource_list.split_off(replace_pos);
                let new_wd = temp_list.front().unwrap().clone();
                clocksource_list.append(&mut temp_list);
                // 替换watchdog
                locked_watchdog.watchdog.replace(new_wd);
                // drop(locked_watchdog);
            }
            // 删除时钟源
            if del_pos != size {
                let mut temp_list = list.split_off(del_pos);
                temp_list.pop_front();
                list.append(&mut temp_list);
            }
        }

        // 清除watchdog标记
        let mut cs_data = self.clocksource_data();
        cs_data
            .flags
            .remove(ClocksourceFlags::CLOCK_SOURCE_WATCHDOG);
        self.update_clocksource_data(cs_data)
            .expect("clocksource_dequeue_watchdog: failed to update clocksource data");
        size = list.len();
        // 停止当前的watchdog
        locked_watchdog.clocksource_stop_watchdog(size - 1);
    }

    /// # 将时钟源从时钟源链表中弹出
    fn clocksource_dequeue(&self) {
        let mut list = CLOCKSOURCE_LIST.lock();
        let data = self.clocksource_data();
        let mut del_pos: usize = list.len();
        for (pos, ele) in list.iter().enumerate() {
            let ele_data = ele.clocksource_data();
            if ele_data.name.eq(&data.name) && ele_data.rating.eq(&data.rating) {
                // 记录时钟源在链表中的下标
                del_pos = pos;
            }
        }

        // 删除时钟源
        if del_pos != list.len() {
            let mut temp_list = list.split_off(del_pos);
            temp_list.pop_front();
            list.append(&mut temp_list);
        }
    }

    /// # 注销时钟源
    #[allow(dead_code)]
    pub fn unregister(&self) {
        // 将时钟源从监视链表中弹出
        self.clocksource_dequeue_watchdog();
        // 将时钟源从时钟源链表中弹出
        self.clocksource_dequeue();
        // 检查是否有更好的时钟源
        clocksource_select();
    }
    /// # 修改时钟源的精度
    ///
    /// ## 参数
    ///
    /// * `rating` - 指定的时钟精度
    fn clocksource_change_rating(&self, rating: i32) {
        // 将时钟源从链表中弹出
        self.clocksource_dequeue();
        let mut data = self.clocksource_data();
        // 修改时钟源的精度
        data.set_rating(rating);
        self.update_clocksource_data(data)
            .expect("clocksource_change_rating:updata clocksource failed");
        // 插入时钟源到时钟源链表中
        self.clocksource_enqueue();
        // 检查是否有更好的时钟源
        clocksource_select();
    }
}

#[derive(Debug, Clone)]
pub struct ClocksourceData {
    /// 时钟源名字
    pub name: String,
    /// 时钟精度
    pub rating: i32,
    pub mask: ClocksourceMask,
    pub mult: u32,
    pub shift: u32,
    pub max_idle_ns: u32,
    pub flags: ClocksourceFlags,
    pub watchdog_last: CycleNum,
    /// 用于watchdog机制中的字段，记录主时钟源上一次被读取的周期数
    pub cs_last: CycleNum,
    // 用于描述时钟源的不确定性边界，时钟源读取的时间可能存在的不确定性和误差范围
    pub uncertainty_margin: u32,
    // 最大的时间调整量
    pub maxadj: u32,
    /// 上一次读取时钟源时的周期数
    pub cycle_last: CycleNum,
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
        max_idle_ns: u32,
        flags: ClocksourceFlags,
        uncertainty_margin: u32,
        maxadj: u32,
    ) -> Self {
        let csd = ClocksourceData {
            name,
            rating,
            mask,
            mult,
            shift,
            max_idle_ns,
            flags,
            watchdog_last: CycleNum(0),
            cs_last: CycleNum(0),
            uncertainty_margin,
            maxadj,
            cycle_last: CycleNum(0),
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
    #[allow(dead_code)]
    pub fn remove_flags(&mut self, flags: ClocksourceFlags) {
        self.flags.remove(flags)
    }
    #[allow(dead_code)]
    pub fn insert_flags(&mut self, flags: ClocksourceFlags) {
        self.flags.insert(flags)
    }
    pub fn set_uncertainty_margin(&mut self, uncertainty_margin: u32) {
        self.uncertainty_margin = uncertainty_margin;
    }
    pub fn set_maxadj(&mut self, maxadj: u32) {
        self.maxadj = maxadj;
    }
}

///  converts clocksource cycles to nanoseconds
///
pub fn clocksource_cyc2ns(cycles: CycleNum, mult: u32, shift: u32) -> u64 {
    // info!("<clocksource_cyc2ns>");
    // info!("cycles = {:?}, mult = {:?}, shift = {:?}", cycles, mult, shift);
    // info!("ret = {:?}", (cycles.data() * mult as u64) >> shift);
    return (cycles.data() * mult as u64) >> shift;
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

/// # 根据watchdog的精度，来检查被监视的时钟源的误差
///
/// ## 返回值
///
/// * `Ok()` - 检查完成
/// * `Err(SystemError)` - 错误码
pub fn clocksource_watchdog() -> Result<(), SystemError> {
    let cs_watchdog = CLOCKSOURCE_WATCHDOG.lock_irqsave();
    // debug!("clocksource_watchdog start");

    // watchdog没有在运行的话直接退出
    if !cs_watchdog.is_running || cs_watchdog.watchdog.is_none() {
        // debug!("is_running = {:?},watchdog = {:?}", cs_watchdog.is_running, cs_watchdog.watchdog);
        return Ok(());
    }

    drop(cs_watchdog);
    let watchdog_list = WATCHDOG_LIST.lock_irqsave();
    for cs in watchdog_list.iter() {
        let mut cs_data = cs.clocksource_data();
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
        let wd = CLOCKSOURCE_WATCHDOG.lock_irqsave();
        let wd_now = wd.watchdog.as_ref().unwrap().clone();
        let wd_now_data = wd_now.as_ref().clocksource_data();
        let wd_now_clock = wd_now.as_ref().read().data();

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
            cs_data
                .flags
                .insert(ClocksourceFlags::CLOCK_SOURCE_WATCHDOG);
            // 记录此次检查的时刻
            cs_data.watchdog_last = CycleNum::new(wd_now_clock);
            cs_data.cs_last = cs_now_clock;
            cs.update_clocksource_data(cs_data.clone())?;
            continue;
        }

        let wd_dev_nsec = clocksource_cyc2ns(
            CycleNum((wd_now_clock - cs_data.watchdog_last.data()) & wd_now_data.mask.bits),
            wd_now_data.mult,
            wd_now_data.shift,
        );

        let cs_dev_nsec = clocksource_cyc2ns(
            CycleNum(cs_now_clock.div(cs_data.cs_last).data() & cs_data.mask.bits),
            cs_data.mult,  // 2343484437
            cs_data.shift, // 23
        );
        // 记录此次检查的时刻
        cs_data.watchdog_last = CycleNum::new(wd_now_clock);
        cs_data.cs_last = cs_now_clock;
        cs.update_clocksource_data(cs_data.clone())?;

        // 判断是否有误差
        if cs_dev_nsec.abs_diff(wd_dev_nsec) > WATCHDOG_THRESHOLD.into() {
            // debug!("set_unstable");
            // 误差过大，标记为unstable
            info!("cs_dev_nsec = {}", cs_dev_nsec);
            info!("wd_dev_nsec = {}", wd_dev_nsec);
            cs.set_unstable(cs_dev_nsec.abs_diff(wd_dev_nsec).try_into().unwrap())?;
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
            cs_data
                .flags
                .insert(ClocksourceFlags::CLOCK_SOURCE_VALID_FOR_HRES);
            cs.update_clocksource_data(cs_data)?;
            // TODO 通知tick机制 切换为高精度模式
        }
    }
    create_new_watchdog_timer_function();
    return Ok(());
}

fn create_new_watchdog_timer_function() {
    let mut cs_watchdog = CLOCKSOURCE_WATCHDOG.lock_irqsave();

    cs_watchdog.timer_expires += WATCHDOG_INTERVAL;
    //创建定时器执行watchdog
    let watchdog_func = Box::new(WatchdogTimerFunc {});
    let watchdog_timer = Timer::new(watchdog_func, cs_watchdog.timer_expires);
    watchdog_timer.activate();
}

fn __clocksource_watchdog_kthread() {
    let mut del_vec: Vec<usize> = Vec::new();
    let mut del_clocks: Vec<Arc<dyn Clocksource>> = Vec::new();
    let mut wd_list = WATCHDOG_LIST.lock_irqsave();

    // 将不稳定的时钟源弹出监视链表
    for (pos, ele) in wd_list.iter().enumerate() {
        let data = ele.clocksource_data();
        if data.flags.contains(ClocksourceFlags::CLOCK_SOURCE_UNSTABLE) {
            del_vec.push(pos);
            del_clocks.push(ele.clone());
        }
    }
    for pos in del_vec {
        let mut temp_list = wd_list.split_off(pos);
        temp_list.pop_front();
        wd_list.append(&mut temp_list);
    }

    // 检查是否需要停止watchdog
    CLOCKSOURCE_WATCHDOG
        .lock_irqsave()
        .clocksource_stop_watchdog(wd_list.len());
    drop(wd_list);
    // 将不稳定的时钟源精度都设置为最低，然后删除unstable标记
    for clock in del_clocks.iter() {
        clock.clocksource_change_rating(0);
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
    let list_guard = WATCHDOG_LIST.lock_irqsave();
    for ele in list_guard.iter() {
        ele.clocksource_data()
            .flags
            .remove(ClocksourceFlags::CLOCK_SOURCE_WATCHDOG);
    }
}

/// # 重启检查器
pub fn clocksource_resume_watchdog() {
    clocksource_reset_watchdog();
}

/// # 根据精度选择最优的时钟源，或者接受用户指定的时间源
pub fn clocksource_select() {
    let list_guard = CLOCKSOURCE_LIST.lock();
    if !FINISHED_BOOTING.load(Ordering::Relaxed) || list_guard.is_empty() {
        return;
    }
    let mut best = list_guard.front().unwrap().clone();
    let override_name = OVERRIDE_NAME.lock();
    // 判断是否有用户空间指定的时间源
    for ele in list_guard.iter() {
        if ele.clocksource_data().name.eq(override_name.deref()) {
            // TODO 判断是否是高精度模式
            // 暂时不支持高精度模式
            // 如果是高精度模式，但是时钟源不支持高精度模式的话，就要退出循环
            best = ele.clone();
            break;
        }
    }
    // 对比当前的时钟源和记录到最好的时钟源的精度
    if CUR_CLOCKSOURCE.lock().as_ref().is_some() {
        // 当前时钟源不为空
        let cur_clocksource = CUR_CLOCKSOURCE.lock().as_ref().unwrap().clone();
        let best_name = &best.clocksource_data().name;
        if cur_clocksource.clocksource_data().name.ne(best_name) {
            info!("Switching to the clocksource {:?}\n", best_name);
            drop(cur_clocksource);
            CUR_CLOCKSOURCE.lock().replace(best.clone());
            // TODO 通知timerkeeping 切换了时间源
        }
    } else {
        // 当前时钟源为空
        CUR_CLOCKSOURCE.lock().replace(best.clone());
    }
    debug!("clocksource_select finish, CUR_CLOCKSOURCE = {best:?}");
}

/// # clocksource模块加载完成
pub fn clocksource_boot_finish() {
    let mut cur_clocksource = CUR_CLOCKSOURCE.lock();
    cur_clocksource.replace(clocksource_default_clock());
    FINISHED_BOOTING.store(true, Ordering::Relaxed);
    // 清除不稳定的时钟源
    __clocksource_watchdog_kthread();
    debug!("clocksource_boot_finish");
}

fn run_watchdog_kthread() {
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
