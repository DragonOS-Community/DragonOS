use core::sync::atomic::{AtomicU32, Ordering};

use crate::{arch::asm::current::current_pcb, include::bindings::bindings::PROC_UNINTERRUPTIBLE};

use super::wait_queue::WaitQueue;

struct Semaphore {
    counter: AtomicU32,
    wait_queue: WaitQueue,
}

impl Semaphore {
    fn new(counter: u32) -> Self {
        Self {
            counter: AtomicU32::new(counter),
            wait_queue: WaitQueue::INIT,
        }
    }

    fn down(&self){
        if self.counter.fetch_sub(1, Ordering::Release)<=0{
            self.counter.fetch_add(1,Ordering::Relaxed);
            //let wait=WaitQueue::
            //current_pcb().state=PROC_UNINTERRUPTIBLE as u64;
        }//资源不充足,信号量<=0
 
    }
}
