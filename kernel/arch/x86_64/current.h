#pragma once
#include <common/glib.h>

#pragma GCC push_options
#pragma GCC optimize("O0")
struct process_control_block;
// 获取当前的pcb
struct process_control_block *get_current_pcb()
{
	struct process_control_block *current = NULL;
	// 利用了当前pcb和栈空间总大小为32k大小对齐，将rsp低15位清空，即可获得pcb的起始地址
	barrier();
	__asm__ __volatile__("andq %%rsp, %0   \n\t"
						 : "=r"(current)
						 : "0"(~32767UL));
	barrier();
	return current;
};
#define current_pcb get_current_pcb()
#pragma GCC pop_options