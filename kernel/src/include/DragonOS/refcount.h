#pragma once

#if ARCH(I386) || ARCH(X86_64)

#include <common/atomic.h>

// 该结构体需要与libs/refcount.rs的保持一致，且以rust版本为准
typedef struct refcount_struct {
	atomic_t refs;
} refcount_t;

#endif