#pragma once
#include "mm.h"

extern int rs_mmio_create(uint32_t size, uint64_t vm_flags, uint64_t* res_vaddr, uint64_t* res_length);
extern int rs_mmio_release(uint64_t vaddr, uint64_t length);
