#ifndef CURRENT_PCB_COMPAT_H
#define CURRENT_PCB_COMPAT_H

#include <stdint.h>

uint32_t rs_current_pcb_state();
void rs_current_pcb_set_state(uint32_t state);
void rs_current_pcb_set_cpuid(uint32_t on_cpu);
uint32_t rs_current_pcb_cpuid();
int32_t rs_current_pcb_pid();
void rs_current_pcb_set_preempt_count(uint32_t num);
uint32_t rs_current_pcb_preempt_count();
uint32_t rs_current_pcb_flags();
void rs_current_pcb_set_flags(uint32_t new_flags);
int32_t rs_current_pcb_virtual_runtime();
int64_t rs_current_pcb_thread_rbp();
void* rs_get_current_pcb();
#endif