//! 这个文件内放置初始内核线程的代码。

use crate::process::kthread::KernelThreadMechanism;

pub fn initial_kernel_thread() -> i32 {
    KernelThreadMechanism::init_stage2();
    todo!("Implement initial_kernel_thread");
}
