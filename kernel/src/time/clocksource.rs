use core::{
    ffi::c_void,
    intrinsics::log2f32,
    sync::atomic::{AtomicBool, Ordering},
};

use alloc::{boxed::Box, collections::LinkedList, string::String, sync::Arc, vec::Vec};
use lazy_static::__Deref;

use crate::{
    include::bindings::bindings::run_watchdog_kthread, kdebug, kinfo, libs::spinlock::SpinLock,
    syscall::SystemError,
};

use super::{
    jiffies::{clocksource_default_clock, ClocksourceJiffies},
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
pub const HZ: u64 = 1000;
/// watchdog检查间隔
pub const WATCHDOG_INTERVAL: u64 = HZ >> 1;
/// 最大能接受的误差大小
pub const WATCHDOG_THRESHOLD: u32 = NSEC_PER_SEC >> 4;

// 时钟周期数
#[derive(Debug, Clone, Copy)]
#[repr(transparent)]
pub struct CycleNum(pub u64);
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

    /// 启用检查器
    pub fn clocksource_start_watchdog(&mut self) {
        kdebug!("enter clocksource_start_watchdog");
        // let cs_watchdog = &mut CLOCKSOUCE_WATCHDOG.lock();

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
        kdebug!("create a wd_timer");
        self.is_running = true;
        kdebug!("clocksource_start_watchdog");
    }
    /// 停止检查器
    pub fn clocksource_stop_watchdog(&mut self, list_len: usize) {
        kdebug!("enter clocksource_stop_watchdog func");
        // let wd_list = &WATCHDOG_LIST.lock();
        // kdebug!("clocksource_stop_watchdog :WATCHDOG_LIST.lock()");
        if !self.is_running || (self.watchdog.is_some() && list_len != 0) {
            return;
        }
        // TODO 当实现了周期性的定时器后 需要将监视用的定时器删除
        self.is_running = false;
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
// TODO log2 暂放
pub fn log2(x: u32) -> u32 {
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
    // BUG 可能会出现格式转换导致结果错误的问题
    pub fn clocksource_max_deferment(&self) -> u64 {
        let cs_data_guard = self.clocksource_data();
        let max_nsecs: u64;
        let mut max_cycles: u64;
        max_cycles = (1 << (63 - (log2(cs_data_guard.mult) + 1))) as u64;
        max_cycles = max_cycles.min(cs_data_guard.mask.bits);
        max_nsecs = clocksource_cyc2ns(
            CycleNum(max_cycles),
            cs_data_guard.mult,
            cs_data_guard.shift,
        );
        kdebug!("clocksource_max_deferment");
        return max_nsecs - (max_nsecs >> 5);
    }

    pub fn register(&self) -> Result<(), SystemError> {
        let ns = self.clocksource_max_deferment();
        let mut cs_data = self.clocksource_data();

        cs_data.max_idle_ns = ns as u32;
        self.update_clocksource_data(cs_data)?;
        self.clocksource_enqueue();
        self.clocksource_enqueue_watchdog();
        clocksource_select();
        kdebug!("clocksource_register successfully");
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
        kdebug!(
            "CLOCKSOURCE_LIST len = {:?},clocksource_enqueue sccessfully",
            list_guard.len()
        );
    }

    /// 将时间源插入监控队列
    pub fn clocksource_enqueue_watchdog(&self) -> Result<(), SystemError> {
        kdebug!("enter clocksource_enqueue_watchdog");
        // BUG 可能需要lock irq
        let mut cs_data = self.clocksource_data();
        kdebug!("WATCHDOG_LIST.lock_irqsave()");

        let cs = self.clocksource();
        if cs_data
            .flags
            .contains(ClocksourceFlags::CLOCK_SOURCE_MUST_VERIFY)
        {
            let list_guard = &mut WATCHDOG_LIST.lock_irqsave();
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
            kdebug!("CLOCKSOUCE_WATCHDOG.lock()");
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
                clocksource_reset_watchdog();
            }
            cs_watchdog.clocksource_start_watchdog();
            kdebug!("clocksource_start_watchdog successfully");
        }
        kdebug!("clocksource_enqueue_watchdog successfully");
        return Ok(());
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

    /// 将时间源从被监视链表中弹出
    fn clocksource_dequeue_watchdog(&self) {
        let data = self.clocksource_data();
        let locked_watchdog = &mut CLOCKSOUCE_WATCHDOG.lock();
        let watchdog = locked_watchdog
            .get_watchdog()
            .clone()
            .unwrap()
            .clocksource_data();
        drop(locked_watchdog);

        let list = &mut WATCHDOG_LIST.lock();
        // 要删除的时钟源是被监控的
        let size = list.len();
        let mut del_pos: usize = size;
        for (pos, ele) in list.iter().enumerate() {
            let ele_data = ele.clocksource_data();
            if ele_data.name.eq(&data.name) && ele_data.rating.eq(&data.rating) {
                del_pos = pos;
            }
        }
        if data
            .flags
            .contains(ClocksourceFlags::CLOCK_SOURCE_MUST_VERIFY)
        {
            // 删除符合的时钟源
            if del_pos != size {
                let mut temp_list = list.split_off(del_pos);
                temp_list.pop_front();
                list.append(&mut temp_list);
            }
        } else if watchdog.name.eq(&data.name) && watchdog.rating.eq(&data.rating) {
            // 如果要删除的时钟源是监视器，则需要找到一个新的监视器
            // TODO 重新设置时钟源
            // 将链表解锁防止reset中双重加锁 并释放保存的旧的watchdog的数据
            drop(list);
            drop(watchdog);
            clocksource_reset_watchdog();

            let list = &mut WATCHDOG_LIST.lock();
            let mut replace_pos: usize = list.len();
            let locked_watchdog = &mut CLOCKSOUCE_WATCHDOG.lock();

            for (pos, ele) in list.iter().enumerate() {
                let ele_data = ele.clocksource_data();

                if del_pos == pos
                    || ele_data
                        .flags
                        .contains(ClocksourceFlags::CLOCK_SOURCE_MUST_VERIFY)
                {
                    continue;
                }
                let watchdog = locked_watchdog.get_watchdog().clone();
                if watchdog.is_none()
                    || ele_data.rating > watchdog.unwrap().clocksource_data().rating
                {
                    replace_pos = pos;
                }
            }
            // 获取新的watchdog
            let mut temp_list = list.split_off(replace_pos);
            let new_wd = temp_list.front().unwrap().clone();
            list.append(&mut temp_list);
            // 替换watchdog
            locked_watchdog.watchdog.replace(new_wd);
        }
        let mut cs_data = self.clocksource_data();
        cs_data
            .flags
            .remove(ClocksourceFlags::CLOCK_SOURCE_WATCHDOG);
        self.update_clocksource_data(cs_data);
        // TODO 停止当前的watchdog
        CLOCKSOUCE_WATCHDOG.lock().clocksource_stop_watchdog(size-1);
    }

    /// 将时间源从链表中弹出
    fn clocksource_dequeue(&self) {
        let list = &mut CLOCKSOURCE_LIST.lock();
        let data = self.clocksource_data();
        let mut del_pos: usize = list.len();
        for (pos, ele) in list.iter().enumerate() {
            let ele_data = ele.clocksource_data();

            if ele_data.name.eq(&data.name) && ele_data.rating.eq(&data.rating) {
                del_pos = pos;
            }
        }
        // 删除符合的时钟源
        if del_pos != list.len() {
            let mut temp_list = list.split_off(del_pos);
            temp_list.pop_front();
            list.append(&mut temp_list);
        }
    }

    /// 注销一个时间源
    fn clocksource_unregister(&self) {
        self.clocksource_dequeue_watchdog();
        self.clocksource_dequeue();
        clocksource_select();
    }

    fn clocksource_change_rating(&self, rating: i32) {
        self.clocksource_dequeue();
        let mut data = self.clocksource_data();
        data.set_rating(rating);
        self.update_clocksource_data(data)
            .expect("clocksource_change_rating:updata clocksource failed");
        self.clocksource_enqueue();
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

/// 将所有的时间源重启
pub fn clocksource_resume() {
    let list = CLOCKSOURCE_LIST.lock();
    for ele in list.iter() {
        let data = ele.clocksource_data();
        match ele.resume() {
            Ok(_) => continue,
            Err(e) => {
                kdebug!("clocksource {:?} resume failed", data.name)
            }
        }
    }
    clocksource_resume_watchdog();
}

/// 将所有的时间源暂停
pub fn clocksource_suspend() {
    let list = CLOCKSOURCE_LIST.lock();
    for ele in list.iter() {
        let data = ele.clocksource_data();
        match ele.suspend() {
            Ok(_) => continue,
            Err(e) => {
                kdebug!("clocksource {:?} suspend failed", data.name)
            }
        }
    }
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
            unsafe { run_watchdog_kthread() };

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

/// watchdog线程的逻辑
pub fn clocksource_watchdog_kthread() {
    // 将unstable的时钟源都从监视链表移除
    let mut del_vec: Vec<usize> = Vec::new();
    let mut del_clocks: Vec<Arc<dyn Clocksource>> = Vec::new();
    let wd_list = &mut WATCHDOG_LIST.lock();
    kdebug!("WATCHDOG_LIST.lock()");
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
    // drop(wd_list);
    // kdebug!("ready to lock watchdog");
    // 检查是否需要停止watchdog
    // BUG 双重加锁
    CLOCKSOUCE_WATCHDOG
        .lock()
        .clocksource_stop_watchdog(wd_list.len());
    kdebug!("clocksource_stop_watchdog finished");
    // 将不稳定的时钟源精度都设置为最低
    for clock in del_clocks.iter() {
        clock.clocksource_change_rating(0);
    }
}

/// 将所有被监视的时间源设置为未被监视
pub fn clocksource_reset_watchdog() {
    let list_guard = &mut WATCHDOG_LIST.lock();
    for ele in list_guard.iter() {
        ele.clocksource_data()
            .flags
            .remove(ClocksourceFlags::CLOCK_SOURCE_WATCHDOG);
    }
    kdebug!("clocksource_reset_watchdog successfully");
}

/// 重启检查器
pub fn clocksource_resume_watchdog() {
    clocksource_reset_watchdog();
}

/// 根据精度选择最优的时钟源，或者接受用户指定的时间源
pub fn clocksource_select() {
    let list_guard = &mut CLOCKSOURCE_LIST.lock();
    if unsafe { FINISHED_BOOTING.load(Ordering::Relaxed) } || list_guard.is_empty() {
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
    if CUR_CLOCKSOURCE.lock().as_ref().is_some() {
        let cur_clocksource = CUR_CLOCKSOURCE.lock().as_ref().unwrap().clone();
        let best_name = &best.clocksource_data().name;
        if cur_clocksource.clocksource_data().name.ne(best_name) {
            kinfo!("Switching to clocksource {:?}\n", best_name);
            drop(cur_clocksource);
            CUR_CLOCKSOURCE.lock().replace(best);
            // TODO 通知timerkeeping 切换了时间源
        }
    } else {
        CUR_CLOCKSOURCE.lock().replace(best);
    }
}

/// clocksource模块加载完成
pub fn clocksource_boot_finish() {
    let cur_clocksource = &mut CUR_CLOCKSOURCE.lock();
    cur_clocksource.replace(clocksource_default_clock());
    // *unsafe { FINISHED_BOOTING.get_mut() } = true;
    unsafe { FINISHED_BOOTING.store(true, Ordering::Relaxed) };
    // 清除不稳定的时钟源
    clocksource_watchdog_kthread();
    kdebug!("clocksource_boot_finish");
}

#[no_mangle]
pub extern "C" fn rs_clocksource_boot_finish() {
    clocksource_boot_finish();
}
#[no_mangle]
pub extern "C" fn rs_clocksource_watchdog_kthread(_data: c_void) -> i32 {
    clocksource_watchdog_kthread();
    return 0;
}
