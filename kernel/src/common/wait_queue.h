#ifndef WAIT_QUEUE_H
#define WAIT_QUEUE_H

#ifdef __cplusplus
extern "C" {
#endif

// 声明 Rust 函数的原型
void rs_waitqueue_sleep_on(void* wait_queue);
void* rs_waitqueue_init();

void rs_waitqueue_sleep_on_interriptible(void* wait_queue);
void rs_waitqueue_wakeup(void* wait_queue, unsigned long long state);

#ifdef __cplusplus
}
#endif

#endif /* WAIT_QUEUE_H */

