#pragma once

#include "DragonOS/stdint.h"
#include <stdbool.h>

typedef unsigned char u_char;
typedef unsigned short u_short;
typedef unsigned int u_int;
typedef unsigned long u_long;

typedef uint32_t uid_t;
typedef uint32_t gid_t;
typedef long long ssize_t;

typedef int64_t pid_t;
typedef __SIZE_TYPE__ size_t;

typedef char *caddr_t;

typedef int id_t;

typedef uint64_t ino_t;
typedef int64_t off_t;

typedef uint32_t blkcnt_t;
typedef uint32_t blksize_t;
typedef uint32_t dev_t;
typedef uint16_t mode_t;
typedef uint32_t nlink_t;

typedef int64_t time_t;
typedef uint32_t useconds_t;
typedef int32_t suseconds_t;
typedef uint32_t clock_t;

typedef uint64_t fsblkcnt_t;
typedef uint64_t fsfilcnt_t;

typedef uint64_t sector_t;

#define __socklen_t_defined
#define __socklen_t uint32_t
typedef __socklen_t socklen_t;

#define pgoff_t unsigned long

struct utimbuf
{
    time_t actime;
    time_t modtime;
};

typedef int pthread_t;
typedef int pthread_key_t;
typedef uint32_t pthread_once_t;

typedef struct __pthread_mutex_t
{
    uint32_t lock;
    pthread_t owner;
    int level;
    int type;
} pthread_mutex_t;

typedef void *pthread_attr_t;
typedef struct __pthread_mutexattr_t
{
    int type;
} pthread_mutexattr_t;

typedef struct __pthread_cond_t
{
    pthread_mutex_t *mutex;
    uint32_t value;
    int clockid; // clockid_t
} pthread_cond_t;

typedef uint64_t pthread_rwlock_t;
typedef void *pthread_rwlockattr_t;
typedef struct __pthread_spinlock_t
{
    int m_lock;
} pthread_spinlock_t;
typedef struct __pthread_condattr_t
{
    int clockid; // clockid_t
} pthread_condattr_t;

typedef uint64_t gfp_t;

// 定义8字节对齐变量属性
#ifndef __aligned_u64
    #define __aligned_u64 uint64_t __attribute__((aligned(8)))
#endif

#define aligned_u64 __aligned_u64