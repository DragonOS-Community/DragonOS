#include "common/asm.h"

.section .bootstrap

#define CSR_SSTATUS		0x100
#define CSR_SIE			0x104
#define CSR_SIP			0x144

# define CSR_STATUS	CSR_SSTATUS
#define CSR_IE CSR_SIE
#define CSR_IP CSR_SIP

#define SR_FS 0x00006000
#define SR_VS 0x00000600
#define SR_FS_VS	(SR_FS | SR_VS) /* Vector and Floating-Point Unit */

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
	/*
	 * Disable FPU & VECTOR to detect illegal usage of
	 * floating point or vector in kernel space
	 */
	li t0, SR_FS_VS
	csrc CSR_STATUS, t0
	/* Call the kernel */
	call kernel_main
	nop
_loop:
	j _loop


