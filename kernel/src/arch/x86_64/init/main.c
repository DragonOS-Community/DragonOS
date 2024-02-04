//
// Created by longjin on 2022/1/20.
//

#include <common/cpu.h>


void __init_set_cpu_stack_start(uint32_t cpu, uint64_t stack_start)
{
  cpu_core_info[cpu].stack_start = stack_start;
}
