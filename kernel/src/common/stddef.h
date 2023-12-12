#pragma once

#include "./sys/types.h"

#define NULL (void*)0

typedef __PTRDIFF_TYPE__ ptrdiff_t; // Signed integer type of the result of subtracting two pointers.

#ifndef __always_inline
#define __always_inline __inline__
#endif

// 定义类型的缩写
typedef unsigned char uchar;
typedef unsigned short ushort;
typedef unsigned int uint;
typedef unsigned long ul;
typedef unsigned long long int ull;
typedef long long int ll;