#pragma once

#ifndef __ASM__
#define __ASM__

// 符号名
#define SYMBOL_NAME(X)	X
// 符号名字符串
#define SYMBOL_NAME_STR(X)	#X
// 符号名label
#define SYMBOL_NAME_LABEL(X) X##:

#define L1_CACHE_BYTES 32

#define asmlinkage __attribute__((regparm(0)))	

#define ____cacheline_aligned __attribute__((__aligned__(L1_CACHE_BYTES)))

#define ENTRY(name)		\
.global	SYMBOL_NAME(name);	\
SYMBOL_NAME_LABEL(name)

#endif