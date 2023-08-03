use core::ffi::c_void;

use alloc::{sync::Arc, vec::Vec};

/// 通知链中注册的回调函数类型
type NotifierFnT = fn(Arc<NotifierBlock>, u64, *mut c_void) -> i32;

/// 通知链节点
pub struct NotifierBlock {
    notifier_call: Option<NotifierFnT>,
    priority: i32,
}

impl NotifierBlock {
    pub fn new(notifier_call: Option<NotifierFnT>, priority: i32) -> Self {
        Self {
            notifier_call,
            priority,
        }
    }
}
