#include "common/asm.h"
#include "asm/csr.h"

.section .bootstrap

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
	

	// 暂存hartid
	la t0, __initial_hartid_ptr
	sd a0, 0(t0)
	// 暂存平坦设备树地址
	la t0, __initial_fdt_ptr
	sd a1, 0(t0)


	// 暂存_start标签被DragonStub加载到的物理地址
	auipc t0, 0
	li t1, -4095
	and t0, t0, t1
	la t1, __initial_start_load_paddr
	sd t0, 0(t1)

	// 清空页表的空间
	la a0, __initial_pgtable
	call __initial_clear_pgtable
	la a0, __initial_l1_pgtable
	call __initial_clear_pgtable
	la a0, __initial_l1_pgtable
	li a1, 4096
	add a0, a0, a1
	call __initial_clear_pgtable 



	// 设置页表，把内核当前所在的物理地址映射到链接时的内核虚拟空间
	la a0, __initial_start_load_paddr
	ld a0, 0(a0)

	// 偏移量0xffffffc000000000，计算起始的L0页表项
	// 因为内核链接地址还有16M的空间，所以这里加上0x1000000
	li a1, KERNEL_VIRT_START
	
	// 映射物理地址到虚拟地址
	call initial_map_256M_phys_addr

	// 增加恒等映射
	la a0, __initial_start_load_paddr
	ld a0, 0(a0)

	mv a1, a0
	call initial_map_1g_identical

__init_set_pgtable_loop_end:
	call __initial_reloacate_enable_mmu



.option push
.option norelax
	// 设置栈指针
	la a0, BSP_IDLE_STACK_SPACE
	mv sp, a0
	li t0, 32752	// 预留16字节防止越界
	add sp, sp, t0
.option pop
	/*
	 * Disable FPU & VECTOR to detect illegal usage of
	 * floating point or vector in kernel space
	 */
	li t0, SR_FS_VS
	csrc CSR_STATUS, t0

	
	/* Call the kernel */
	la a0, __initial_hartid_ptr
	ld a0, 0(a0)
	la a1, __initial_fdt_ptr
	ld a1, 0(a1)
	
	
	// 跳转到kernel_main
	call kernel_main
	nop
	wfi


__initial_reloacate_enable_mmu:
	// 计算起始物理地址与内核高虚拟地址的偏移量
	la t0, __initial_start_load_paddr
	ld t0, 0(t0)

	
	li t1, KERNEL_VIRT_START
	sub t1, t1, t0

	// 重定位返回地址
	add ra, ra, t1

	/* Point stvec to virtual address of intruction after satp write */
	/* Set trap vector to spin forever to help debug */
	la a2, 3f
	add a2, a2, t1
	csrw CSR_TVEC, a2
	// enable MMU
	la a2, __initial_pgtable
	srli a2, a2, 12
	la a0, __initial_satp_mode
	ld a0, 0(a0)
	or a2, a2, a0


	sfence.vma
	csrw satp, a2
	
3:
	la a0, __initial_Lsecondary_park
	add a0, a0, t1
	csrw CSR_TVEC, a0

	csrw satp, a2
	sfence.vma

	ret

// 映射物理地址到虚拟地址(2M页，1G大小)
// 参数：
//   a0: 物理地址
//   a1: 虚拟地址
initial_map_256M_phys_addr:
	// 检查物理地址是否对齐到2M
	li t0, 0x1fffff
	and t0, t0, a0
	bnez t0, __initial_map_1g_phys_failed

	// 检查虚拟地址是否对齐到2M
	li t0, 0x1fffff
	and t0, t0, a1
	bnez t0, __initial_map_1g_phys_failed

	// 把起始虚拟地址存储到t2中
	mv t2, a1
	// 按照2M对齐
	li t1, -0x200000
	and t2, t2, t1

	// 计算L0页表项的索引
	srli t2, t2, 30
	andi t2, t2, 511
	
	
	// 填写第一个L0页表项
	la t4, __initial_pgtable
	slli t5, t2, 3 // t5 = t2 * 8
	add t4, t4, t5 // t4 = t4 + t5， t4指向L0页表项

	// 提取L1页表的地址
	la t5, __initial_l1_pgtable
	srli t5, t5, 12
	slli t5, t5, 10
	ori t5, t5, 0x1 // 设置L1页表项属性，V = 1
	// 设置L0页表项的值
	sd t5, 0(t4)

	// 计算是否需要填写第二个L1页表项（判断是否超过第一个L1页表的范围）
	addi t3, t2, 128
	li t5, 512
	blt t3, t5, __initial_set_l1_pgtable
	// 填写第二个L1页表
	la t3, __initial_l1_pgtable
	li t5, 4096
	add t3, t3, t5
	srli t3, t3, 12
	slli t3, t3, 10
	ori t3, t3, 0x1 // 设置L1页表项属性，V = 1
	// 设置L0页表项的值
	sd t3, 8(t4)

