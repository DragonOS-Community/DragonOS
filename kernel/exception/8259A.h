/**
 * @file 8259A.h
 * @author longjin
 * @brief 8259A中断芯片
 * @version 0.1
 * @date 2022-01-29
 * 
 * @copyright Copyright (c) 2022
 * 
 */

#pragma once

#include "../common/glib.h"

#define PIC_EOI		0x20
#define PIC_master		0x20		/* IO base address for master PIC */
#define PIC2_slave		0xA0		/* IO base address for slave PIC */

// 初始化8259A芯片的中断服务
void init_8259A();




