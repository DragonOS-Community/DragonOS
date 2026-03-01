#define FUSE_USE_VERSION 31
#define _GNU_SOURCE

#include <fuse.h>

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/types.h>
#include <unistd.h>

static char g_backing_dir[PATH_MAX];
static int g_cleanup_backing = 0;
static int g_verbose_log = -1;

static int demo_verbose_enabled(void) {
    if (g_verbose_log < 0) {
        const char *v = getenv("FUSE3_TEST_LOG");
        g_verbose_log = (v && v[0] && strcmp(v, "0") != 0) ? 1 : 0;
    }
    return g_verbose_log;
}

static void demo_logf(const char *fmt, ...) {
    if (!demo_verbose_enabled()) {
        return;
    }
    va_list ap;
    va_start(ap, fmt);
    fprintf(stderr, "[fuse3-demo] ");
    vfprintf(stderr, fmt, ap);
    fprintf(stderr, "\n");
    va_end(ap);
}

static int demo_sanitize_open_flags(int flags, int for_create) {
    int keep = O_ACCMODE | O_APPEND | O_NONBLOCK | O_DSYNC | O_DIRECT | O_LARGEFILE |
               O_DIRECTORY | O_NOFOLLOW | O_NOATIME | O_CLOEXEC;
#ifdef O_PATH
    keep |= O_PATH;
#endif
#ifdef O_SYNC
    keep |= O_SYNC;
#endif
#ifdef O_TRUNC
    keep |= O_TRUNC;
#endif
    if (for_create) {
        keep |= O_CREAT;
#ifdef O_EXCL
        keep |= O_EXCL;
#endif
    }
    return flags & keep;
}

static int demo_realpath(const char *path, char *buf, size_t buflen) {
    if (!path || path[0] != '/') {
        return -EINVAL;
    }
    int n = snprintf(buf, buflen, "%s%s", g_backing_dir, path);
    if (n < 0 || (size_t)n >= buflen) {
        return -ENAMETOOLONG;
    }
    return 0;
}

static void remove_tree(const char *root) {
    DIR *dir = opendir(root);
    if (!dir) {
        return;
    }

    struct dirent *ent;
    while ((ent = readdir(dir)) != NULL) {
        if (strcmp(ent->d_name, ".") == 0 || strcmp(ent->d_name, "..") == 0) {
            continue;
        }
        char full[PATH_MAX];
        int n = snprintf(full, sizeof(full), "%s/%s", root, ent->d_name);
        if (n < 0 || (size_t)n >= sizeof(full)) {
            continue;
        }

        struct stat st;
        if (lstat(full, &st) != 0) {
            continue;
        }

        if (S_ISDIR(st.st_mode)) {
            remove_tree(full);
            rmdir(full);
        } else {
            unlink(full);
        }
    }
    closedir(dir);
}

static int prepare_backing_dir(const char *custom) {
    const char *base = custom;
    char tmp[] = "/tmp/fuse3_demo_backing_XXXXXX";

    if (!base) {
        base = mkdtemp(tmp);
        if (!base) {
            return -errno;
        }
        g_cleanup_backing = 1;
    } else {
        struct stat st;
        if (stat(base, &st) != 0) {
            if (mkdir(base, 0755) != 0) {
                return -errno;
            }
        } else if (!S_ISDIR(st.st_mode)) {
            return -ENOTDIR;
        }
    }

    if (strlen(base) >= sizeof(g_backing_dir)) {
        return -ENAMETOOLONG;
    }
    strcpy(g_backing_dir, base);

    char hello_path[PATH_MAX];
    int err = demo_realpath("/hello.txt", hello_path, sizeof(hello_path));
    if (err) {
        return err;
    }

    int fd = open(hello_path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        return -errno;
    }

    const char *msg = "hello from libfuse3\n";
    ssize_t wn = write(fd, msg, strlen(msg));
    close(fd);
    if (wn != (ssize_t)strlen(msg)) {
        return -EIO;
    }
    return 0;
}

static int demo_getattr(const char *path, struct stat *stbuf, struct fuse_file_info *fi) {
    (void)fi;
    char full[PATH_MAX];
    int err = demo_realpath(path, full, sizeof(full));
    if (err) {
        return err;
    }
    if (lstat(full, stbuf) != 0) {
        return -errno;
    }
    return 0;
}

