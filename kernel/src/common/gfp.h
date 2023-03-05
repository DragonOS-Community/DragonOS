#pragma once
#include <common/sys/types.h>
#include <common/compiler.h>
/**
 * __GFP_ZERO: 获取内存的同时，将获取到的这块内存清空
 *
 */
#define __GFP_ZERO ((gfp_t)(1UL << 0))