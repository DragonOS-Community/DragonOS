#include "mm.h"
#include "mm-types.h"
#include "mmio.h"
#include "slab.h"
#include <common/printk.h>
#include <common/kprint.h>
#include <driver/multiboot2/multiboot2.h>
#include <process/process.h>
#include <common/compiler.h>
#include <common/errno.h>
#include <debug/traceback/traceback.h>

struct mm_struct initial_mm = {0};
