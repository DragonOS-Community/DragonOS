#pragma once
#include "mm.h"

extern void mmio_create(uint32_t size, uint64_t vm_flagsu, uint64_t* res_vaddr, uint64_t* res_length);
extern int mmio_release(int vaddr, int length);
void mmio_init();
