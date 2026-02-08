/*
 * Minimal FUSE userspace daemon for DragonOS kernel tests (no libfuse).
 *
 * This header provides a tiny in-memory filesystem and request handlers for
 * a subset of FUSE opcodes used by Phase C/D tests.
 */

#pragma once

#define _GNU_SOURCE

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#define FUSE_TEST_LOG_PREFIX "[fuse-test] "
#define FUSE_SIMPLEFS_REV "statfs-v1"

static inline int fuse_test_log_enabled(void) {
    static int inited = 0;
    static int enabled = 0;
    if (!inited) {
        const char *v = getenv("FUSE_TEST_LOG");
        enabled = (v && v[0] && strcmp(v, "0") != 0);
        inited = 1;
    }
    return enabled;
}

#define FUSE_TEST_LOG(fmt, ...)                                                       \
    do {                                                                               \
        if (fuse_test_log_enabled()) {                                                 \
            fprintf(stderr, FUSE_TEST_LOG_PREFIX fmt "\n", ##__VA_ARGS__);             \
        }                                                                              \
    } while (0)

#ifndef DT_DIR
#define DT_DIR 4
#endif
#ifndef DT_REG
#define DT_REG 8
#endif

/* Keep test buffers off small thread stacks. */
#define FUSE_TEST_BUF_SIZE (64 * 1024)

/* Opcodes (subset) */
#ifndef FUSE_LOOKUP
#define FUSE_LOOKUP 1
#endif
#ifndef FUSE_FORGET
#define FUSE_FORGET 2
#endif
#ifndef FUSE_GETATTR
#define FUSE_GETATTR 3
#endif
#ifndef FUSE_SETATTR
#define FUSE_SETATTR 4
#endif
#ifndef FUSE_MKNOD
#define FUSE_MKNOD 8
#endif
#ifndef FUSE_MKDIR
#define FUSE_MKDIR 9
#endif
#ifndef FUSE_UNLINK
#define FUSE_UNLINK 10
#endif
#ifndef FUSE_RMDIR
#define FUSE_RMDIR 11
#endif
#ifndef FUSE_RENAME
#define FUSE_RENAME 12
#endif
#ifndef FUSE_OPEN
#define FUSE_OPEN 14
#endif
#ifndef FUSE_READ
#define FUSE_READ 15
#endif
#ifndef FUSE_WRITE
#define FUSE_WRITE 16
#endif
#ifndef FUSE_STATFS
#define FUSE_STATFS 17
#endif
#ifndef FUSE_RELEASE
#define FUSE_RELEASE 18
#endif
#ifndef FUSE_INIT
#define FUSE_INIT 26
#endif
#ifndef FUSE_OPENDIR
#define FUSE_OPENDIR 27
#endif
#ifndef FUSE_READDIR
#define FUSE_READDIR 28
#endif
#ifndef FUSE_RELEASEDIR
#define FUSE_RELEASEDIR 29
#endif
#ifndef FUSE_DESTROY
#define FUSE_DESTROY 38
#endif

#ifndef FUSE_MIN_READ_BUFFER
#define FUSE_MIN_READ_BUFFER 8192
#endif

/* INIT flags (subset) */
#ifndef FUSE_INIT_EXT
#define FUSE_INIT_EXT (1u << 30)
#endif
#ifndef FUSE_MAX_PAGES
#define FUSE_MAX_PAGES (1u << 22)
#endif

/* setattr valid bits (subset) */
#ifndef FATTR_MODE
#define FATTR_MODE (1u << 0)
#endif
#ifndef FATTR_UID
#define FATTR_UID (1u << 1)
#endif
#ifndef FATTR_GID
#define FATTR_GID (1u << 2)
#endif
#ifndef FATTR_SIZE
#define FATTR_SIZE (1u << 3)
#endif

struct fuse_in_header {
    uint32_t len;
    uint32_t opcode;
    uint64_t unique;
    uint64_t nodeid;
    uint32_t uid;
    uint32_t gid;
    uint32_t pid;
    uint16_t total_extlen;
    uint16_t padding;
};

struct fuse_out_header {
    uint32_t len;
    int32_t error; /* -errno */
    uint64_t unique;
};

struct fuse_init_in {
    uint32_t major;
    uint32_t minor;
    uint32_t max_readahead;
    uint32_t flags;
    uint32_t flags2;
    uint32_t unused[11];
};

struct fuse_init_out {
    uint32_t major;
    uint32_t minor;
    uint32_t max_readahead;
    uint32_t flags;
    uint16_t max_background;
    uint16_t congestion_threshold;
    uint32_t max_write;
    uint32_t time_gran;
    uint16_t max_pages;
    uint16_t map_alignment;
    uint32_t flags2;
    uint32_t unused[7];
};

struct fuse_attr {
    uint64_t ino;
    uint64_t size;
    uint64_t blocks;
    uint64_t atime;
    uint64_t mtime;
    uint64_t ctime;
    uint32_t atimensec;
    uint32_t mtimensec;
    uint32_t ctimensec;
    uint32_t mode;
    uint32_t nlink;
    uint32_t uid;
    uint32_t gid;
    uint32_t rdev;
    uint32_t blksize;
    uint32_t flags;
};

struct fuse_entry_out {
    uint64_t nodeid;
    uint64_t generation;
    uint64_t entry_valid;
    uint64_t attr_valid;
    uint32_t entry_valid_nsec;
    uint32_t attr_valid_nsec;
    struct fuse_attr attr;
};

struct fuse_forget_in {
    uint64_t nlookup;
};

struct fuse_getattr_in {
    uint32_t getattr_flags;
    uint32_t dummy;
    uint64_t fh;
};

struct fuse_attr_out {
    uint64_t attr_valid;
    uint32_t attr_valid_nsec;
    uint32_t dummy;
    struct fuse_attr attr;
};

struct fuse_open_in {
    uint32_t flags;
    uint32_t open_flags;
};

struct fuse_open_out {
    uint64_t fh;
    uint32_t open_flags;
    uint32_t padding;
};

struct fuse_read_in {
    uint64_t fh;
    uint64_t offset;
    uint32_t size;
    uint32_t read_flags;
    uint64_t lock_owner;
    uint32_t flags;
    uint32_t padding;
};

struct fuse_write_in {
    uint64_t fh;
    uint64_t offset;
    uint32_t size;
    uint32_t write_flags;
    uint64_t lock_owner;
    uint32_t flags;
    uint32_t padding;
};

struct fuse_write_out {
    uint32_t size;
    uint32_t padding;
};

struct fuse_kstatfs {
    uint64_t blocks;
    uint64_t bfree;
    uint64_t bavail;
    uint64_t files;
    uint64_t ffree;
    uint32_t bsize;
    uint32_t namelen;
    uint32_t frsize;
    uint32_t padding;
    uint32_t spare[6];
};

struct fuse_statfs_out {
    struct fuse_kstatfs st;
};

struct fuse_release_in {
    uint64_t fh;
    uint32_t flags;
    uint32_t release_flags;
    uint64_t lock_owner;
};

struct fuse_mknod_in {
    uint32_t mode;
    uint32_t rdev;
    uint32_t umask;
    uint32_t padding;
};

struct fuse_mkdir_in {
    uint32_t mode;
    uint32_t umask;
};

struct fuse_rename_in {
    uint64_t newdir;
};

struct fuse_setattr_in {
    uint32_t valid;
    uint32_t padding;
    uint64_t fh;
    uint64_t size;
    uint64_t lock_owner;
    uint64_t atime;
    uint64_t mtime;
    uint64_t ctime;
    uint32_t atimensec;
    uint32_t mtimensec;
    uint32_t ctimensec;
    uint32_t mode;
    uint32_t unused4;
    uint32_t uid;
    uint32_t gid;
    uint32_t unused5;
};

struct fuse_dirent {
    uint64_t ino;
    uint64_t off;
    uint32_t namelen;
    uint32_t type;
    /* char name[]; */
};

static inline size_t fuse_dirent_rec_len(size_t namelen) {
    size_t unaligned = sizeof(struct fuse_dirent) + namelen;
    return (unaligned + 8 - 1) & ~(size_t)(8 - 1);
}

/* ===== in-memory FS ===== */

#define SIMPLEFS_MAX_NODES 64
#define SIMPLEFS_NAME_MAX 64
#define SIMPLEFS_DATA_MAX 8192

struct simplefs_node {
    int used;
    uint64_t nodeid;
    uint64_t ino;
    uint64_t parent;
    int is_dir;
    uint32_t mode; /* includes type bits */
    char name[SIMPLEFS_NAME_MAX];
    unsigned char data[SIMPLEFS_DATA_MAX];
    size_t size;
};

struct simplefs {
    struct simplefs_node nodes[SIMPLEFS_MAX_NODES];
    uint64_t next_nodeid;
    uint64_t next_ino;
};

static inline void simplefs_init(struct simplefs *fs) {
    memset(fs, 0, sizeof(*fs));
    fs->next_nodeid = 2;
    fs->next_ino = 2;

    /* root nodeid=1 */
    fs->nodes[0].used = 1;
    fs->nodes[0].nodeid = 1;
    fs->nodes[0].ino = 1;
    fs->nodes[0].parent = 1;
    fs->nodes[0].is_dir = 1;
    fs->nodes[0].mode = 0040755;
    strcpy(fs->nodes[0].name, "");
    fs->nodes[0].size = 0;

    /* hello.txt under root */
    fs->nodes[1].used = 1;
    fs->nodes[1].nodeid = 2;
    fs->nodes[1].ino = 2;
    fs->nodes[1].parent = 1;
    fs->nodes[1].is_dir = 0;
    fs->nodes[1].mode = 0100644;
    strcpy(fs->nodes[1].name, "hello.txt");
    const char *msg = "hello from fuse\n";
    fs->nodes[1].size = strlen(msg);
    memcpy(fs->nodes[1].data, msg, fs->nodes[1].size);

    fs->next_nodeid = 3;
    fs->next_ino = 3;
}

static inline struct simplefs_node *simplefs_find_node(struct simplefs *fs, uint64_t nodeid) {
    for (int i = 0; i < SIMPLEFS_MAX_NODES; i++) {
        if (fs->nodes[i].used && fs->nodes[i].nodeid == nodeid) {
            return &fs->nodes[i];
        }
    }
    return NULL;
}

static inline struct simplefs_node *simplefs_find_child(struct simplefs *fs, uint64_t parent,
                                                        const char *name) {
    for (int i = 0; i < SIMPLEFS_MAX_NODES; i++) {
        if (!fs->nodes[i].used)
            continue;
        if (fs->nodes[i].parent != parent)
            continue;
        if (strcmp(fs->nodes[i].name, name) == 0)
            return &fs->nodes[i];
    }
    return NULL;
}

static inline int simplefs_has_children(struct simplefs *fs, uint64_t parent) {
    for (int i = 0; i < SIMPLEFS_MAX_NODES; i++) {
        if (!fs->nodes[i].used)
            continue;
        if (fs->nodes[i].parent == parent)
            return 1;
    }
    return 0;
}

static inline struct simplefs_node *simplefs_alloc(struct simplefs *fs) {
    for (int i = 0; i < SIMPLEFS_MAX_NODES; i++) {
        if (!fs->nodes[i].used) {
            struct simplefs_node *n = &fs->nodes[i];
            memset(n, 0, sizeof(*n));
            n->used = 1;
            n->nodeid = fs->next_nodeid++;
            n->ino = fs->next_ino++;
            return n;
        }
    }
    return NULL;
}

static inline void simplefs_fill_attr(const struct simplefs_node *n, struct fuse_attr *a) {
    memset(a, 0, sizeof(*a));
    a->ino = n->ino;
    a->size = n->size;
    a->blocks = (n->size + 511) / 512;
    a->mode = n->mode;
    a->nlink = n->is_dir ? 2 : 1;
    a->uid = getuid();
    a->gid = getgid();
    a->blksize = 4096;
}

static inline int fuse_write_reply(int fd, uint64_t unique, int err_neg, const void *payload,
                                   size_t payload_len) {
    struct fuse_out_header out;
    memset(&out, 0, sizeof(out));
    out.len = sizeof(out) + (uint32_t)payload_len;
    out.error = err_neg;
    out.unique = unique;

    size_t total = sizeof(out) + payload_len;
    unsigned char *buf = malloc(total);
    if (!buf) {
        errno = ENOMEM;
        return -1;
    }
    memcpy(buf, &out, sizeof(out));
    if (payload_len) {
        memcpy(buf + sizeof(out), payload, payload_len);
    }
    ssize_t wn = write(fd, buf, total);
    free(buf);
    if (wn == (ssize_t)total) {
        FUSE_TEST_LOG("reply unique=%llu err=%d len=%zu",
                      (unsigned long long)unique, (int)err_neg, total);
    }
    if (wn != (ssize_t)total) {
        return -1;
    }
    return 0;
}

struct fuse_daemon_args {
    int fd;
    volatile int *stop;
    volatile int *init_done;
    int enable_write_ops;
    int exit_after_init;
    int stop_on_destroy;
    uint32_t root_mode_override;
    uint32_t hello_mode_override;
    volatile uint32_t *forget_count;
    volatile uint64_t *forget_nlookup_sum;
    volatile uint32_t *destroy_count;
    volatile uint32_t *init_in_flags;
    volatile uint32_t *init_in_flags2;
    volatile uint32_t *init_in_max_readahead;
    struct simplefs fs;
};

static inline int fuse_handle_one(struct fuse_daemon_args *a, const unsigned char *req, size_t n) {
    if (n < sizeof(struct fuse_in_header)) {
        return -1;
    }
    const struct fuse_in_header *h = (const struct fuse_in_header *)req;
    const unsigned char *payload = req + sizeof(*h);
    size_t payload_len = n - sizeof(*h);
    FUSE_TEST_LOG("handle opcode=%u unique=%llu nodeid=%llu len=%u payload=%zu",
                  h->opcode, (unsigned long long)h->unique, (unsigned long long)h->nodeid,
                  h->len, payload_len);

    switch (h->opcode) {
    case FUSE_INIT: {
        if (payload_len < sizeof(struct fuse_init_in)) {
            return -1;
        }
        const struct fuse_init_in *in = (const struct fuse_init_in *)payload;
        if (a->init_in_flags)
            *a->init_in_flags = in->flags;
        if (a->init_in_flags2)
            *a->init_in_flags2 = in->flags2;
        if (a->init_in_max_readahead)
            *a->init_in_max_readahead = in->max_readahead;

        struct fuse_init_out out;
        memset(&out, 0, sizeof(out));
        out.major = 7;
        out.minor = 39;
        out.flags = FUSE_INIT_EXT | FUSE_MAX_PAGES;
        out.flags2 = 0;
        out.max_write = 4096;
        out.max_pages = 32;
        if (fuse_write_reply(a->fd, h->unique, 0, &out, sizeof(out)) != 0) {
            return -1;
        }
        *a->init_done = 1;
        return 0;
    }
    case FUSE_FORGET: {
        if (payload_len < sizeof(struct fuse_forget_in))
            return -1;
        const struct fuse_forget_in *in = (const struct fuse_forget_in *)payload;
        if (a->forget_count)
            (*a->forget_count)++;
        if (a->forget_nlookup_sum)
            (*a->forget_nlookup_sum) += in->nlookup;
        return 0;
    }
    case FUSE_LOOKUP: {
        const char *name = (const char *)payload;
        if (payload_len == 0 || name[payload_len - 1] != '\0') {
            return -1;
        }
        struct simplefs_node *parent = simplefs_find_node(&a->fs, h->nodeid);
        if (!parent || !parent->is_dir) {
            return fuse_write_reply(a->fd, h->unique, -ENOENT, NULL, 0);
        }
        struct simplefs_node *child = simplefs_find_child(&a->fs, h->nodeid, name);
        if (!child) {
            return fuse_write_reply(a->fd, h->unique, -ENOENT, NULL, 0);
        }
        struct fuse_entry_out out;
        memset(&out, 0, sizeof(out));
        out.nodeid = child->nodeid;
        simplefs_fill_attr(child, &out.attr);
        return fuse_write_reply(a->fd, h->unique, 0, &out, sizeof(out));
    }
    case FUSE_GETATTR: {
        (void)payload;
        struct simplefs_node *node = simplefs_find_node(&a->fs, h->nodeid);
        if (!node) {
            return fuse_write_reply(a->fd, h->unique, -ENOENT, NULL, 0);
        }
        struct fuse_attr_out out;
        memset(&out, 0, sizeof(out));
        simplefs_fill_attr(node, &out.attr);
        return fuse_write_reply(a->fd, h->unique, 0, &out, sizeof(out));
    }
    case FUSE_OPENDIR:
    case FUSE_OPEN: {
        struct simplefs_node *node = simplefs_find_node(&a->fs, h->nodeid);
        if (!node) {
            return fuse_write_reply(a->fd, h->unique, -ENOENT, NULL, 0);
        }
        if (h->opcode == FUSE_OPENDIR && !node->is_dir) {
            return fuse_write_reply(a->fd, h->unique, -ENOTDIR, NULL, 0);
        }
        if (h->opcode == FUSE_OPEN && node->is_dir) {
            return fuse_write_reply(a->fd, h->unique, -EISDIR, NULL, 0);
        }
        struct fuse_open_out out;
        memset(&out, 0, sizeof(out));
        out.fh = node->nodeid;
        return fuse_write_reply(a->fd, h->unique, 0, &out, sizeof(out));
    }
    case FUSE_READ: {
        if (payload_len < sizeof(struct fuse_read_in)) {
            return -1;
        }
        const struct fuse_read_in *in = (const struct fuse_read_in *)payload;
        struct simplefs_node *node = simplefs_find_node(&a->fs, h->nodeid);
        if (!node || node->is_dir) {
            return fuse_write_reply(a->fd, h->unique, -EINVAL, NULL, 0);
        }
        if (in->offset >= node->size) {
            return fuse_write_reply(a->fd, h->unique, 0, NULL, 0);
        }
        size_t remain = node->size - (size_t)in->offset;
        size_t to_copy = in->size;
        if (to_copy > remain) {
            to_copy = remain;
        }
        return fuse_write_reply(a->fd, h->unique, 0, node->data + in->offset, to_copy);
    }
    case FUSE_READDIR: {
        if (payload_len < sizeof(struct fuse_read_in)) {
            return -1;
        }
        const struct fuse_read_in *in = (const struct fuse_read_in *)payload;
        (void)in;
        struct simplefs_node *node = simplefs_find_node(&a->fs, h->nodeid);
        if (!node || !node->is_dir) {
            return fuse_write_reply(a->fd, h->unique, -ENOTDIR, NULL, 0);
        }

        /* offset is an entry index: 0=".", 1="..", then children */
        uint64_t idx = in->offset;
        unsigned char *outbuf = malloc(FUSE_TEST_BUF_SIZE);
        if (!outbuf) {
            return fuse_write_reply(a->fd, h->unique, -ENOMEM, NULL, 0);
        }
        size_t outlen = 0;

        const char *fixed_names[2] = {".", ".."};
        for (; idx < 2; idx++) {
            const char *nm = fixed_names[idx];
            size_t nmlen = strlen(nm);
            size_t reclen = fuse_dirent_rec_len(nmlen);
            if (outlen + reclen > FUSE_TEST_BUF_SIZE)
                break;
            struct fuse_dirent de;
            memset(&de, 0, sizeof(de));
            de.ino = 1;
            de.off = idx + 1;
            de.namelen = (uint32_t)nmlen;
            de.type = DT_DIR;
            memcpy(outbuf + outlen, &de, sizeof(de));
            memcpy(outbuf + outlen + sizeof(de), nm, nmlen);
            outlen += reclen;
        }

        /* children in insertion order */
        uint64_t child_base = 2;
        uint64_t cur = idx;
        for (int i = 0; i < SIMPLEFS_MAX_NODES; i++) {
            struct simplefs_node *c = &a->fs.nodes[i];
            if (!c->used || c->parent != h->nodeid)
                continue;
            if (cur < child_base) {
                cur = child_base;
            }
            if (cur > child_base) {
                /* skip until we reach this entry index */
                child_base++;
                continue;
            }

            size_t nmlen = strlen(c->name);
            size_t reclen = fuse_dirent_rec_len(nmlen);
            if (outlen + reclen > FUSE_TEST_BUF_SIZE)
                break;
            struct fuse_dirent de;
            memset(&de, 0, sizeof(de));
            de.ino = c->ino;
            de.off = child_base + 1;
            de.namelen = (uint32_t)nmlen;
            de.type = c->is_dir ? DT_DIR : DT_REG;
            memcpy(outbuf + outlen, &de, sizeof(de));
            memcpy(outbuf + outlen + sizeof(de), c->name, nmlen);
            outlen += reclen;

            child_base++;
            cur++;
        }

        int ret = fuse_write_reply(a->fd, h->unique, 0, outbuf, outlen);
        free(outbuf);
        return ret;
    }
    case FUSE_STATFS: {
        struct fuse_statfs_out out;
        memset(&out, 0, sizeof(out));

        unsigned used = 0;
        for (int i = 0; i < SIMPLEFS_MAX_NODES; i++) {
            if (a->fs.nodes[i].used) {
                used++;
            }
        }

        out.st.blocks = 1024;
        out.st.bfree = 512;
        out.st.bavail = 512;
        out.st.files = SIMPLEFS_MAX_NODES;
        out.st.ffree = (used > SIMPLEFS_MAX_NODES) ? 0 : (SIMPLEFS_MAX_NODES - used);
        out.st.bsize = 4096;
        out.st.frsize = 4096;
        out.st.namelen = SIMPLEFS_NAME_MAX - 1;
        FUSE_TEST_LOG("statfs reply ok blocks=%llu bfree=%llu bavail=%llu",
                      (unsigned long long)out.st.blocks,
                      (unsigned long long)out.st.bfree,
                      (unsigned long long)out.st.bavail);
        return fuse_write_reply(a->fd, h->unique, 0, &out, sizeof(out));
    }
    case FUSE_RELEASE:
    case FUSE_RELEASEDIR:
        return fuse_write_reply(a->fd, h->unique, 0, NULL, 0);
    case FUSE_DESTROY:
        if (a->destroy_count)
            (*a->destroy_count)++;
        if (a->stop_on_destroy && a->stop)
            *a->stop = 1;
        return 0;
    case FUSE_WRITE: {
        if (!a->enable_write_ops) {
            return fuse_write_reply(a->fd, h->unique, -ENOSYS, NULL, 0);
        }
        if (payload_len < sizeof(struct fuse_write_in)) {
            return -1;
        }
        const struct fuse_write_in *in = (const struct fuse_write_in *)payload;
        const unsigned char *data = payload + sizeof(*in);
        size_t data_len = payload_len - sizeof(*in);
        if (data_len < in->size) {
            return -1;
        }
        struct simplefs_node *node = simplefs_find_node(&a->fs, h->nodeid);
        if (!node || node->is_dir) {
            return fuse_write_reply(a->fd, h->unique, -EINVAL, NULL, 0);
        }
        if (in->offset >= SIMPLEFS_DATA_MAX) {
            return fuse_write_reply(a->fd, h->unique, -EFBIG, NULL, 0);
        }
        size_t to_copy = in->size;
        if (in->offset + to_copy > SIMPLEFS_DATA_MAX) {
            to_copy = SIMPLEFS_DATA_MAX - (size_t)in->offset;
        }
        memcpy(node->data + in->offset, data, to_copy);
        if (node->size < in->offset + to_copy) {
            node->size = (size_t)in->offset + to_copy;
        }
        struct fuse_write_out out;
        memset(&out, 0, sizeof(out));
        out.size = (uint32_t)to_copy;
        return fuse_write_reply(a->fd, h->unique, 0, &out, sizeof(out));
    }
    case FUSE_MKDIR:
    case FUSE_MKNOD: {
        if (!a->enable_write_ops) {
            return fuse_write_reply(a->fd, h->unique, -ENOSYS, NULL, 0);
        }
        const char *name = NULL;
        size_t name_off = 0;
        int is_dir = (h->opcode == FUSE_MKDIR);
        uint32_t mode = 0;
        if (is_dir) {
            if (payload_len < sizeof(struct fuse_mkdir_in) + 1)
                return -1;
            const struct fuse_mkdir_in *in = (const struct fuse_mkdir_in *)payload;
            mode = in->mode;
            name_off = sizeof(*in);
        } else {
            if (payload_len < sizeof(struct fuse_mknod_in) + 1)
                return -1;
            const struct fuse_mknod_in *in = (const struct fuse_mknod_in *)payload;
            mode = in->mode;
            name_off = sizeof(*in);
        }
        name = (const char *)(payload + name_off);
        if (name[payload_len - name_off - 1] != '\0')
            return -1;
        if (simplefs_find_child(&a->fs, h->nodeid, name)) {
            return fuse_write_reply(a->fd, h->unique, -EEXIST, NULL, 0);
        }
        struct simplefs_node *p = simplefs_find_node(&a->fs, h->nodeid);
        if (!p || !p->is_dir) {
            return fuse_write_reply(a->fd, h->unique, -ENOTDIR, NULL, 0);
        }
        struct simplefs_node *nnode = simplefs_alloc(&a->fs);
        if (!nnode) {
            return fuse_write_reply(a->fd, h->unique, -ENOSPC, NULL, 0);
        }
        nnode->parent = h->nodeid;
        nnode->is_dir = is_dir;
        nnode->mode = mode;
        strncpy(nnode->name, name, sizeof(nnode->name) - 1);
        nnode->size = 0;

        struct fuse_entry_out out;
        memset(&out, 0, sizeof(out));
        out.nodeid = nnode->nodeid;
        simplefs_fill_attr(nnode, &out.attr);
        return fuse_write_reply(a->fd, h->unique, 0, &out, sizeof(out));
    }
    case FUSE_UNLINK:
    case FUSE_RMDIR: {
        if (!a->enable_write_ops) {
            return fuse_write_reply(a->fd, h->unique, -ENOSYS, NULL, 0);
        }
        const char *name = (const char *)payload;
        if (payload_len == 0 || name[payload_len - 1] != '\0') {
            return -1;
        }
        struct simplefs_node *child = simplefs_find_child(&a->fs, h->nodeid, name);
        if (!child) {
            return fuse_write_reply(a->fd, h->unique, -ENOENT, NULL, 0);
        }
        if (h->opcode == FUSE_RMDIR) {
            if (!child->is_dir) {
                return fuse_write_reply(a->fd, h->unique, -ENOTDIR, NULL, 0);
            }
            if (simplefs_has_children(&a->fs, child->nodeid)) {
                return fuse_write_reply(a->fd, h->unique, -ENOTEMPTY, NULL, 0);
            }
        } else {
            if (child->is_dir) {
                return fuse_write_reply(a->fd, h->unique, -EISDIR, NULL, 0);
            }
        }
        child->used = 0;
        return fuse_write_reply(a->fd, h->unique, 0, NULL, 0);
    }
    case FUSE_RENAME: {
        if (!a->enable_write_ops) {
            return fuse_write_reply(a->fd, h->unique, -ENOSYS, NULL, 0);
        }
        if (payload_len < sizeof(struct fuse_rename_in) + 3) {
            return -1;
        }
        const struct fuse_rename_in *in = (const struct fuse_rename_in *)payload;
        const char *names = (const char *)(payload + sizeof(*in));
        size_t names_len = payload_len - sizeof(*in);

        /* oldname\0newname\0 */
        const char *oldname = names;
        size_t oldlen = strnlen(oldname, names_len);
        if (oldlen == names_len)
            return -1;
        const char *newname = names + oldlen + 1;
        size_t remain = names_len - oldlen - 1;
        if (remain == 0)
            return -1;
        size_t newlen = strnlen(newname, remain);
        if (newlen == remain)
            return -1;

        struct simplefs_node *src = simplefs_find_child(&a->fs, h->nodeid, oldname);
        if (!src) {
            return fuse_write_reply(a->fd, h->unique, -ENOENT, NULL, 0);
        }
        if (simplefs_find_child(&a->fs, in->newdir, newname)) {
            return fuse_write_reply(a->fd, h->unique, -EEXIST, NULL, 0);
        }
        struct simplefs_node *dst_parent = simplefs_find_node(&a->fs, in->newdir);
        if (!dst_parent || !dst_parent->is_dir) {
            return fuse_write_reply(a->fd, h->unique, -ENOTDIR, NULL, 0);
        }
        src->parent = in->newdir;
        strncpy(src->name, newname, sizeof(src->name) - 1);
        return fuse_write_reply(a->fd, h->unique, 0, NULL, 0);
    }
    case FUSE_SETATTR: {
        if (!a->enable_write_ops) {
            return fuse_write_reply(a->fd, h->unique, -ENOSYS, NULL, 0);
        }
        if (payload_len < sizeof(struct fuse_setattr_in)) {
            return -1;
        }
        const struct fuse_setattr_in *in = (const struct fuse_setattr_in *)payload;
        struct simplefs_node *node = simplefs_find_node(&a->fs, h->nodeid);
        if (!node) {
            return fuse_write_reply(a->fd, h->unique, -ENOENT, NULL, 0);
        }
        if (in->valid & FATTR_SIZE) {
            if (in->size > SIMPLEFS_DATA_MAX) {
                return fuse_write_reply(a->fd, h->unique, -EFBIG, NULL, 0);
            }
            node->size = (size_t)in->size;
        }
        if (in->valid & FATTR_MODE) {
            node->mode = in->mode;
        }
        struct fuse_attr_out out;
        memset(&out, 0, sizeof(out));
        simplefs_fill_attr(node, &out.attr);
        return fuse_write_reply(a->fd, h->unique, 0, &out, sizeof(out));
    }
    default:
        return fuse_write_reply(a->fd, h->unique, -ENOSYS, NULL, 0);
    }
}

static inline void *fuse_daemon_thread(void *arg) {
    struct fuse_daemon_args *a = (struct fuse_daemon_args *)arg;
    unsigned char *buf = malloc(FUSE_TEST_BUF_SIZE);
    if (!buf) {
        return NULL;
    }

    simplefs_init(&a->fs);
    if (a->root_mode_override) {
        a->fs.nodes[0].mode = a->root_mode_override;
    }
    if (a->hello_mode_override) {
        a->fs.nodes[1].mode = a->hello_mode_override;
    }

    while (!*a->stop) {
        FUSE_TEST_LOG("daemon read start");
        ssize_t n = read(a->fd, buf, FUSE_TEST_BUF_SIZE);
        if (n < 0) {
            FUSE_TEST_LOG("daemon read error n=%zd errno=%d", n, errno);
            if (errno == EINTR)
                continue;
            if (errno == ENOTCONN)
                break;
            continue;
        }
        if (n == 0) {
            FUSE_TEST_LOG("daemon read EOF");
            break;
        }
        FUSE_TEST_LOG("daemon read n=%zd", n);
        struct fuse_in_header *h = (struct fuse_in_header *)buf;
        if ((size_t)n != h->len) {
            FUSE_TEST_LOG("daemon short read n=%zd hdr.len=%u", n, h->len);
            continue;
        }
        (void)fuse_handle_one(a, buf, (size_t)n);
        if (a->exit_after_init && a->init_done && *a->init_done) {
            break;
        }
    }
    free(buf);
    return NULL;
}

static inline int ensure_dir(const char *path) {
    struct stat st;
    if (stat(path, &st) == 0) {
        if (S_ISDIR(st.st_mode))
            return 0;
        errno = ENOTDIR;
        return -1;
    }
    return mkdir(path, 0755);
}
