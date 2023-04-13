#pragma once

#include <sys/types.h>

#ifdef __cplusplus
   #define NULL 0
#else
   #define NULL ((void *)0)
#endif

#if defined(__cplusplus) 
extern  "C"  { 
#endif

typedef __PTRDIFF_TYPE__ ptrdiff_t; // Signed integer type of the result of subtracting two pointers.

#if defined(__cplusplus) 
}  /* extern "C" */ 
#endif