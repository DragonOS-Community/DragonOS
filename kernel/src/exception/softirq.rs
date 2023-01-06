use alloc::boxed::Box;
use core::ffi::c_void;
use core::ptr::null_mut;

use crate::arch::interrupt::cli;
use crate::arch::interrupt::sti;
use crate::include::bindings::bindings::verify_area;
use crate::include::bindings::bindings::EBUSY;
use crate::include::bindings::bindings::EEXIST;
use crate::include::bindings::bindings::EPERM;
use crate::kBUG;
use crate::libs::spinlock::RawSpinlock;

const MAX_SOFTIRQ_NUM: u64 = 64;
const MAX_UNREGISTER_TRIAL_TIME: u64 = 50;

/// not used until softirq.h is removed
pub enum SirqParam {
    TIMER_SIRQ = 0,         //时钟软中断信号
    VIDEO_REFRESH_SIRQ = 1, //帧缓冲区刷新软中断
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SoftirqVector {
    pub action: Option<unsafe extern "C" fn(data: *mut ::core::ffi::c_void)>, //软中断处理函数
    pub data: *mut c_void,
}

impl Default for SoftirqVector {
    fn default() -> Self {
        Self {
            action: None,
            data: null_mut(),
        }
    }
}

pub struct Softirq {
    softirq_modify_lock: RawSpinlock,
    softirq_pending: u64,
    softirq_running: u64,
    softirq_table: [SoftirqVector; MAX_SOFTIRQ_NUM as usize],
}

pub static mut SOFTIRQ_HANDLER_PTR: *mut Softirq = null_mut();

#[no_mangle]
#[allow(dead_code)]
/// @brief 提供给c的接口函数,用于初始化静态指针
pub extern "C" fn softirq_init() {
    unsafe {
        if SOFTIRQ_HANDLER_PTR.is_null() {
            SOFTIRQ_HANDLER_PTR = Box::leak(Box::new(Softirq::default()));
        } else {
            kBUG!("Try to init SOFTIRQ_HANDLER_PTR twice.");
            panic!("Try to init SOFTIRQ_HANDLER_PTR twice.");
        }
    }
    let softirq_handler = __get_softirq_handler_mut();
    softirq_handler.softirq_init();
}

/// @brief 将raw pointer转换为指针,减少unsafe块
#[inline]
pub fn __get_softirq_handler_mut() -> &'static mut Softirq {
    return unsafe { SOFTIRQ_HANDLER_PTR.as_mut().unwrap() };
}

#[no_mangle]
#[allow(dead_code)]
pub extern "C" fn raise_softirq(sirq_num: u64) {
    let softirq_handler = __get_softirq_handler_mut();
    softirq_handler.set_softirq_pending(1 << sirq_num);
}

/// @brief 软中断注册函数
///
/// @param irq_num 软中断号
/// @param action 响应函数
/// @param data 响应数据结构体
#[no_mangle]
#[allow(dead_code)]
pub extern "C" fn register_softirq(
    irq_num: u32,
    action: Option<unsafe extern "C" fn(data: *mut ::core::ffi::c_void)>,
    data: *mut c_void,
) {
    let softirq_handler = __get_softirq_handler_mut();
    softirq_handler.register_softirq(irq_num, action, data);
}

/// @brief 卸载软中断
/// @param irq_num 软中断号
#[no_mangle]
#[allow(dead_code)]
pub extern "C" fn unregister_softirq(irq_num: u32) {
    let softirq_handler = __get_softirq_handler_mut();
    softirq_handler.unregister_softirq(irq_num);
}

/// 设置软中断的运行状态（只应在do_softirq中调用此宏）
#[no_mangle]
#[allow(dead_code)]
pub extern "C" fn set_softirq_pending(irq_num: u32) {
    let softirq_handler = __get_softirq_handler_mut();
    softirq_handler.set_softirq_pending(irq_num);
}

/// @brief 设置软中断运行结束
///
/// @param softirq_num
#[no_mangle]
#[allow(dead_code)]
pub extern "C" fn clear_softirq_pending(irq_num: u32) {
    let softirq_handler = __get_softirq_handler_mut();
    softirq_handler.clear_softirq_pending(irq_num);
}

/// @brief 软中断处理程序
#[no_mangle]
#[allow(dead_code)]
pub extern "C" fn do_softirq() {
    let softirq_handler = __get_softirq_handler_mut();
    softirq_handler.do_softirq();
}

