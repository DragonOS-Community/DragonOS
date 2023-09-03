#pragma once

#include <stdint.h>

extern uint32_t rs_current_pcb_cpuid();
extern uint32_t rs_current_pcb_pid();
extern uint32_t rs_current_pcb_preempt_count();
extern uint32_t rs_current_pcb_flags();
extern int64_t rs_current_pcb_thread_rbp();
