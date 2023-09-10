#include "process.h"

// 初始化 初始进程的union ，并将其链接到.data.init_proc段内
union proc_union initial_proc_union
    __attribute__((__section__(".data.init_proc_union"))) = {0};

// 为每个核心初始化初始进程的tss
struct tss_struct initial_tss[MAX_CPU_NUM] = {[0 ... MAX_CPU_NUM - 1] = INITIAL_TSS};