__initial_set_l1_pgtable:	// 开始填写L1页表

	// 获取起始物理地址
	mv t6, a0
	// 获取L1页表的地址
	la t0, __initial_l1_pgtable

	// 计算起始L1页表项的索引
	mv t3, a1
	srli t3, t3, 21
	andi t3, t3, 511

	slli t3, t3, 3 // t3 = t3 * 8
	add t0, t0, t3 // t0 = t0 + t3
	
	// 加载计数器
	li t5, 0
__initial_set_l1_pgtable_loop:
	
	mv t3, t6
	srli t3, t3, 12 // t3 = t6 >> 12 (page frame number)
	li t1, 0x3FFFFFFFFFFFFF
	and t3, t3, t1 // t3 = t3 & 0x3FFFFFFFFFFFFF
	slli t3, t3, 10 // t3 = t3 << 10
	ori t3, t3, 0xEF // 设置L1页表项属性，set R/W/X/V/A/D/G = 1
	// 设置L1页表项的值
	sd t3, 0(t0)

	// 增加 页表项指针
	addi t0, t0, 8
	// 增加 t6 的值(2M)
	li t2, 0x200000
	add t6, t6, t2

	// 增加计数器
	addi t5, t5, 1
	// 判断计数器是否超过128
	li t2, 128
	blt t5, t2, __initial_set_l1_pgtable_loop

	
	// 填写完成
	ret




__initial_map_1g_phys_failed:
	// 地址没有对齐到2M
	wfi
	la a0, __initial_map_1g_phys_failed
	// 跳转 
	jr a0



// 映射物理地址到虚拟地址（恒等映射）
// 参数：
//   a0: 物理地址
initial_map_1g_identical:
	mv a1, a0
	// 把_start向下对齐到1GB
	li t0, -0x40000000
	// 计算起始物理地址,存放在t0中
	and t0, t0, a0


	// 把起始虚拟地址存储到t2中
	mv t2, a1
	// 按照1g对齐
	li t1, -0x40000000
	and t2, t2, t1

	
	// 右移30位，得到L0页表项的索引
	srli t2, t2, 30
	// 与511进行与运算，得到L0页表项的索引
	andi t2, t2, 511
	

	// 填写页表项
	la t4, __initial_pgtable
	slli t3, t2, 3 // t3 = t2 * 8
	add t4, t4, t3 // t4 = t4 + t3


	mv t3, t0
	srli t3, t3, 12 // t3 = t0 >> 12 (page frame number)
	slli t3, t3, 10 // t3 = t3 << 10 
	ori t3, t3, 0xEF // set R/W/X/V/A/D/G = 1

	// 计算delta的pfn
	li t2, 0x40000000
	srli t2, t2, 12
	// 把delta pfn移位到页表项的第10位的位置
	slli t2, t2, 10
	li t1, 2

__loop_set_8g:

	
	sd t3, 0(t4)

	// 增加 t4 的值
	addi t4, t4, 8
	// 增加1G的pfn	
	add t3, t3, t2
	
	
	addi t1, t1, -1
	bnez t1, __loop_set_8g

	ret


// 用于清空页表的空间
// 参数：
//   a0: page table address
__initial_clear_pgtable:
	mv t0, a0
	li t1, 512
	li t2, 0 // 用于存储 0

__initial_clear_pgtable_loop:

	sd t2, 0(t0) // 将 0 存储到当前word
	addi t0, t0, 8 // 增加 t0 的值
	addi t1, t1, -1 // 减少剩余的word数
	bnez t1, __initial_clear_pgtable_loop

	ret


.align 2
__initial_Lsecondary_park:
	/* We lack SMP support or have too many harts, so park this hart */
	wfi
	j __initial_Lsecondary_park

// 全局变量，存储平坦设备树的地址和hartid
.global __initial_fdt_ptr
__initial_fdt_ptr:
	.quad 0

.global __initial_hartid_ptr
__initial_hartid_ptr:
	.quad 0

// _start标签在启动时被加载到的物理地址
.global __initial_start_load_paddr
__initial_start_load_paddr:
	.quad 0

__initial_kernel_main_vaddr:
	.quad 0



.global __initial_satp_mode
__initial_satp_mode:
	.quad SATP_MODE_39

// 初始页表的空间（sv39模式的L0页表）
.section .initial_pgtable_section
.global __initial_pgtable
__initial_pgtable:
	.skip 4096

.global __initial_l1_pgtable
__initial_l1_pgtable:
	.skip 8192

