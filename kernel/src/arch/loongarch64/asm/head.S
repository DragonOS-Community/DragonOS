#include "common/asm.h"

.section .bootstrap

// 内核入口
.global _start
.type _start, @function
ENTRY(_start)
1:
    b 1b