static int demo_access(const char *path, int mask) {
    char full[PATH_MAX];
    int err = demo_realpath(path, full, sizeof(full));
    if (err) {
        return err;
    }
    if (access(full, mask) != 0) {
        return -errno;
    }
    return 0;
}

static int demo_readlink(const char *path, char *buf, size_t size) {
    if (size == 0) {
        return -EINVAL;
    }
    char full[PATH_MAX];
    int err = demo_realpath(path, full, sizeof(full));
    if (err) {
        return err;
    }
    ssize_t len = readlink(full, buf, size - 1);
    if (len < 0) {
        return -errno;
    }
    buf[len] = '\0';
    return 0;
}

static int demo_readdir(const char *path, void *buf, fuse_fill_dir_t filler, off_t offset,
                        struct fuse_file_info *fi, enum fuse_readdir_flags flags) {
    (void)offset;
    (void)fi;
    (void)flags;

    char full[PATH_MAX];
    int err = demo_realpath(path, full, sizeof(full));
    if (err) {
        return err;
    }

    DIR *dir = opendir(full);
    if (!dir) {
        return -errno;
    }

    struct dirent *ent;
    while ((ent = readdir(dir)) != NULL) {
        struct stat st;
        memset(&st, 0, sizeof(st));
        st.st_ino = ent->d_ino;
        st.st_mode = (mode_t)(ent->d_type << 12);
        if (filler(buf, ent->d_name, &st, 0, 0) != 0) {
            break;
        }
    }
    closedir(dir);
    return 0;
}

static int demo_mknod(const char *path, mode_t mode, dev_t rdev) {
    char full[PATH_MAX];
    int err = demo_realpath(path, full, sizeof(full));
    if (err) {
        return err;
    }

    int ret;
    if (S_ISREG(mode)) {
        ret = open(full, O_CREAT | O_EXCL | O_WRONLY, mode);
        if (ret >= 0) {
            close(ret);
            ret = 0;
        }
    } else if (S_ISFIFO(mode)) {
        ret = mkfifo(full, mode);
    } else {
        ret = mknod(full, mode, rdev);
    }
    if (ret != 0) {
        return -errno;
    }
    return 0;
}

static int demo_mkdir(const char *path, mode_t mode) {
    char full[PATH_MAX];
    int err = demo_realpath(path, full, sizeof(full));
    if (err) {
        return err;
    }
    if (mkdir(full, mode) != 0) {
        return -errno;
    }
    return 0;
}

static int demo_unlink(const char *path) {
    char full[PATH_MAX];
    int err = demo_realpath(path, full, sizeof(full));
    if (err) {
        return err;
    }
    if (unlink(full) != 0) {
        return -errno;
    }
    return 0;
}

static int demo_rmdir(const char *path) {
    char full[PATH_MAX];
    int err = demo_realpath(path, full, sizeof(full));
    if (err) {
        return err;
    }
    if (rmdir(full) != 0) {
        return -errno;
    }
    return 0;
}

static int demo_symlink(const char *from, const char *to) {
    char full_to[PATH_MAX];
    int err = demo_realpath(to, full_to, sizeof(full_to));
    if (err) {
        return err;
    }
    if (symlink(from, full_to) != 0) {
        return -errno;
    }
    return 0;
}

static int demo_rename(const char *from, const char *to, unsigned int flags) {
    if (flags != 0) {
        return -EINVAL;
    }

    char full_from[PATH_MAX];
    char full_to[PATH_MAX];
    int err = demo_realpath(from, full_from, sizeof(full_from));
    if (err) {
        return err;
    }
    err = demo_realpath(to, full_to, sizeof(full_to));
    if (err) {
        return err;
    }

    if (rename(full_from, full_to) != 0) {
        return -errno;
    }
    return 0;
}

static int demo_link(const char *from, const char *to) {
    char full_from[PATH_MAX];
    char full_to[PATH_MAX];
    int err = demo_realpath(from, full_from, sizeof(full_from));
    if (err) {
        return err;
    }
    err = demo_realpath(to, full_to, sizeof(full_to));
    if (err) {
        return err;
    }
    if (link(full_from, full_to) != 0) {
        return -errno;
    }
    return 0;
}

