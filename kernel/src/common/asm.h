#pragma once

#ifndef __ASM__
#define __ASM__
#define APU_BOOT_TMP_STACK_SIZE 1024
// 符号名
#define SYMBOL_NAME(X) X
// 符号名字符串
#define SYMBOL_NAME_STR(X) #X
// 符号名label
#define SYMBOL_NAME_LABEL(X) X##:

#define ENTRY(name)                                                            \
  .global SYMBOL_NAME(name);                                                   \
  SYMBOL_NAME_LABEL(name)

#endif
