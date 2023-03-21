#pragma once

#include <common/glib.h>
#include <process/process.h>

/*
 * Scheduling policies
 */
#define SCHED_NORMAL 0
#define SCHED_FIFO 1
#define SCHED_RR 2
#define SCHED_BATCH 3
/* SCHED_ISO: reserved but not implemented yet */
#define SCHED_IDLE 5
#define SCHED_DEADLINE 6
#define SCHED_MAX_POLICY_NUM SCHED_DEADLINE

#define IS_VALID_SCHED_POLICY(_policy) ((_policy) > 0 && (_policy) <= SCHED_MAX_POLICY_NUM)


// ================= Rust 实现 =============
extern void sched_update_jiffies();
extern void sched_init();
extern void sched();
extern void sched_enqueue(struct process_control_block *pcb, bool reset_time);
extern void sched_set_cpu_idle(uint64_t cpu_id, struct process_control_block *pcb);
extern void sched_migrate_process(struct process_control_block *pcb, uint64_t target);

void switch_proc(struct process_control_block *prev, struct process_control_block *proc);
