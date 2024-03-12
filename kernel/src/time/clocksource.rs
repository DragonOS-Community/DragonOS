use core::{
    ffi::c_void,
    fmt::Debug,
    sync::atomic::{AtomicBool, Ordering},
};

use alloc::{boxed::Box, collections::LinkedList, string::String, sync::Arc, vec::Vec};
use lazy_static::__Deref;
use system_error::SystemError;

use crate::{
    include::bindings::bindings::run_watchdog_kthread, kdebug, kinfo, libs::spinlock::SpinLock,
};

use super::{
    jiffies::clocksource_default_clock,
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

/// 正在被使用时钟源
pub static CUR_CLOCKSOURCE: SpinLock<Option<Arc<dyn Clocksource>>> = SpinLock::new(None);
/// 是否完成加载
pub static mut FINISHED_BOOTING: AtomicBool = AtomicBool::new(false);

/// Interval: 0.5sec Threshold: 0.0625s
/// 系统节拍率
pub const HZ: u64 = 250;
/// watchdog检查间隔
pub const WATCHDOG_INTERVAL: u64 = HZ >> 1;
/// 最大能接受的误差大小
pub const WATCHDOG_THRESHOLD: u32 = NSEC_PER_SEC >> 4;

// 时钟周期数
#[derive(Debug, Clone, Copy)]
#[repr(transparent)]
pub struct CycleNum(pub u64);

#[allow(dead_code)]
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

    /// 获取watchdog
    fn get_watchdog(&mut self) -> &mut Option<Arc<dyn Clocksource>> {
        &mut self.watchdog
    }

    /// 启用检查器
    pub fn clocksource_start_watchdog(&mut self) {
        // 如果watchdog未被设置或者已经启用了就退出
        let watchdog_list = &WATCHDOG_LIST.lock();
        if self.is_running || self.watchdog.is_none() || watchdog_list.is_empty() {
            return;
        }
        // 生成一个定时器
        let wd_timer_func: Box<WatchdogTimerFunc> = Box::new(WatchdogTimerFunc {});
        self.timer_expires += clock() + WATCHDOG_INTERVAL;
        self.last_check = self.watchdog.as_ref().unwrap().clone().read();
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
        max_cycles = (1 << (63 - (log2(cs_data_guard.mult) + 1))) as u64;
        max_cycles = max_cycles.min(cs_data_guard.mask.bits);
        let max_nsecs = clocksource_cyc2ns(
            CycleNum(max_cycles),
            cs_data_guard.mult,
            cs_data_guard.shift,
        );
        return max_nsecs - (max_nsecs >> 5);
    }

    /// # 注册时钟源
    ///
    /// ## 返回值
    ///
    /// * `Ok(0)` - 时钟源注册成功。
    /// * `Err(SystemError)` - 时钟源注册失败。
    pub fn register(&self) -> Result<i32, SystemError> {
        let ns = self.clocksource_max_deferment();
        let mut cs_data = self.clocksource_data();
        cs_data.max_idle_ns = ns as u32;
        self.update_clocksource_data(cs_data)?;
        // 将时钟源加入到时钟源队列中
        self.clocksource_enqueue();
        // 将时钟源加入到监视队列中
        self.clocksource_enqueue_watchdog()
            .expect("register: failed to enqueue watchdog list");
        // 选择一个最好的时钟源
        clocksource_select();
        kdebug!("clocksource_register successfully");
        return Ok(0);
    }

    /// # 将时钟源插入时钟源队列
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
        // kdebug!(
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
            let mut list_guard = WATCHDOG_LIST.lock();
            list_guard.push_back(cs.clone());
            drop(list_guard);

            // 对比当前注册的时间源的精度和监视器的精度
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

        // 启动watchdog线程 进行后续处理
        if unsafe { FINISHED_BOOTING.load(Ordering::Relaxed) } {
            // TODO 在实现了工作队列后，将启动线程换成schedule work
            unsafe { run_watchdog_kthread() }
        }
        return Ok(0);
    }

    /// # 将时间源从监视链表中弹出
    fn clocksource_dequeue_watchdog(&self) {
        let data = self.clocksource_data();
        let mut locked_watchdog = CLOCKSOUCE_WATCHDOG.lock();
        let watchdog = locked_watchdog
            .get_watchdog()
            .clone()
            .unwrap()
            .clocksource_data();

        let mut list = WATCHDOG_LIST.lock();
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
}

