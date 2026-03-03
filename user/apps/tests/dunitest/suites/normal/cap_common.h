#pragma once

#include <errno.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <unistd.h>

#define _LINUX_CAPABILITY_VERSION_1 0x19980330u
#define _LINUX_CAPABILITY_VERSION_2 0x20071026u
#define _LINUX_CAPABILITY_VERSION_3 0x20080522u

#define _LINUX_CAPABILITY_U32S_1 1
#define _LINUX_CAPABILITY_U32S_2 2
#define _LINUX_CAPABILITY_U32S_3 2

typedef struct {
    uint32_t version;
    int32_t pid;
} cap_user_header_t;

typedef struct {
    uint32_t effective;
    uint32_t permitted;
    uint32_t inheritable;
} cap_user_data_t;

static inline uint64_t cap_words_to_u64(uint32_t lo, uint32_t hi) {
    return ((uint64_t)hi << 32) | (uint64_t)lo;
}

static inline uint64_t cap_effective_u64(const cap_user_data_t in[2]) {
    return cap_words_to_u64(in[0].effective, in[1].effective);
}

static inline uint64_t cap_permitted_u64(const cap_user_data_t in[2]) {
    return cap_words_to_u64(in[0].permitted, in[1].permitted);
}

static inline uint64_t cap_inheritable_u64(const cap_user_data_t in[2]) {
    return cap_words_to_u64(in[0].inheritable, in[1].inheritable);
}

static inline int capget_errno(uint32_t version, int32_t pid, cap_user_data_t* data) {
    cap_user_header_t hdr = {.version = version, .pid = pid};
    int ret = syscall(SYS_capget, &hdr, data);
    if (ret == -1) {
        return errno;
    }
    return 0;
}

static inline int capset_errno(uint32_t version, int32_t pid, cap_user_data_t* data) {
    cap_user_header_t hdr = {.version = version, .pid = pid};
    int ret = syscall(SYS_capset, &hdr, data);
    if (ret == -1) {
        return errno;
    }
    return 0;
}

static inline void fill_caps_v3(uint64_t e, uint64_t p, uint64_t i, cap_user_data_t out[2]) {
    out[0].effective = (uint32_t)(e & 0xFFFFFFFFu);
    out[0].permitted = (uint32_t)(p & 0xFFFFFFFFu);
    out[0].inheritable = (uint32_t)(i & 0xFFFFFFFFu);
    out[1].effective = (uint32_t)((e >> 32) & 0xFFFFFFFFu);
    out[1].permitted = (uint32_t)((p >> 32) & 0xFFFFFFFFu);
    out[1].inheritable = (uint32_t)((i >> 32) & 0xFFFFFFFFu);
}