impl Default for Softirq {
    fn default() -> Self {
        Self {
            softirq_modify_lock: RawSpinlock::INIT,
            softirq_pending: (0),
            softirq_running: (0),
            softirq_table: [Default::default(); MAX_SOFTIRQ_NUM as usize],
        }
    }
}

impl Softirq {
    pub fn softirq_init(&mut self) {
        self.softirq_pending = 0;
        self.softirq_table = [Default::default(); MAX_SOFTIRQ_NUM as usize];
    }
    
    pub fn get_softirq_pending(&self) -> u64 {
        return self.softirq_pending;
    }

    pub fn get_softirq_running(&self) -> u64 {
        return self.softirq_running;
    }

    #[inline]
    pub fn set_softirq_pending(&mut self, softirq_num: u32) {
        self.softirq_pending |= 1 << softirq_num;
    }

    #[inline]
    pub fn set_softirq_running(&mut self, softirq_num: u32) {
        self.softirq_running |= 1 << softirq_num;
    }

    #[inline]
    pub fn clear_softirq_running(&mut self, softirq_num: u32) {
        self.softirq_running &= !(1 << softirq_num);
    }

    /// @brief 清除软中断pending标志位
    #[inline]
    pub fn clear_softirq_pending(&mut self, softirq_num: u32) {
        self.softirq_pending &= !(1 << softirq_num);
    }

    /// @brief 判断对应running标志位是否为0
    /// @return true: 标志位为1; false: 标志位为0
    #[inline]
    pub fn read_softirq_running(&mut self, softirq_num: u32) -> bool {
        return (self.softirq_running & (1 << softirq_num)).ne(&0);
    }

    /// @brief 判断对应pending标志位是否为0
    /// @return true: 标志位为1; false: 标志位为0
    #[inline]
    pub fn read_softirq_pending(&mut self, softirq_num: u32) -> bool {
        return (self.softirq_pending & (1 << softirq_num)).ne(&0);
    }

    pub fn register_softirq(
        &mut self,
        irq_num: u32,
        action: Option<unsafe extern "C" fn(data: *mut ::core::ffi::c_void)>,
        data: *mut c_void,
    ) -> i32 {
        if self.softirq_table[irq_num as usize].action.is_some() {
            return -(EEXIST as i32);
        }

        if unsafe { verify_area(action.unwrap() as u64, 1) } {
            return -(EPERM as i32);
        }
        self.softirq_modify_lock.lock();
        self.softirq_table[irq_num as usize].action = action;
        self.softirq_table[irq_num as usize].data = data;
        self.softirq_modify_lock.unlock();
        return 0;
    }

    pub fn unregister_softirq(&mut self, irq_num: u32) -> i32 {
        for _trial_time in 0..MAX_UNREGISTER_TRIAL_TIME - 1 {
            if self.read_softirq_running(irq_num) {
                continue; //running标志位为1
            }
            if self.softirq_modify_lock.try_lock() {
                break;
            }
        }
        if !self.softirq_modify_lock.is_locked() {
            return -(EBUSY as i32);
        }
        self.clear_softirq_running(irq_num);
        self.clear_softirq_pending(irq_num);
        self.softirq_table[irq_num as usize].action = None;
        self.softirq_table[irq_num as usize].data = null_mut();
        self.softirq_modify_lock.unlock();
        return 0;
    }

    pub fn do_softirq(&mut self) {
        sti();
        let mut softirq_index: u32 = 0;
        while (softirq_index as u64) < MAX_SOFTIRQ_NUM && self.softirq_pending != 0 {
            if self.read_softirq_pending(softirq_index)
                && self.softirq_table[softirq_index as usize].action.is_some()
                && !self.read_softirq_running(softirq_index)
            {
                if self.softirq_modify_lock.try_lock() {
                    self.clear_softirq_pending(softirq_index);
                    self.set_softirq_running(softirq_index);
                    self.softirq_modify_lock.unlock();
                    unsafe {
                        (self.softirq_table[softirq_index as usize].action.unwrap())(
                            self.softirq_table[softirq_index as usize].data,
                        );
                    }
                    self.clear_softirq_running(softirq_index);
                }
            }
            softirq_index += 1;
        }
        cli();
    }
}