static int demo_chmod(const char *path, mode_t mode, struct fuse_file_info *fi) {
    (void)fi;
    char full[PATH_MAX];
    int err = demo_realpath(path, full, sizeof(full));
    if (err) {
        return err;
    }
    if (chmod(full, mode) != 0) {
        return -errno;
    }
    return 0;
}

static int demo_chown(const char *path, uid_t uid, gid_t gid, struct fuse_file_info *fi) {
    (void)fi;
    char full[PATH_MAX];
    int err = demo_realpath(path, full, sizeof(full));
    if (err) {
        return err;
    }
    if (lchown(full, uid, gid) != 0) {
        return -errno;
    }
    return 0;
}

static int demo_truncate(const char *path, off_t size, struct fuse_file_info *fi) {
    if (fi != NULL) {
        if (ftruncate((int)fi->fh, size) != 0) {
            demo_logf("truncate fh=%llu size=%lld errno=%d", (unsigned long long)fi->fh,
                      (long long)size, errno);
            return -errno;
        }
        demo_logf("truncate fh=%llu size=%lld ok", (unsigned long long)fi->fh, (long long)size);
        return 0;
    }

    char full[PATH_MAX];
    int err = demo_realpath(path, full, sizeof(full));
    if (err) {
        return err;
    }
    if (truncate(full, size) != 0) {
        demo_logf("truncate path=%s size=%lld errno=%d", path, (long long)size, errno);
        return -errno;
    }
    demo_logf("truncate path=%s size=%lld ok", path, (long long)size);
    return 0;
}

static int demo_open(const char *path, struct fuse_file_info *fi) {
    char full[PATH_MAX];
    int err = demo_realpath(path, full, sizeof(full));
    if (err) {
        return err;
    }
    int open_flags = demo_sanitize_open_flags(fi->flags, 0);
    int fd = open(full, open_flags);
    if (fd < 0) {
        demo_logf("open path=%s flags=0x%x sanitized=0x%x errno=%d", path, fi->flags, open_flags,
                  errno);
        return -errno;
    }
    demo_logf("open path=%s flags=0x%x sanitized=0x%x fd=%d", path, fi->flags, open_flags, fd);
    fi->fh = (uint64_t)fd;
    return 0;
}

static int demo_create(const char *path, mode_t mode, struct fuse_file_info *fi) {
    char full[PATH_MAX];
    int err = demo_realpath(path, full, sizeof(full));
    if (err) {
        return err;
    }
    int create_flags = demo_sanitize_open_flags(fi->flags | O_CREAT, 1);
    int fd = open(full, create_flags, mode);
    if (fd < 0) {
        demo_logf("create path=%s flags=0x%x sanitized=0x%x mode=0%o errno=%d", path, fi->flags,
                  create_flags, (unsigned)mode, errno);
        return -errno;
    }
    demo_logf("create path=%s flags=0x%x sanitized=0x%x mode=0%o fd=%d", path, fi->flags,
              create_flags, (unsigned)mode, fd);
    fi->fh = (uint64_t)fd;
    return 0;
}

static int demo_read(const char *path, char *buf, size_t size, off_t offset,
                     struct fuse_file_info *fi) {
    (void)path;
    int fd = (int)fi->fh;
    ssize_t n = pread(fd, buf, size, offset);
    if (n < 0) {
        return -errno;
    }
    return (int)n;
}

static int demo_write(const char *path, const char *buf, size_t size, off_t offset,
                      struct fuse_file_info *fi) {
    (void)path;
    int fd = (int)fi->fh;
    ssize_t n = pwrite(fd, buf, size, offset);
    if (n < 0) {
        return -errno;
    }
    return (int)n;
}

static int demo_statfs(const char *path, struct statvfs *stbuf) {
    char full[PATH_MAX];
    int err = demo_realpath(path, full, sizeof(full));
    if (err) {
        return err;
    }
    if (statvfs(full, stbuf) != 0) {
        return -errno;
    }
    return 0;
}

static int demo_flush(const char *path, struct fuse_file_info *fi) {
    (void)path;
    if (fi == NULL) {
        return 0;
    }

    int dupfd = dup((int)fi->fh);
    if (dupfd < 0) {
        return -errno;
    }
    if (close(dupfd) != 0) {
        return -errno;
    }
    return 0;
}

