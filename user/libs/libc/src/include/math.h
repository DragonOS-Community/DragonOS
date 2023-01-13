#pragma once
#include <stddef.h>

#if defined(__cplusplus) 
extern  "C"  { 
#endif

double fabs(double x);
// float fabsf(float x);
long double fabsl(long double x);

double round(double x);
float roundf(float x);
long double roundl(long double x);

int64_t pow(int64_t x, int y);

#if defined(__cplusplus) 
}  /* extern "C" */ 
#endif