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

extern void sched_init();
extern void sched();
