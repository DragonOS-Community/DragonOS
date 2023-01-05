use alloc::boxed::Box;
use core::ffi::c_void;
use core::ptr::null_mut;

use crate::arch::interrupt::cli;
use crate::arch::interrupt::sti;
use crate::kBUG;
use crate::kdebug;
use crate::libs::spinlock::RawSpinlock;

const MAX_SOFTIRQ_NUM: u64 = 64;

/// not used until softirq.h is removed
pub enum SirqParam {
    TIMER_SIRQ = 0,         //时钟软中断信号
    VIDEO_REFRESH_SIRQ = 1, //帧缓冲区刷新软中断
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct softirq_t {
    pub action: Option<unsafe extern "C" fn(data: *mut ::core::ffi::c_void)>, //软中断处理函数
    pub data: *mut c_void,
}

impl Default for softirq_t {
    fn default() -> Self {
        Self {
            action: None,
            data: null_mut(),
        }
    }
}

pub struct SoftirqHandlerT {
    pub softirq_modify_lock: RawSpinlock,
    pub softirq_pending: u64,
    pub softirq_running: u64,
    pub softirq_vector: [softirq_t; MAX_SOFTIRQ_NUM as usize],
}

pub static mut SOFTIRQ_HANDLER_PTR: *mut SoftirqHandlerT = null_mut();

#[no_mangle]
#[allow(dead_code)]
pub extern "C" fn softirq_init() {
    unsafe {
        if SOFTIRQ_HANDLER_PTR.is_null() {
            SOFTIRQ_HANDLER_PTR = Box::leak(Box::new(SoftirqHandlerT::default()));
        } else {
            kBUG!("Try to init SOFTIRQ_HANDLER_PTR twice.");
            panic!("Try to init SOFTIRQ_HANDLER_PTR twice.");
        }
    }
    let softirq_handler = __get_softirq_handler_ref();
    softirq_handler.softirq_init();
    kdebug!("fine?");
}

#[inline]
pub fn __get_softirq_handler_ref() -> &'static mut SoftirqHandlerT {
    return unsafe { SOFTIRQ_HANDLER_PTR.as_mut().unwrap() };
}

#[no_mangle]
#[allow(dead_code)]
pub extern "C" fn raise_softirq(sirq_num: u64) {
    let softirq_handler = __get_softirq_handler_ref();
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
    let softirq_handler = __get_softirq_handler_ref();
    softirq_handler.register_softirq(irq_num, action, data);
}


/// @brief 卸载软中断
/// @param irq_num 软中断号
#[no_mangle]
#[allow(dead_code)]
pub extern "C" fn unregister_softirq(irq_num: u32) {
    let softirq_handler = __get_softirq_handler_ref();
    softirq_handler.unregister_softirq(irq_num);
}

/// 设置软中断的运行状态（只应在do_softirq中调用此宏）
#[no_mangle]
#[allow(dead_code)]
pub extern "C" fn set_softirq_pending(status: u64) {
    let softirq_handler = __get_softirq_handler_ref();
    softirq_handler.set_softirq_pending(status);
}


/// @brief 设置软中断运行结束
///
/// @param softirq_num
#[no_mangle]
#[allow(dead_code)]
pub extern "C" fn clear_softirq_pending(irq_num: u32) {
    let softirq_handler = __get_softirq_handler_ref();
    softirq_handler.clear_softirq_pending(irq_num);
}

/// @brief 软中断处理程序
#[no_mangle]
#[allow(dead_code)]
pub extern "C" fn do_softirq() {
    let softirq_handler = __get_softirq_handler_ref();
    softirq_handler.do_softirq();
}

impl Default for SoftirqHandlerT {
    fn default() -> Self {
        Self {
            softirq_modify_lock: RawSpinlock::INIT,
            softirq_pending: (0),
            softirq_running: (0),
            softirq_vector: [Default::default(); MAX_SOFTIRQ_NUM as usize],
        }
    }
}

impl SoftirqHandlerT {
    pub fn set_softirq_pending(&mut self, status: u64) {
        self.softirq_pending |= status;
    }

    pub fn get_softirq_pending(&self) -> u64 {
        return self.softirq_pending;
    }

    pub fn get_softirq_running(&self) -> u64 {
        return self.softirq_running;
    }

    pub fn clear_softirq_running(&mut self, softirq_num: u32) {
        self.softirq_running &= !(1 << softirq_num);
    }

    pub fn set_softirq_running(&mut self, softirq_num: u32) {
        self.softirq_running |= 1 << softirq_num;
    }
    /// @brief 清除软中断pending标志位
    pub fn softirq_ack(&mut self, softirq_num: u32) {
        self.softirq_pending &= !(1 << softirq_num);
    }

    pub fn register_softirq(
        &mut self,
        irq_num: u32,
        action: Option<unsafe extern "C" fn(data: *mut ::core::ffi::c_void)>,
        data: *mut c_void,
    ) {
        self.softirq_vector[irq_num as usize].action = action;
        self.softirq_vector[irq_num as usize].data = data;
    }

    pub fn unregister_softirq(&mut self, irq_num: u32) {
        self.softirq_vector[irq_num as usize].action = None;
        self.softirq_vector[irq_num as usize].data = null_mut();
    }

    pub fn do_softirq(&mut self) {
        sti();
        let mut index: u32 = 0;
        while (index as u64) < MAX_SOFTIRQ_NUM && self.softirq_pending != 0 {
            if (self.softirq_pending & (1 << index)) != 0
                && self.softirq_vector[index as usize].action != None
                && (!(self.get_softirq_running() & (1 << index))) != 0
            {
                if self.softirq_modify_lock.try_lock() {
                    if (self.get_softirq_running() & (1 << index)) != 0 {
                        self.softirq_modify_lock.unlock();
                        index += 1;
                        continue;
                    }
                    self.softirq_ack(index);
                    self.set_softirq_running(index);
                    self.softirq_modify_lock.unlock();

                    unsafe {
                        (self.softirq_vector[index as usize].action.unwrap())(
                            self.softirq_vector[index as usize].data,
                        );
                    }

                    self.clear_softirq_running(index);
                }
            }
            index += 1;
        }
        cli();
    }

    pub fn clear_softirq_pending(&mut self, irq_num: u32) {
        self.clear_softirq_running(irq_num);
    }

    pub fn softirq_init(&mut self) {
        self.softirq_pending = 0;
        self.softirq_vector = [Default::default(); MAX_SOFTIRQ_NUM as usize];
    }
}
