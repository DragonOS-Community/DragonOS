#include "stat.h"
#include<libsystem/syscall.h>

int mkdir(const char *path, mode_t mode)
{
    return syscall_invoke(SYS_MKDIR, (uint64_t)path, (uint64_t)mode, 0,0,0,0,0,0);
}