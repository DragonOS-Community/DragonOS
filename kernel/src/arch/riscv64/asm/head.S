#include "common/asm.h"

.section .bootstrap

#define CSR_SIE			0x104
#define CSR_SIP			0x144

#define CSR_IE CSR_SIE
#define CSR_IP CSR_SIP

// 内核入口（从DragonStub跳转到这里）
// 参数：
//   a0: hartid （核心ID）
//   a1: fdt （平坦设备树）
.global _start
.type _start, @function
ENTRY(_start)
	/* Mask all interrupts */
	csrw CSR_IE, zero
	csrw CSR_IP, zero
	/* Load the global pointer */
.option push
.option norelax
	la sp, BSP_IDLE_STACK_SPACE
	li t0, 32768
	add sp, sp, t0
.option pop
	/* Call the kernel */
	call kernel_main
	nop
_loop:
	j _loop


