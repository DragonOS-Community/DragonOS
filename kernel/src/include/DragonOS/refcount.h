#pragma once
#include <common/atomic.h>

typedef struct refcount_struct {
	atomic_t refs;
} refcount_t;