#pragma once

#include <stdint.h>

extern uint32_t rs_current_pcb_state();
extern uint32_t rs_current_pcb_cpuid();
extern int32_t rs_current_pcb_pid();
extern uint32_t rs_current_pcb_preempt_count();
extern uint32_t rs_current_pcb_flags();
extern void rs_current_pcb_set_flags(uint32_t new_flags);
extern int32_t rs_current_pcb_virtual_runtime();
extern int64_t rs_current_pcb_thread_rbp();
extern void* rs_get_current_pcb();