#include "sched.h"

/**
 * @brief 切换进程上下文。请注意，只能在中断上下文内调用本函数
 * TODO：使用Rust重构这里
 * @param prev 前一个进程的pcb
 * @param proc 后一个进程的pcb
 */
void switch_proc(struct process_control_block *prev, struct process_control_block *proc)
{
    // process_switch_mm(proc);
    io_mfence();
    switch_to(prev, proc);
}