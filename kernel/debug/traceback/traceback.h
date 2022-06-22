#pragma once
#include <common/glib.h>
#include<process/ptrace.h>

// 使用弱引用属性导出kallsyms中的符号表。
// 采用weak属性是由于第一次编译时，kallsyms还未链接进来，若不使用weak属性则会报错
extern const uint64_t kallsyms_address[] __attribute__((weak));
extern const uint64_t kallsyms_num __attribute__((weak));
extern const uint64_t kallsyms_names_index[] __attribute__((weak));
extern const char* kallsyms_names __attribute__((weak));

/**
 * @brief 追溯内核栈调用情况
 * 
 * @param regs 内核栈结构体
 */
void traceback(struct pt_regs * regs);