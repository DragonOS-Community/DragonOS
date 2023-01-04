use core::ffi::c_void;
use core::intrinsics::size_of;
use core::ptr::null_mut;
use alloc::boxed::Box;

use crate::arch::x86_64::interrupt::cli;
use crate::include::bindings::bindings::{memset, spin_init, spin_trylock, spin_unlock};
use crate::kBUG;
use crate::{arch::x86_64::interrupt::sti, include::bindings::bindings::spinlock_t};

const MAX_SOFTIRQ_NUM: u64 = 64;

pub enum SIRQ_PARAM {
    TIMER_SIRQ = 0,
    VIDEO_REFRESH_SIRQ = 1,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct softirq_t {
    pub action: *mut Option<unsafe extern "C" fn(data: *mut ::core::ffi::c_void)>,
    pub data: *mut c_void,
}

impl Default for softirq_t {
    fn default() -> Self {
        Self {
            action: null_mut(),
            data: null_mut(),
        }
    }
}

pub struct softirq_handler_t {
    pub softirq_modify_lock: spinlock_t,
    pub softirq_pending: u64,
    pub softirq_running: u64,
    pub softirq_vector: [softirq_t; MAX_SOFTIRQ_NUM as usize],
}

pub static mut softirq_handler_t_ptr: *mut softirq_handler_t = null_mut();

#[no_mangle]
#[allow(dead_code)]
pub extern "C" fn softirq_init() {
    unsafe {
        if softirq_handler_t_ptr.is_null() {
            softirq_handler_t_ptr = Box::leak(Box::new(softirq_handler_t::default()));
        } else {
            kBUG!("Try to init softirq_handler_t_ptr twice.");
            panic!("Try to init softirq_handler_t_ptr twice.");
        }
        (*softirq_handler_t_ptr).softirq_init();
    }
}

#[no_mangle]
#[allow(dead_code)]
pub extern "C" fn raise_softirq(sirq_num: u64) {
    unsafe {
        (*softirq_handler_t_ptr).set_softirq_pending(1 << sirq_num);
    }
}
#[no_mangle]
#[allow(dead_code)]
pub extern "C" fn register_softirq(
    irq_num: u32,
    action: *mut Option<unsafe extern "C" fn(data: *mut ::core::ffi::c_void)>,
    data: *mut c_void,
) {
    unsafe {
        (*softirq_handler_t_ptr).register_softirq(irq_num, action, data);
    }
}
#[no_mangle]
#[allow(dead_code)]
pub extern "C" fn unregister_softirq(irq_num: u32){
    unsafe{
        (*softirq_handler_t_ptr).unregister_softirq(irq_num);
    }
}

#[no_mangle]
#[allow(dead_code)]
pub extern "C" fn set_softirq_pending(status: u64){
    unsafe{
        (*softirq_handler_t_ptr).set_softirq_pending(status);
    }
}

#[no_mangle]
#[allow(dead_code)]
pub extern "C" fn clear_softirq_pending(irq_num: u32){
    unsafe{
        (*softirq_handler_t_ptr).clear_softirq_pending(irq_num);
    }
}

#[no_mangle]
#[allow(dead_code)]
pub extern "C" fn do_softirq(){
    unsafe{
        (*softirq_handler_t_ptr).do_softirq();
    }
}

impl Default for softirq_handler_t {
    fn default() -> Self {
        Self {
            softirq_modify_lock: Default::default(),
            softirq_pending: (0),
            softirq_running: (0),
            softirq_vector: [Default::default(); MAX_SOFTIRQ_NUM as usize],
        }
    }
}

impl softirq_handler_t {
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

    pub fn softirq_ack(&mut self, softirq_num: u32) {
        self.softirq_pending &= !(1 << softirq_num);
    }

    pub fn register_softirq(
        &mut self,
        irq_num: u32,
        action: *mut Option<unsafe extern "C" fn(data: *mut ::core::ffi::c_void)>,
        data: *mut c_void,
    ) {
        self.softirq_vector[irq_num as usize].action = action;
        self.softirq_vector[irq_num as usize].data = data;
    }

    pub fn unregister_softirq(&mut self, irq_num: u32) {
        self.softirq_vector[irq_num as usize].action = null_mut();
        self.softirq_vector[irq_num as usize].data = null_mut();
    }

    pub fn do_softirq(&mut self) {
        sti();
        let mut index: u32 = 0;
        while (index as u64) < MAX_SOFTIRQ_NUM && self.softirq_pending != 0 {
            if (self.softirq_pending & (1 << index)) != 0
                && self.softirq_vector[index as usize].action != null_mut()
                && (!(self.get_softirq_running() & (1 << index))) != 0
            {
                unsafe {
                    if spin_trylock(&mut self.softirq_modify_lock) != 0 {
                        if (self.get_softirq_running() & (1 << index)) != 0 {
                            spin_unlock(&mut self.softirq_modify_lock);
                            index += 1;
                            continue;
                        }

                        self.softirq_ack(index);
                        self.set_softirq_running(index);
                        spin_unlock(&mut self.softirq_modify_lock);

                        ((*self.softirq_vector[index as usize].action).unwrap())(
                            self.softirq_vector[index as usize].data,
                        );
                        self.clear_softirq_running(index);
                    }
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
        unsafe {
            memset(
                &mut self.softirq_vector as *mut _ as *mut c_void,
                0,
                (size_of::<softirq_t>() * MAX_SOFTIRQ_NUM as usize) as u64,
            );
            spin_init(&mut self.softirq_modify_lock);
        }
    }
}
