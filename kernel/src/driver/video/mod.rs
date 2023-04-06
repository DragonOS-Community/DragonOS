use core::{ptr::null_mut, sync::atomic::{AtomicBool, Ordering}};

use alloc::sync::Arc;

use crate::{
    exception::softirq::{SoftirqNumber, SoftirqVec, softirq_vectors},
    include::bindings::bindings::video_refresh_framebuffer,
};

#[derive(Debug)]
pub struct VideoRefreshFramebuffer{
    running: AtomicBool
}

impl SoftirqVec for VideoRefreshFramebuffer {
    fn run(&self) {
        if self.set_run() == false{
            return;
        }
        
        unsafe {
            video_refresh_framebuffer(null_mut());
        }

        self.clear_run();
    }
}
impl VideoRefreshFramebuffer {
    pub fn new() -> VideoRefreshFramebuffer {
        VideoRefreshFramebuffer {
            running: AtomicBool::new(false)
        }
    }

    fn set_run(&self) -> bool {
        let x = self
            .running
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed);
        if x.is_ok() {
            return true;
        } else {
            return false;
        }
    }

    fn clear_run(&self) {
        self.running.store(false, Ordering::Release);
    }
}

pub fn register_softirq_video() {
    // kdebug!("register_softirq_video");
    let handler = Arc::new(VideoRefreshFramebuffer::new());
    softirq_vectors()
        .register_softirq(SoftirqNumber::VideoRefresh, handler)
        .expect("register_softirq_video run failed");
}
// ======= 以下为给C提供的接口,video重构完后请删除 =======
#[no_mangle]
pub extern "C" fn rs_register_softirq_video() {
    register_softirq_video();
}
