#include "mmio.h"
#include <common/math.h>

void mmio_init()
{
    __mmio_buddy_init();
}