static int demo_release(const char *path, struct fuse_file_info *fi) {
    (void)path;
    if (close((int)fi->fh) != 0) {
        return -errno;
    }
    return 0;
}

static int demo_fsync(const char *path, int isdatasync, struct fuse_file_info *fi) {
    (void)path;
    int ret = isdatasync ? fdatasync((int)fi->fh) : fsync((int)fi->fh);
    if (ret != 0) {
        return -errno;
    }
    return 0;
}

static int demo_fsyncdir(const char *path, int isdatasync, struct fuse_file_info *fi) {
    (void)path;
    (void)isdatasync;
    (void)fi;
    return 0;
}

static int demo_utimens(const char *path, const struct timespec tv[2], struct fuse_file_info *fi) {
    (void)fi;
    char full[PATH_MAX];
    int err = demo_realpath(path, full, sizeof(full));
    if (err) {
        return err;
    }
    if (utimensat(AT_FDCWD, full, tv, AT_SYMLINK_NOFOLLOW) != 0) {
        return -errno;
    }
    return 0;
}

static void *demo_init(struct fuse_conn_info *conn, struct fuse_config *cfg) {
    (void)cfg;
    fprintf(stderr, "fuse3_demo: INIT proto=%u.%u capable=0x%llx want=0x%llx\n", conn->proto_major,
            conn->proto_minor, (unsigned long long)conn->capable,
            (unsigned long long)conn->want);
    return NULL;
}

static struct fuse_operations g_ops = {
    .init = demo_init,
    .getattr = demo_getattr,
    .readlink = demo_readlink,
    .mknod = demo_mknod,
    .mkdir = demo_mkdir,
    .unlink = demo_unlink,
    .rmdir = demo_rmdir,
    .symlink = demo_symlink,
    .rename = demo_rename,
    .link = demo_link,
    .chmod = demo_chmod,
    .chown = demo_chown,
    .truncate = demo_truncate,
    .open = demo_open,
    .read = demo_read,
    .write = demo_write,
    .statfs = demo_statfs,
    .flush = demo_flush,
    .release = demo_release,
    .fsync = demo_fsync,
    .fsyncdir = demo_fsyncdir,
    .readdir = demo_readdir,
    .create = demo_create,
    .utimens = demo_utimens,
    .access = demo_access,
};

static void usage(const char *prog) {
    fprintf(stderr,
            "Usage: %s <mountpoint> [--backing-dir DIR] [--single] [--debug] [libfuse opts...]\n",
            prog);
}

int main(int argc, char **argv) {
    if (argc < 2) {
        usage(argv[0]);
        return 1;
    }

    const char *mountpoint = argv[1];
    const char *backing_dir = NULL;

    char **fuse_argv = calloc((size_t)argc + 4, sizeof(char *));
    if (!fuse_argv) {
        perror("calloc");
        return 1;
    }

    int fuse_argc = 0;
    fuse_argv[fuse_argc++] = argv[0];
    fuse_argv[fuse_argc++] = "-f";

    for (int i = 2; i < argc; i++) {
        if (strcmp(argv[i], "--backing-dir") == 0) {
            if (i + 1 >= argc) {
                usage(argv[0]);
                free(fuse_argv);
                return 1;
            }
            backing_dir = argv[++i];
            continue;
        }
        if (strcmp(argv[i], "--single") == 0) {
            fuse_argv[fuse_argc++] = "-s";
            continue;
        }
        if (strcmp(argv[i], "--debug") == 0) {
            fuse_argv[fuse_argc++] = "-d";
            continue;
        }
        fuse_argv[fuse_argc++] = argv[i];
    }

    fuse_argv[fuse_argc++] = (char *)mountpoint;

    int err = prepare_backing_dir(backing_dir);
    if (err != 0) {
        fprintf(stderr, "fuse3_demo: prepare backing dir failed: %s (%d)\n", strerror(-err), -err);
        free(fuse_argv);
        return 1;
    }

    fprintf(stderr, "fuse3_demo: mount=%s backing=%s\n", mountpoint, g_backing_dir);
    int ret = fuse_main(fuse_argc, fuse_argv, &g_ops, NULL);

    if (g_cleanup_backing) {
        remove_tree(g_backing_dir);
        rmdir(g_backing_dir);
    }
    free(fuse_argv);
    return ret;
}
