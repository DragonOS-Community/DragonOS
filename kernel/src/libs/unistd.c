#include <common/unistd.h>
#include <common/glib.h>


void swab(void *restrict src, void *restrict dest, ssize_t nbytes)
{
    unsigned char buf[32];
    char *_src = src;
    char *_dest = dest;
    uint32_t transfer;
    for (; nbytes > 0; nbytes -= transfer)
    {
        transfer = (nbytes > 32) ? 32 : nbytes;
        memcpy(buf, _src, transfer);
        memcpy(_src, _dest, transfer);
        memcpy(_dest, buf, transfer);
        _src += transfer;
        _dest += transfer;
    }
}