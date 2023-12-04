#include "common/asm.h"

.section .bootstrap

.global _start
.type _start, @function
ENTRY(_start)
loop:
    j loop
