#include <string.h>
#include <stddef.h>

size_t strlen(const char *s)
{
    register int __res = 0;
    while (s[__res] != '\0')
    {
        ++__res;
    }
    return __res;
}

int strcmp(const char *FirstPart, const char *SecondPart)
{
    register int __res;
    __asm__ __volatile__("cld	\n\t"
                         "1:	\n\t"
                         "lodsb	\n\t"
                         "scasb	\n\t"
                         "jne	2f	\n\t"
                         "testb	%%al,	%%al	\n\t"
                         "jne	1b	\n\t"
                         "xorl	%%eax,	%%eax	\n\t"
                         "jmp	3f	\n\t"
                         "2:	\n\t"
                         "movl	$1,	%%eax	\n\t"
                         "jl	3f	\n\t"
                         "negl	%%eax	\n\t"
                         "3:	\n\t"
                         : "=a"(__res)
                         : "D"(FirstPart), "S"(SecondPart)
                         :);
    return __res;
}

void *memset(void *dst, unsigned char C, uint64_t size)
{

    int d0, d1;
    unsigned long tmp = C * 0x0101010101010101UL;
    __asm__ __volatile__("cld	\n\t"
                         "rep	\n\t"
                         "stosq	\n\t"
                         "testb	$4, %b3	\n\t"
                         "je	1f	\n\t"
                         "stosl	\n\t"
                         "1:\ttestb	$2, %b3	\n\t"
                         "je	2f\n\t"
                         "stosw	\n\t"
                         "2:\ttestb	$1, %b3	\n\t"
                         "je	3f	\n\t"
                         "stosb	\n\t"
                         "3:	\n\t"
                         : "=&c"(d0), "=&D"(d1)
                         : "a"(tmp), "q"(size), "0"(size / 8), "1"(dst)
                         : "memory");
    return dst;
}

char *strncpy(char *dst, const char *src, size_t count)
{
    __asm__ __volatile__("cld	\n\t"
                         "1:	\n\t"
                         "decq	%2	\n\t"
                         "js	2f	\n\t"
                         "lodsb	\n\t"
                         "stosb	\n\t"
                         "testb	%%al,	%%al	\n\t"
                         "jne	1b	\n\t"
                         "rep	\n\t"
                         "stosb	\n\t"
                         "2:	\n\t"
                         :
                         : "S"(src), "D"(dst), "c"(count)
                         : "ax", "memory");
    return dst;
}

char *strcat(char *dest, const char *src)
{
    strcpy(dest + strlen(dest), src);
    return dest;
}

char *strcpy(char *dst, const char *src)
{
    while (*src)
    {
        *(dst++) = *(src++);
    }
    *dst = 0;

    return dst;
}

char *strtok(char *str, const char *delim)
{
    static char *saveptr;
    return strtok_r(str, delim, &saveptr);
}

char *strtok_r(char *str, const char *delim, char **saveptr)
{
    char *end;
    if (str == NULL)
        str = *saveptr;
    if (*str == '\0')
    {
        *saveptr = str;
        return NULL;
    }
    str += strspn(str, delim);
    if (*str == '\0')
    {
        *saveptr = str;
        return NULL;
    }
    end = str + strcspn(str, delim);
    if (*end == '\0')
    {
        *saveptr = end;
        return str;
    }
    *end = '\0';
    *saveptr = end + 1;
    return str;
}

size_t strspn(const char *str1, const char *str2)
{
    if (str1 == NULL || str2 == NULL)
        return 0;
    bool cset[256] = {0};
    while ((*str2) != '\0')
    {
        cset[*str2] = 1;
        ++str2;
    }
    int index = 0;
    while (str1[index] != '\0')
    {
        if (cset[str1[index]])
            index++;
        else
            break;
    }
    return index;
}

size_t strcspn(const char *str1, const char *str2)
{
    if (str1 == NULL || str2 == NULL)
        return 0;
    bool cset[256] = {0};
    while ((*str2) != '\0')
    {
        cset[*str2] = 1;
        ++str2;
    }
    int len = 0;
    while (str1[len] != '\0')
    {
        if (!cset[str1[len]])
            len++;
        else
            break;
    }
    return len;
}

char *strpbrk(const char *str1, const char *str2)
{
    typedef unsigned char uchar;

    if (str1 == NULL || str2 == NULL)
        return NULL;
    uchar cset[32] = {0};
    while ((*str2) != '\0')
    {
        uchar t = (uchar)*str2++;
        cset[t % 32] |= 1 << (t / 32);
    }
    while ((*str1) != '\0')
    {
        uchar t = (uchar)*str1;
        if (cset[t % 32] & (1 << (t / 32)))
        {
            return (char *)str1;
        }
        else
        {
            ++str1;
        }
    }
    return NULL;
}

char *strchr(const char *str, int c)
{
    if (str == NULL)
        return NULL;

    while (*str != '\0')
    {
        if (*str == c)
        {
            return str;
        }
        str++;
    }
    return NULL;
}

char *strrchr(const char *str, int c)
{
    if (str == NULL)
        return NULL;

    char *p_char = NULL;
    while (*str != '\0')
    {
        if (*str == (char)c)
        {
            p_char = (char *)str;
        }
        str++;
    }

    return p_char;
}