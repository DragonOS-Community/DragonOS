#pragma once

// 声明 Rust 函数的原型
extern void rs_waitqueue_sleep_on(void* wait_queue);
extern void* rs_waitqueue_init();
extern void rs_waitqueue_sleep_on_interriptible(void* wait_queue);
extern void rs_waitqueue_wakeup(void* wait_queue, unsigned long long state);
