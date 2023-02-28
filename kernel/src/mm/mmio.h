#pragma once
#include "mm.h"

extern void mmio_buddy_init();
extern void mmio_create();
extern int mmio_release(int vaddr, int length);
extern void __mmio_buddy_init();
void mmio_init();