impl ClocksourceData {
    #[allow(dead_code)]
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
            name,
            rating,
            mask,
            mult,
            shift,
            max_idle_ns,
            flags,
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
    #[allow(dead_code)]
    pub fn remove_flags(&mut self, flags: ClocksourceFlags) {
        self.flags.remove(flags)
    }
    #[allow(dead_code)]
    pub fn insert_flags(&mut self, flags: ClocksourceFlags) {
        self.flags.insert(flags)
    }
}

///  converts clocksource cycles to nanoseconds
///
pub fn clocksource_cyc2ns(cycles: CycleNum, mult: u32, shift: u32) -> u64 {
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
                kdebug!("clocksource {:?} resume failed", data.name);
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
                kdebug!("clocksource {:?} suspend failed", data.name);
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
    let mut cs_watchdog = CLOCKSOUCE_WATCHDOG.lock();

    // watchdog没有在运行的话直接退出
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
            // 启动watchdog_kthread
            unsafe { run_watchdog_kthread() };
            continue;
        }
        // 读取时钟源现在的时间
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
        if cs_dev_nsec.abs_diff(wd_dev_nsec) > WATCHDOG_THRESHOLD.into() {
            // 误差过大，标记为unstable
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
        //创建定时器执行watchdog
        let watchdog_func = Box::new(WatchdogTimerFunc {});
        let watchdog_timer = Timer::new(watchdog_func, cs_watchdog.timer_expires);
        watchdog_timer.activate();
    }
    return Ok(());
}

/// # watchdog线程的逻辑，执行unstable的后续操作
pub fn clocksource_watchdog_kthread() {
    let mut del_vec: Vec<usize> = Vec::new();
    let mut del_clocks: Vec<Arc<dyn Clocksource>> = Vec::new();
    let wd_list = &mut WATCHDOG_LIST.lock();

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
    CLOCKSOUCE_WATCHDOG
        .lock()
        .clocksource_stop_watchdog(wd_list.len());
    // 将不稳定的时钟源精度都设置为最低
    for clock in del_clocks.iter() {
        clock.clocksource_change_rating(0);
    }
}

/// # 清空所有时钟源的watchdog标志位
pub fn clocksource_reset_watchdog() {
    let list_guard = WATCHDOG_LIST.lock();
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
    if unsafe { FINISHED_BOOTING.load(Ordering::Relaxed) } || list_guard.is_empty() {
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
            kinfo!("Switching to the clocksource {:?}\n", best_name);
            drop(cur_clocksource);
            CUR_CLOCKSOURCE.lock().replace(best);
            // TODO 通知timerkeeping 切换了时间源
        }
    } else {
        // 当前时钟源为空
        CUR_CLOCKSOURCE.lock().replace(best);
    }
    kdebug!(" clocksource_select finish");
}

/// # clocksource模块加载完成
pub fn clocksource_boot_finish() {
    let mut cur_clocksource = CUR_CLOCKSOURCE.lock();
    cur_clocksource.replace(clocksource_default_clock());
    unsafe { FINISHED_BOOTING.store(true, Ordering::Relaxed) };
    // 清除不稳定的时钟源
    clocksource_watchdog_kthread();
    kdebug!("clocksource_boot_finish");
}

// ======== 以下为对C的接口 ========

/// # 启动watchdog线程的辅助函数
#[no_mangle]
pub extern "C" fn rs_clocksource_watchdog_kthread(_data: c_void) -> i32 {
    clocksource_watchdog_kthread();
    return 0;
}
