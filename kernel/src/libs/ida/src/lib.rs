#![no_std]
#![feature(core_intrinsics)]
#![allow(clippy::needless_return)]

use core::intrinsics::unlikely;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// id分配器
///
/// TODO: 当前只是为了简单实现功能，将来这里应使用类似linux的ida的方式去实现
#[derive(Debug)]
pub struct IdAllocator {
    current_id: AtomicUsize,
    max_id: usize,
    dead: AtomicBool,
}

impl IdAllocator {
    /// 创建一个新的id分配器
    pub const fn new(initial_id: usize, max_id: usize) -> Self {
        Self {
            current_id: AtomicUsize::new(initial_id),
            max_id,
            dead: AtomicBool::new(false),
        }
    }

    /// 分配一个新的id
    ///
    /// ## 返回
    ///
    /// 如果分配成功，返回Some(id)，否则返回None
    pub fn alloc(&self) -> Option<usize> {
        if unlikely(self.dead.load(Ordering::SeqCst)) {
            return None;
        }

        let ret = self.current_id.fetch_add(1, Ordering::SeqCst);
        // 如果id溢出，panic
        if ret == self.max_id {
            self.dead.store(true, Ordering::SeqCst);
            return None;
        }

        return Some(ret);
    }

    pub fn free(&self, _id: usize) {
        // todo: free
    }
}
