#include "mmio.h"
#include <common/math.h>
extern void __mmio_buddy_init();

void mmio_init()
{
    __mmio_buddy_init();
    kinfo("mmio_init success");
}
