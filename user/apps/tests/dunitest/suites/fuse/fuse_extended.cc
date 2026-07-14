#include <gtest/gtest.h>

#include <signal.h>
#include <setjmp.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <sys/xattr.h>

#include "fuse_gtest_common.h"

static sigjmp_buf g_fuse_sigbus_jmp;
static volatile sig_atomic_t g_fuse_sigbus_seen = 0;
static sigjmp_buf g_fuse_sigsegv_jmp;
static volatile sig_atomic_t g_fuse_sigsegv_seen = 0;

static void fuse_sigbus_longjmp_handler(int sig) {
    (void)sig;
    g_fuse_sigbus_seen = 1;
    siglongjmp(g_fuse_sigbus_jmp, 1);
}

static void fuse_sigsegv_longjmp_handler(int sig) {
    (void)sig;
    g_fuse_sigsegv_seen = 1;
    siglongjmp(g_fuse_sigsegv_jmp, 1);
}

#ifndef FUSE_DEV_IOC_CLONE
#define FUSE_DEV_IOC_CLONE 0x8004e500
#endif

#ifndef POSIX_FADV_NOREUSE
#define POSIX_FADV_NOREUSE 5
#endif

#ifndef XATTR_NAME_MAX
#define XATTR_NAME_MAX 255
#endif

#ifndef XATTR_SIZE_MAX
#define XATTR_SIZE_MAX 65536
#endif

static void fill_user_xattr_name(char *buf, size_t len) {
    memset(buf, 'a', len);
    memcpy(buf, "user.", strlen("user."));
    buf[len] = '\0';
}

static int ext_test_p2_ops() {
    const char *mp = "/tmp/test_fuse_p2_ops";
    int f = -1;
    int dfd = -1;
    ssize_t tn = -1;
    ssize_t rn = -1;
    char hello[256];
    char created[256];
    char symlink_path[256];
    char target_buf[256];
    char hard_path[256];
    char sparse_path[256];
    char rbuf[64];
    char dst_exist[256];
    char renamed[256];
    const char extension = 'X';
    const char sparse_marker = 'S';
    const off_t sparse_offset = 5000;
    const size_t sparse_size = (size_t)sparse_offset + 1;
    unsigned char *sparse_contents = NULL;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t access_count = 0;
    volatile uint32_t flush_count = 0;
    volatile uint32_t fsync_count = 0;
    volatile uint32_t fsyncdir_count = 0;
    volatile uint32_t create_count = 0;
    volatile uint32_t rename2_count = 0;
    volatile uint32_t write_count = 0;
    volatile uint32_t last_write_size = 0;
    volatile uint32_t last_write_flags = 0;
    volatile uint32_t write_count_at_fsync = 0;
    volatile uint32_t last_write_flags_at_fsync = 0;
    volatile unsigned char extension_write_byte = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.access_count = &access_count;
    args.flush_count = &flush_count;
    args.fsync_count = &fsync_count;
    args.fsyncdir_count = &fsyncdir_count;
    args.create_count = &create_count;
    args.rename2_count = &rename2_count;
    args.write_count = &write_count;
    args.last_write_size = &last_write_size;
    args.last_write_flags = &last_write_flags;
    args.write_count_at_fsync = &write_count_at_fsync;
    args.last_write_flags_at_fsync = &last_write_flags_at_fsync;
    args.write_watch_offset = 200;
    args.last_write_watch_byte = &extension_write_byte;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_WRITEBACK_CACHE;
    args.link_reuse_old_nodeid = 1;
    args.access_deny_mask = 2;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,allow_other", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    snprintf(hello, sizeof(hello), "%s/hello.txt", mp);
    if (access(hello, R_OK) != 0) {
        printf("[FAIL] access(R_OK): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (access(hello, W_OK) == 0 || errno != EACCES) {
        printf("[FAIL] access(W_OK) expected EACCES, errno=%d (%s)\n", errno, strerror(errno));
        goto fail;
    }

    snprintf(created, sizeof(created), "%s/p2_create.txt", mp);
    f = open(created, O_CREAT | O_RDWR, 0644);
    if (f < 0) {
        printf("[FAIL] open(O_CREAT): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (fuseg_write_all_fd(f, "p2-data") != 0) {
        printf("[FAIL] write created file: %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    // LINK returns an attribute snapshot for the same inode.  With
    // writeback-cache negotiated, that daemon-side size is stale until fsync;
    // processing the LINK reply must not roll back the local dirty size.
    snprintf(hard_path, sizeof(hard_path), "%s/p2_hard.txt", mp);
    if (link(created, hard_path) != 0) {
        printf("[FAIL] link dirty writeback file: %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    if (write_count != 0) {
        printf("[FAIL] writeback-cache write reached daemon before fsync: writes=%u\n",
               write_count);
        close(f);
        goto fail;
    }
    if (fsync(f) != 0) {
        printf("[FAIL] fsync(file): %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    if (write_count_at_fsync == 0 || last_write_size != strlen("p2-data") ||
        (last_write_flags_at_fsync & FUSE_WRITE_CACHE) == 0) {
        printf("[FAIL] fsync did not drain full cached write first: writes=%u size=%u flags=0x%x\n",
               write_count_at_fsync, last_write_size, last_write_flags_at_fsync);
        close(f);
        goto fail;
    }

    // Exercise writeback of an extension in the original EOF page. The
    // writeback length must be calculated from the extended local size.
    write_count = 0;
    last_write_size = 0;
    write_count_at_fsync = 0;
    if (pwrite(f, &extension, 1, 200) != 1) {
        printf("[FAIL] extend dirty writeback file: %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    if (fsync(f) != 0) {
        printf("[FAIL] fsync(extended file): %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    if (write_count_at_fsync == 0 || last_write_size != 201 ||
        extension_write_byte != (unsigned char)extension) {
        printf("[FAIL] fsync truncated extended cached page: writes=%u size=%u byte=%u\n",
               write_count_at_fsync, last_write_size, extension_write_byte);
        close(f);
        goto fail;
    }
    close(f);

    // A short daemon READ inside a locally extended sparse file denotes a
    // hole under FUSE_WRITEBACK_CACHE. It must neither shrink local i_size nor
    // discard the dirty page beyond the hole before writeback reaches daemon.
    snprintf(sparse_path, sizeof(sparse_path), "%s/p2_sparse.txt", mp);
    f = open(sparse_path, O_CREAT | O_RDWR, 0644);
    if (f < 0) {
        printf("[FAIL] open sparse file: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (pwrite(f, &sparse_marker, 1, sparse_offset) != 1) {
        printf("[FAIL] sparse cached extension: %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    sparse_contents = (unsigned char *)malloc(sparse_size);
    if (!sparse_contents) {
        printf("[FAIL] allocate sparse read buffer\n");
        close(f);
        goto fail;
    }
    memset(sparse_contents, 0xff, sparse_size);
    if (pread(f, sparse_contents, sparse_size, 0) != (ssize_t)sparse_size) {
        printf("[FAIL] read sparse extension before fsync: %s (errno=%d)\n", strerror(errno),
               errno);
        free(sparse_contents);
        close(f);
        goto fail;
    }
    for (size_t i = 0; i < sparse_size - 1; ++i) {
        if (sparse_contents[i] != 0) {
            printf("[FAIL] sparse hole byte %zu is %u\n", i, sparse_contents[i]);
            free(sparse_contents);
            close(f);
            goto fail;
        }
    }
    if (sparse_contents[sparse_size - 1] != (unsigned char)sparse_marker) {
        printf("[FAIL] sparse dirty tail lost before fsync: got=%u\n",
               sparse_contents[sparse_size - 1]);
        free(sparse_contents);
        close(f);
        goto fail;
    }
    free(sparse_contents);
    if (fsync(f) != 0) {
        printf("[FAIL] fsync sparse file: %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    close(f);

    if (unlink(hard_path) != 0) {
        printf("[FAIL] unlink dirty-link probe: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    snprintf(symlink_path, sizeof(symlink_path), "%s/p2_symlink.txt", mp);
    if (symlink("p2_create.txt", symlink_path) != 0) {
        printf("[FAIL] symlink: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    tn = readlink(symlink_path, target_buf, sizeof(target_buf) - 1);
    if (tn <= 0) {
        printf("[FAIL] readlink: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    target_buf[tn] = '\0';
    if (strcmp(target_buf, "p2_create.txt") != 0) {
        printf("[FAIL] readlink target mismatch: got=%s\n", target_buf);
        goto fail;
    }

    if (link(created, hard_path) != 0) {
        printf("[FAIL] link after fsync: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    if (unlink(created) != 0) {
        printf("[FAIL] unlink original: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    f = open(hard_path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open hard link: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    rn = read(f, rbuf, sizeof(rbuf) - 1);
    close(f);
    if (rn < (ssize_t)strlen("p2-data")) {
        printf("[FAIL] read hard link: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (memcmp(rbuf, "p2-data", strlen("p2-data")) != 0) {
        printf("[FAIL] hard link content prefix mismatch\n");
        goto fail;
    }

    snprintf(dst_exist, sizeof(dst_exist), "%s/p2_dst_exist.txt", mp);
    f = open(dst_exist, O_CREAT | O_RDWR, 0644);
    if (f < 0) {
        printf("[FAIL] create dst_exist: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(f);

    if (syscall(SYS_renameat2, AT_FDCWD, hard_path, AT_FDCWD, dst_exist, RENAME_NOREPLACE) == 0 ||
        errno != EEXIST) {
        printf("[FAIL] renameat2 NOREPLACE expected EEXIST, errno=%d (%s)\n", errno,
               strerror(errno));
        goto fail;
    }

    snprintf(renamed, sizeof(renamed), "%s/p2_renamed.txt", mp);
    if (syscall(SYS_renameat2, AT_FDCWD, hard_path, AT_FDCWD, renamed, RENAME_NOREPLACE) != 0) {
        printf("[FAIL] renameat2 NOREPLACE success path: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    dfd = open(mp, O_RDONLY | O_DIRECTORY);
    if (dfd < 0) {
        printf("[FAIL] open mountpoint dirfd: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (fsync(dfd) != 0) {
        printf("[FAIL] fsync(dirfd): %s (errno=%d)\n", strerror(errno), errno);
        close(dfd);
        goto fail;
    }
    close(dfd);

    usleep(100 * 1000);

    if (access_count < 2 || flush_count == 0 || fsync_count == 0 || fsyncdir_count == 0 ||
        create_count == 0 || rename2_count < 2) {
        printf("[FAIL] counters access=%u flush=%u fsync=%u fsyncdir=%u create=%u rename2=%u\n",
               access_count, flush_count, fsync_count, fsyncdir_count, create_count,
               rename2_count);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_positive_lookup_cache_respects_entry_ttl() {
    const char *mp = "/tmp/test_fuse_lookup_cache";
    char hello[256];
    char missing[256];
    struct stat st;
    char buf[32];

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t lookup_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.lookup_count = &lookup_count;
    args.entry_valid_sec = 60;
    args.attr_valid_sec = 60;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(hello, sizeof(hello), "%s/hello.txt", mp);
    for (int i = 0; i < 3; ++i) {
        if (stat(hello, &st) != 0) {
            printf("[FAIL] stat hello iteration %d: %s (errno=%d)\n", i, strerror(errno), errno);
            goto fail;
        }
        int f = open(hello, O_RDONLY);
        if (f < 0) {
            printf("[FAIL] open hello iteration %d: %s (errno=%d)\n", i, strerror(errno), errno);
            goto fail;
        }
        ssize_t n = read(f, buf, sizeof(buf));
        int saved_errno = errno;
        close(f);
        if (n <= 0) {
            errno = saved_errno;
            printf("[FAIL] read hello iteration %d: %s (errno=%d)\n", i, strerror(errno), errno);
            goto fail;
        }
    }

    if (lookup_count != 1) {
        printf("[FAIL] positive lookup cache expected 1 lookup, got %u\n", lookup_count);
        goto fail;
    }

    snprintf(missing, sizeof(missing), "%s/missing.txt", mp);
    for (int i = 0; i < 2; ++i) {
        if (stat(missing, &st) == 0 || errno != ENOENT) {
            printf("[FAIL] stat missing iteration %d expected ENOENT, errno=%d (%s)\n", i,
                   errno, strerror(errno));
            goto fail;
        }
    }

    if (lookup_count != 3) {
        printf("[FAIL] ordinary ENOENT should not be long-term cached, lookup_count=%u\n",
               lookup_count);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_xattr_ops() {
    const char *mp = "/tmp/test_fuse_xattr";
    char path[256];
    char list[64] = {};
    char small[4] = {};
    char value[64] = {};
    char name_255[XATTR_NAME_MAX + 1] = {};
    char name_256[XATTR_NAME_MAX + 2] = {};
    static char value_too_large[XATTR_SIZE_MAX + 1];
    static char max_xattr_buf[XATTR_SIZE_MAX + 1];
    ssize_t n = 0;
    uint32_t set_count_before = 0;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t getxattr_count = 0;
    volatile uint32_t setxattr_count = 0;
    volatile uint32_t listxattr_count = 0;
    volatile uint32_t removexattr_count = 0;
    volatile uint32_t last_setxattr_flags = UINT32_MAX;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.getxattr_count = &getxattr_count;
    args.setxattr_count = &setxattr_count;
    args.listxattr_count = &listxattr_count;
    args.removexattr_count = &removexattr_count;
    args.last_setxattr_flags = &last_setxattr_flags;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    errno = 0;
    n = listxattr(path, NULL, 0);
    if (n <= 0) {
        printf("[FAIL] listxattr size returned %zd errno=%d (%s)\n", n, errno, strerror(errno));
        goto fail;
    }
    n = listxattr(path, list, sizeof(list));
    if (n <= 0 || memcmp(list, "user.dragonos", sizeof("user.dragonos")) != 0) {
        printf("[FAIL] listxattr value n=%zd first='%s' errno=%d\n", n, list, errno);
        goto fail;
    }
    if (listxattr_count != 2) {
        printf("[FAIL] listxattr_count=%u expected=2\n", listxattr_count);
        goto fail;
    }

    args.force_listxattr_erange_at_max = 1;
    errno = 0;
    if (listxattr(path, max_xattr_buf, sizeof(max_xattr_buf)) != -1 || errno != E2BIG) {
        printf("[FAIL] listxattr max-size ERANGE errno=%d expected=%d\n", errno, E2BIG);
        goto fail;
    }
    if (listxattr_count != 3) {
        printf("[FAIL] listxattr max-size count=%u expected=3\n", listxattr_count);
        goto fail;
    }
    args.force_listxattr_erange_at_max = 0;

    n = getxattr(path, "user.dragonos", NULL, 0);
    if (n != (ssize_t)strlen("virtiofs-xattr")) {
        printf("[FAIL] getxattr size n=%zd errno=%d (%s)\n", n, errno, strerror(errno));
        goto fail;
    }
    errno = 0;
    if (getxattr(path, "user.dragonos", small, sizeof(small)) != -1 || errno != ERANGE) {
        printf("[FAIL] getxattr small buffer errno=%d expected=%d\n", errno, ERANGE);
        goto fail;
    }
    n = getxattr(path, "user.dragonos", value, sizeof(value));
    if (n != (ssize_t)strlen("virtiofs-xattr") ||
        memcmp(value, "virtiofs-xattr", strlen("virtiofs-xattr")) != 0) {
        printf("[FAIL] getxattr value n=%zd value='%s' errno=%d\n", n, value, errno);
        goto fail;
    }
    if (getxattr_count != 3) {
        printf("[FAIL] getxattr_count=%u expected=3\n", getxattr_count);
        goto fail;
    }

    args.force_getxattr_erange_at_max = 1;
    errno = 0;
    if (getxattr(path, "user.dragonos", max_xattr_buf, sizeof(max_xattr_buf)) != -1 ||
        errno != E2BIG) {
        printf("[FAIL] getxattr max-size ERANGE errno=%d expected=%d\n", errno, E2BIG);
        goto fail;
    }
    if (getxattr_count != 4) {
        printf("[FAIL] getxattr max-size count=%u expected=4\n", getxattr_count);
        goto fail;
    }
    args.force_getxattr_erange_at_max = 0;

    set_count_before = setxattr_count;
    errno = 0;
    if (setxattr(path, "user.dragonos", "new", 3, 0x4) != -1 || errno != EINVAL) {
        printf("[FAIL] setxattr invalid flags errno=%d expected=%d\n", errno, EINVAL);
        goto fail;
    }
    if (setxattr_count != set_count_before) {
        printf("[FAIL] invalid flags reached fuse daemon count=%u before=%u\n", setxattr_count,
               set_count_before);
        goto fail;
    }

    errno = 0;
    if (setxattr(path, "user.dragonos", value_too_large, sizeof(value_too_large), 0) != -1 ||
        errno != E2BIG) {
        printf("[FAIL] setxattr oversized value errno=%d expected=%d\n", errno, E2BIG);
        goto fail;
    }
    if (setxattr_count != set_count_before) {
        printf("[FAIL] oversized value reached fuse daemon count=%u before=%u\n", setxattr_count,
               set_count_before);
        goto fail;
    }

    errno = 0;
    if (setxattr(path, "", "new", 3, 0) != -1 || errno != ERANGE) {
        printf("[FAIL] setxattr empty name errno=%d expected=%d\n", errno, ERANGE);
        goto fail;
    }
    if (setxattr_count != set_count_before) {
        printf("[FAIL] empty name reached fuse daemon count=%u before=%u\n", setxattr_count,
               set_count_before);
        goto fail;
    }

    fill_user_xattr_name(name_255, XATTR_NAME_MAX);
    if (setxattr(path, name_255, "new", 3, 0) != 0) {
        printf("[FAIL] setxattr 255-byte name failed errno=%d (%s)\n", errno, strerror(errno));
        goto fail;
    }
    if (last_setxattr_flags != 0) {
        printf("[FAIL] setxattr 255-byte name flags=%u expected=0\n", last_setxattr_flags);
        goto fail;
    }
    set_count_before = setxattr_count;

    fill_user_xattr_name(name_256, XATTR_NAME_MAX + 1);
    errno = 0;
    if (setxattr(path, name_256, "new", 3, 0) != -1 || errno != ERANGE) {
        printf("[FAIL] setxattr 256-byte name errno=%d expected=%d\n", errno, ERANGE);
        goto fail;
    }
    if (setxattr_count != set_count_before) {
        printf("[FAIL] 256-byte name reached fuse daemon count=%u before=%u\n", setxattr_count,
               set_count_before);
        goto fail;
    }

    if (setxattr(path, "user.zero", nullptr, 0, 0) != 0) {
        printf("[FAIL] setxattr zero-size null value failed errno=%d (%s)\n", errno,
               strerror(errno));
        goto fail;
    }
    if (last_setxattr_flags != 0) {
        printf("[FAIL] setxattr zero-size null flags=%u expected=0\n", last_setxattr_flags);
        goto fail;
    }

    if (setxattr(path, "user.dragonos", "new", 3, 0) != 0) {
        printf("[FAIL] setxattr failed errno=%d (%s)\n", errno, strerror(errno));
        goto fail;
    }
    if (last_setxattr_flags != 0) {
        printf("[FAIL] setxattr flags=%u expected=0\n", last_setxattr_flags);
        goto fail;
    }
    errno = 0;
    if (setxattr(path, "user.dragonos", "new", 3, XATTR_CREATE) != -1 || errno != EEXIST) {
        printf("[FAIL] setxattr XATTR_CREATE errno=%d expected=%d\n", errno, EEXIST);
        goto fail;
    }
    if (last_setxattr_flags != XATTR_CREATE) {
        printf("[FAIL] setxattr flags=%u expected XATTR_CREATE=%d\n", last_setxattr_flags,
               XATTR_CREATE);
        goto fail;
    }
    if (setxattr(path, "user.created", "new", 3, XATTR_CREATE) != 0) {
        printf("[FAIL] setxattr XATTR_CREATE missing failed errno=%d (%s)\n", errno,
               strerror(errno));
        goto fail;
    }
    if (last_setxattr_flags != XATTR_CREATE) {
        printf("[FAIL] setxattr flags=%u expected missing XATTR_CREATE=%d\n",
               last_setxattr_flags, XATTR_CREATE);
        goto fail;
    }
    if (setxattr(path, "user.dragonos", "new", 3, XATTR_REPLACE) != 0) {
        printf("[FAIL] setxattr XATTR_REPLACE failed errno=%d (%s)\n", errno, strerror(errno));
        goto fail;
    }
    if (last_setxattr_flags != XATTR_REPLACE) {
        printf("[FAIL] setxattr flags=%u expected XATTR_REPLACE=%d\n", last_setxattr_flags,
               XATTR_REPLACE);
        goto fail;
    }
    errno = 0;
    if (setxattr(path, "user.missing", "new", 3, XATTR_REPLACE) != -1 || errno != ENODATA) {
        printf("[FAIL] setxattr XATTR_REPLACE missing errno=%d expected=%d\n", errno, ENODATA);
        goto fail;
    }
    if (last_setxattr_flags != XATTR_REPLACE) {
        printf("[FAIL] setxattr flags=%u expected missing XATTR_REPLACE=%d\n",
               last_setxattr_flags, XATTR_REPLACE);
        goto fail;
    }
    if (removexattr(path, "user.dragonos") != 0) {
        printf("[FAIL] removexattr failed errno=%d (%s)\n", errno, strerror(errno));
        goto fail;
    }
    if (setxattr_count != 7 || removexattr_count != 1) {
        printf("[FAIL] set/remove counts set=%u remove=%u\n", setxattr_count, removexattr_count);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_xattr_enosys_is_cached() {
    const char *mp = "/tmp/test_fuse_xattr_enosys";
    char path[256];

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t listxattr_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.listxattr_count = &listxattr_count;
    args.force_xattr_enosys = 1;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    for (int i = 0; i < 2; ++i) {
        errno = 0;
        if (listxattr(path, NULL, 0) != -1 ||
            (errno != EOPNOTSUPP && errno != ENOTSUP)) {
            printf("[FAIL] listxattr ENOSYS cache iter=%d errno=%d (%s)\n", i, errno,
                   strerror(errno));
            goto fail;
        }
    }
    if (listxattr_count != 1) {
        printf("[FAIL] listxattr ENOSYS should be cached, count=%u\n", listxattr_count);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static void ext_sigusr1_handler(int signo) {
    (void)signo;
}

struct ext_reader_ctx {
    char path[256];
    volatile int done;
    ssize_t nread;
    int err;
};

static void *ext_reader_thread(void *arg) {
    struct ext_reader_ctx *ctx = (struct ext_reader_ctx *)arg;
    int fd = open(ctx->path, O_RDONLY);
    if (fd < 0) {
        ctx->nread = -1;
        ctx->err = errno;
        ctx->done = 1;
        return NULL;
    }

    char buf[64];
    ssize_t n = read(fd, buf, sizeof(buf));
    if (n < 0) {
        ctx->nread = -1;
        ctx->err = errno;
    } else {
        ctx->nread = n;
        ctx->err = 0;
    }
    close(fd);
    ctx->done = 1;
    return NULL;
}

static int ext_test_p3_interrupt() {
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = ext_sigusr1_handler;
    sigemptyset(&sa.sa_mask);
    sa.sa_flags = 0;

    struct sigaction old_sa;
    if (sigaction(SIGUSR1, &sa, &old_sa) != 0) {
        printf("[FAIL] sigaction(SIGUSR1): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }

    const char *mp = "/tmp/test_fuse_p3_interrupt";
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        sigaction(SIGUSR1, &old_sa, NULL);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        sigaction(SIGUSR1, &old_sa, NULL);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t interrupt_count = 0;
    volatile uint64_t blocked_read_unique = 0;
    volatile uint64_t last_interrupt_header_unique = 0;
    volatile uint64_t last_interrupt_target = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 0;
    args.stop_on_destroy = 1;
    args.block_read_until_interrupt = 1000;
    args.interrupt_count = &interrupt_count;
    args.blocked_read_unique = &blocked_read_unique;
    args.last_interrupt_header_unique = &last_interrupt_header_unique;
    args.last_interrupt_target = &last_interrupt_target;

    pthread_t daemon_th;
    if (pthread_create(&daemon_th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create(daemon)\n");
        close(fd);
        rmdir(mp);
        sigaction(SIGUSR1, &old_sa, NULL);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(daemon_th, NULL);
        rmdir(mp);
        sigaction(SIGUSR1, &old_sa, NULL);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    struct ext_reader_ctx rctx;
    memset(&rctx, 0, sizeof(rctx));
    snprintf(rctx.path, sizeof(rctx.path), "%s/hello.txt", mp);

    pthread_t reader_th;
    if (pthread_create(&reader_th, NULL, ext_reader_thread, &rctx) != 0) {
        printf("[FAIL] pthread_create(reader)\n");
        goto fail;
    }

    for (int i = 0; i < 200; i++) {
        if (blocked_read_unique != 0) {
            break;
        }
        usleep(5 * 1000);
    }
    if (blocked_read_unique == 0) {
        printf("[FAIL] timed out waiting for blocked read request\n");
        stop = 1;
        pthread_join(reader_th, NULL);
        goto fail;
    }

    if (pthread_kill(reader_th, SIGUSR1) != 0) {
        printf("[FAIL] pthread_kill(SIGUSR1)\n");
        stop = 1;
        pthread_join(reader_th, NULL);
        goto fail;
    }
    pthread_join(reader_th, NULL);

    if (rctx.nread != -1 || rctx.err != EINTR) {
        printf("[FAIL] reader expected EINTR, nread=%zd err=%d (%s)\n", rctx.nread, rctx.err,
               strerror(rctx.err));
        goto fail;
    }

    for (int i = 0; i < 500; i++) {
        if (interrupt_count > 0) {
            break;
        }
        usleep(5 * 1000);
    }

    if (interrupt_count == 0) {
        printf("[FAIL] expected FUSE_INTERRUPT request\n");
        goto fail;
    }
    if (last_interrupt_target == 0 || last_interrupt_target != blocked_read_unique) {
        printf("[FAIL] interrupt target mismatch: blocked=%llu interrupt_target=%llu\n",
               (unsigned long long)blocked_read_unique, (unsigned long long)last_interrupt_target);
        goto fail;
    }
    if (last_interrupt_header_unique != (blocked_read_unique | 1ULL)) {
        printf("[FAIL] interrupt header unique mismatch: blocked=%llu header=%llu\n",
               (unsigned long long)blocked_read_unique,
               (unsigned long long)last_interrupt_header_unique);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(daemon_th, NULL);
    rmdir(mp);
    sigaction(SIGUSR1, &old_sa, NULL);
    return 0;

fail:
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(daemon_th, NULL);
    rmdir(mp);
    sigaction(SIGUSR1, &old_sa, NULL);
    return -1;
}

static int ext_test_p3_noopen_readdirplus_notify() {
    const char *mp = "/tmp/test_fuse_p3_noopen";
    ssize_t wn = -1;
    ssize_t verify_n = -1;
    int f = -1;
    uint32_t reads_before_inval = 0;
    uint32_t lookups_before_inval = 0;
    size_t entry_notify_len = 0;
    void *private_map = MAP_FAILED;
    char verify_buf[64];
    struct {
        struct fuse_out_header out;
        struct fuse_notify_inval_entry_out inval;
        char name[sizeof("hello.txt")];
    } entry_notify;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t opendir_count = 0;
    volatile uint32_t release_count = 0;
    volatile uint32_t releasedir_count = 0;
    volatile uint32_t readdirplus_count = 0;
    volatile uint32_t lookup_count = 0;
    volatile uint32_t read_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 0;
    args.stop_on_destroy = 1;
    args.open_count = &open_count;
    args.opendir_count = &opendir_count;
    args.release_count = &release_count;
    args.releasedir_count = &releasedir_count;
    args.readdirplus_count = &readdirplus_count;
    args.lookup_count = &lookup_count;
    args.read_count = &read_count;
    args.force_open_enosys = 1;
    args.force_opendir_enosys = 1;
    args.entry_valid_sec = 60;
    args.attr_valid_sec = 60;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_NO_OPEN_SUPPORT |
                                   FUSE_NO_OPENDIR_SUPPORT | FUSE_DO_READDIRPLUS;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char file_path[256];
    snprintf(file_path, sizeof(file_path), "%s/hello.txt", mp);
    for (int i = 0; i < 2; i++) {
        int f = open(file_path, O_RDONLY);
        if (f < 0) {
            printf("[FAIL] open(%s): %s (errno=%d)\n", file_path, strerror(errno), errno);
            goto fail;
        }
        char buf[64];
        ssize_t n = read(f, buf, sizeof(buf) - 1);
        close(f);
        if (n <= 0) {
            printf("[FAIL] read(%s): %s (errno=%d)\n", file_path, strerror(errno), errno);
            goto fail;
        }
    }

    f = open(file_path, O_RDONLY);
    if (f < 0 || read(f, verify_buf, sizeof(verify_buf)) <= 0) {
        printf("[FAIL] keep-open read before notify: %s (errno=%d)\n", strerror(errno), errno);
        if (f >= 0) close(f);
        goto fail;
    }
    private_map = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE, f, 0);
    if (private_map == MAP_FAILED) {
        printf("[FAIL] MAP_PRIVATE before notify: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    ((volatile char *)private_map)[0] = 'P';

    for (int i = 0; i < 2; i++) {
        DIR *dir = opendir(mp);
        if (!dir) {
            printf("[FAIL] opendir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
            goto fail;
        }
        int saw = 0;
        struct dirent *de;
        while ((de = readdir(dir)) != NULL) {
            if (strcmp(de->d_name, "hello.txt") == 0) {
                saw = 1;
            }
        }
        closedir(dir);
        if (!saw) {
            printf("[FAIL] readdir didn't see hello.txt\n");
            goto fail;
        }
    }

    struct {
        struct fuse_out_header out;
        struct fuse_notify_inval_inode_out inval;
    } notify_msg;
    memset(&notify_msg, 0, sizeof(notify_msg));
    notify_msg.out.len = sizeof(notify_msg);
    notify_msg.out.error = FUSE_NOTIFY_INVAL_INODE;
    notify_msg.out.unique = 0;
    notify_msg.inval.ino = 2;
    notify_msg.inval.off = 0;
    notify_msg.inval.len = -1;
    wn = write(fd, &notify_msg, sizeof(notify_msg));
    if (wn != (ssize_t)sizeof(notify_msg)) {
        printf("[FAIL] write notify: wn=%zd errno=%d (%s)\n", wn, errno, strerror(errno));
        goto fail;
    }

    usleep(100 * 1000);

    reads_before_inval = read_count;
    if (lseek(f, 0, SEEK_SET) < 0) {
        printf("[FAIL] lseek after inode notify: %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    verify_n = read(f, verify_buf, sizeof(verify_buf));
    close(f);
    f = -1;
    if (verify_n <= 0 || read_count <= reads_before_inval) {
        printf("[FAIL] inode notify did not force a fresh READ: before=%u after=%u n=%zd\n",
               reads_before_inval, read_count, verify_n);
        goto fail;
    }
    if (((volatile char *)private_map)[0] != 'P') {
        printf("[FAIL] inode notify discarded MAP_PRIVATE COW data\n");
        goto fail;
    }

    memset(&entry_notify, 0, sizeof(entry_notify));
    entry_notify_len = offsetof(decltype(entry_notify), name) + sizeof(entry_notify.name);
    entry_notify.out.len = entry_notify_len;
    entry_notify.out.error = FUSE_NOTIFY_INVAL_ENTRY;
    entry_notify.inval.parent = 1;
    entry_notify.inval.namelen = strlen("hello.txt");
    memcpy(entry_notify.name, "hello.txt", sizeof("hello.txt"));
    lookups_before_inval = lookup_count;
    wn = write(fd, &entry_notify, entry_notify_len);
    if (wn != (ssize_t)entry_notify_len) {
        printf("[FAIL] write entry notify: wn=%zd errno=%d (%s)\n", wn, errno,
               strerror(errno));
        goto fail;
    }
    f = open(file_path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open after entry notify: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(f);
    if (lookup_count <= lookups_before_inval) {
        printf("[FAIL] entry notify did not force a fresh LOOKUP: before=%u after=%u\n",
               lookups_before_inval, lookup_count);
        goto fail;
    }

    if (open_count != 1 || opendir_count != 1 || release_count != 0 || releasedir_count != 0 ||
        readdirplus_count == 0) {
        printf("[FAIL] counters open=%u opendir=%u release=%u releasedir=%u readdirplus=%u\n",
               open_count, opendir_count, release_count, releasedir_count, readdirplus_count);
        goto fail;
    }

    munmap(private_map, 4096);
    private_map = MAP_FAILED;
    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (private_map != MAP_FAILED) {
        munmap(private_map, 4096);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_open_zero_fh_valid() {
    const char *mp = "/tmp/test_fuse_zero_fh";
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t read_count = 0;
    volatile uint64_t last_open_fh = UINT64_MAX;
    volatile uint64_t last_read_fh = UINT64_MAX;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.open_count = &open_count;
    args.read_count = &read_count;
    args.last_open_fh = &last_open_fh;
    args.last_read_fh = &last_read_fh;
    args.has_hello_open_fh_override = 1;
    args.hello_open_fh_override = 0;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    char buf[128];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    if (fuseg_read_file_cstr(path, buf, sizeof(buf)) < 0) {
        printf("[FAIL] read(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if (strcmp(buf, "hello from fuse\n") != 0) {
        printf("[FAIL] content mismatch: got='%s'\n", buf);
        goto fail;
    }

    usleep(100 * 1000);
    if (open_count == 0 || read_count == 0 || last_open_fh != 0 || last_read_fh != 0) {
        printf("[FAIL] fh counters open=%u read=%u open_fh=%llu read_fh=%llu\n", open_count,
               read_count, (unsigned long long)last_open_fh, (unsigned long long)last_read_fh);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_noopen_fsync_uses_zero_fh() {
    const char *mp = "/tmp/test_fuse_noopen_fsync";
    int f = -1;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t fsync_count = 0;
    volatile uint32_t release_count = 0;
    volatile uint64_t last_fsync_fh = UINT64_MAX;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.open_count = &open_count;
    args.fsync_count = &fsync_count;
    args.release_count = &release_count;
    args.last_fsync_fh = &last_fsync_fh;
    args.force_open_enosys = 1;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_NO_OPEN_SUPPORT;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if (fsync(f) != 0) {
        printf("[FAIL] fsync(no-open file): %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    close(f);

    usleep(100 * 1000);
    if (open_count != 1 || fsync_count == 0 || release_count != 0 || last_fsync_fh != 0) {
        printf("[FAIL] counters open=%u fsync=%u release=%u fsync_fh=%llu\n", open_count,
               fsync_count, release_count, (unsigned long long)last_fsync_fh);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_fsync_enosys_cached_success() {
    const char *mp = "/tmp/test_fuse_fsync_enosys";
    int f = -1;
    int dfd = -1;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t fsync_count = 0;
    volatile uint32_t fsyncdir_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.fsync_count = &fsync_count;
    args.fsyncdir_count = &fsyncdir_count;
    args.force_fsync_errno = ENOSYS;
    args.force_fsyncdir_errno = ENOSYS;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if (fsync(f) != 0 || fsync(f) != 0) {
        printf("[FAIL] fsync(file ENOSYS cache): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(f);
    f = -1;

    dfd = open(mp, O_RDONLY | O_DIRECTORY);
    if (dfd < 0) {
        printf("[FAIL] open dirfd(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }
    if (fsync(dfd) != 0 || fsync(dfd) != 0) {
        printf("[FAIL] fsync(dir ENOSYS cache): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(dfd);
    dfd = -1;

    if (fsync_count != 1 || fsyncdir_count != 1) {
        printf("[FAIL] ENOSYS fsync cache counters fsync=%u fsyncdir=%u\n", fsync_count,
               fsyncdir_count);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    if (dfd >= 0) {
        close(dfd);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_open_release_flags_match_linux() {
    const char *mp = "/tmp/test_fuse_open_flags";
    int requested = O_RDWR | O_NOCTTY | O_TRUNC | O_APPEND | O_NONBLOCK;
    uint32_t expected_open = (uint32_t)(requested & ~(O_CREAT | O_EXCL | O_NOCTTY));
    int f = -1;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t last_open_flags = 0;
    volatile uint32_t last_release_flags = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.last_open_in_flags = &last_open_flags;
    args.last_release_in_flags = &last_release_flags;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_ATOMIC_O_TRUNC;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, requested);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    close(f);

    usleep(100 * 1000);
    if (last_open_flags != expected_open) {
        printf("[FAIL] open flags got=0%o expected=0%o\n", last_open_flags, expected_open);
        goto fail;
    }
    if (last_release_flags != (uint32_t)requested) {
        printf("[FAIL] release flags got=0%o expected=0%o\n", last_release_flags,
               (uint32_t)requested);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_create_reuses_fuse_handle() {
    const char *mp = "/tmp/test_fuse_create_handle";
    const uint64_t create_fh = 0xcafe2019ULL;
    int requested = O_CREAT | O_RDWR | O_TRUNC | O_APPEND | O_NONBLOCK | O_NOCTTY | O_CLOEXEC;
    uint32_t expected_create = (uint32_t)(requested & ~(O_NOCTTY | O_CLOEXEC));
    int f = -1;
    if (ensure_dir(mp) != 0) return -1;
    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0, init_done = 0;
    volatile uint32_t create_count = 0, open_count = 0, write_count = 0;
    volatile uint32_t flush_count = 0, release_count = 0, setattr_count = 0, create_flags = 0;
    volatile uint64_t write_fh = 0, flush_fh = 0, release_fh = 0;
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.create_count = &create_count;
    args.open_count = &open_count;
    args.write_count = &write_count;
    args.flush_count = &flush_count;
    args.release_count = &release_count;
    args.setattr_count = &setattr_count;
    args.last_create_in_flags = &create_flags;
    args.last_write_fh = &write_fh;
    args.last_flush_fh = &flush_fh;
    args.last_release_fh = &release_fh;
    args.create_open_fh_override = create_fh;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_ATOMIC_O_TRUNC;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) goto fail_no_thread;
    char opts[256], path[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) goto fail_thread;
    if (fuseg_wait_init(&init_done) != 0) goto fail;
    snprintf(path, sizeof(path), "%s/new.txt", mp);
    f = open(path, requested, 0644);
    if (f < 0 || write(f, "x", 1) != 1) goto fail;
    close(f);
    f = -1;
    for (int i = 0; i < 200 && release_count < 1; i++) {
        usleep(10 * 1000);
    }

    if (create_count != 1 || open_count != 0 || write_count != 1 || flush_count != 1 ||
        release_count != 1 || setattr_count != 0 || create_flags != expected_create ||
        write_fh != create_fh || flush_fh != create_fh || release_fh != create_fh) {
        printf("[FAIL] create handle reuse create=%u open=%u write=%u flush=%u release=%u "
               "flags=0%o expected=0%o fhs=%llx/%llx/%llx\n",
               create_count, open_count, write_count, flush_count, release_count, create_flags,
               expected_create, (unsigned long long)write_fh, (unsigned long long)flush_fh,
               (unsigned long long)release_fh);
        goto fail;
    }
    if (unlink(path) != 0) goto fail;
    if (umount(mp) != 0) goto fail_no_umount;
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) close(f);
    umount(mp);
fail_no_umount:
    stop = 1;
fail_thread:
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
fail_no_thread:
    close(fd);
    rmdir(mp);
    return -1;
}

static int ext_test_create_enosys_falls_back_and_caches() {
    const char *mp = "/tmp/test_fuse_create_enosys";
    if (ensure_dir(mp) != 0) return -1;
    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        rmdir(mp);
        return -1;
    }
    volatile int stop = 0, init_done = 0;
    volatile uint32_t create_count = 0, mknod_count = 0, open_count = 0, release_count = 0;
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.force_create_errno = ENOSYS;
    args.create_count = &create_count;
    args.mknod_count = &mknod_count;
    args.open_count = &open_count;
    args.release_count = &release_count;
    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) goto fail_no_thread;
    char opts[256], path[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) goto fail_thread;
    if (fuseg_wait_init(&init_done) != 0) goto fail;
    for (int i = 0; i < 2; i++) {
        snprintf(path, sizeof(path), "%s/fallback-%d", mp, i);
        int f = open(path, O_CREAT | O_RDWR, 0644);
        if (f < 0) goto fail;
        close(f);
    }
    for (int i = 0; i < 200 && release_count < 2; i++) {
        usleep(10 * 1000);
    }
    if (create_count != 1 || mknod_count != 2 || open_count != 2 || release_count != 2) {
        printf("[FAIL] create ENOSYS cache create=%u mknod=%u open=%u release=%u\n",
               create_count, mknod_count, open_count, release_count);
        goto fail;
    }
    if (umount(mp) != 0) goto fail_no_umount;
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;
fail:
    umount(mp);
fail_no_umount:
    stop = 1;
fail_thread:
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
fail_no_thread:
    close(fd);
    rmdir(mp);
    return -1;
}

static int ext_test_invalid_create_reply_cleans_resources() {
    const char *mp = "/tmp/test_fuse_create_cleanup";
    const uint64_t create_fh = 0xbad2019ULL;
    int bad_fd = -1;
    if (ensure_dir(mp) != 0) return -1;
    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        rmdir(mp);
        return -1;
    }
    volatile int stop = 0, init_done = 0;
    volatile uint32_t create_count = 0, open_count = 0, release_count = 0, forget_count = 0;
    volatile uint64_t release_fh = 0, forget_nlookup = 0;
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.create_count = &create_count;
    args.open_count = &open_count;
    args.release_count = &release_count;
    args.forget_count = &forget_count;
    args.forget_nlookup_sum = &forget_nlookup;
    args.last_release_fh = &release_fh;
    args.create_open_fh_override = create_fh;
    args.create_reply_mode_override = S_IFDIR | 0755;
    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) goto fail_no_thread;
    char opts[256], path[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) goto fail_thread;
    if (fuseg_wait_init(&init_done) != 0) goto fail;
    snprintf(path, sizeof(path), "%s/invalid", mp);
    errno = 0;
    bad_fd = open(path, O_CREAT | O_RDWR, 0644);
    if (bad_fd >= 0) {
        close(bad_fd);
        goto fail;
    }
    if (errno != EIO) goto fail;
    for (int i = 0; i < 200 && (release_count < 1 || forget_count < 1); i++) {
        usleep(10 * 1000);
    }
    if (create_count != 1 || open_count != 0 || release_count != 1 || release_fh != create_fh ||
        forget_count != 1 || forget_nlookup != 1) {
        printf("[FAIL] invalid CREATE cleanup create=%u open=%u release=%u fh=%llx "
               "forget=%u nlookup=%llu\n",
               create_count, open_count, release_count, (unsigned long long)release_fh,
               forget_count, (unsigned long long)forget_nlookup);
        goto fail;
    }
    if (umount(mp) != 0) goto fail_no_umount;
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;
fail:
    umount(mp);
fail_no_umount:
    stop = 1;
fail_thread:
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
fail_no_thread:
    close(fd);
    rmdir(mp);
    return -1;
}

static int ext_test_fsetfl_updates_fuse_io_flags() {
    const char *mp = "/tmp/test_fuse_fsetfl_flags";
    int requested = O_RDWR;
    int f = -1;
    int old_flags = -1;
    uint32_t expected_open = (uint32_t)requested;
    uint32_t expected_setfl = 0;
    char buf[8];
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t read_count = 0;
    volatile uint32_t write_count = 0;
    volatile uint32_t flush_count = 0;
    volatile uint32_t release_count = 0;
    volatile uint32_t last_open_flags = 0;
    volatile uint32_t last_read_flags = 0;
    volatile uint32_t last_write_flags = 0;
    volatile uint32_t last_flush_uid = UINT32_MAX;
    volatile uint32_t last_flush_gid = UINT32_MAX;
    volatile uint32_t last_flush_pid = 0;
    volatile uint32_t last_release_flags = 0;
    volatile uint32_t last_release_uid = UINT32_MAX;
    volatile uint32_t last_release_gid = UINT32_MAX;
    volatile uint32_t last_release_pid = UINT32_MAX;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.open_count = &open_count;
    args.read_count = &read_count;
    args.write_count = &write_count;
    args.flush_count = &flush_count;
    args.release_count = &release_count;
    args.last_open_in_flags = &last_open_flags;
    args.last_read_open_flags = &last_read_flags;
    args.last_write_open_flags = &last_write_flags;
    args.last_flush_uid = &last_flush_uid;
    args.last_flush_gid = &last_flush_gid;
    args.last_flush_pid = &last_flush_pid;
    args.last_release_in_flags = &last_release_flags;
    args.last_release_uid = &last_release_uid;
    args.last_release_gid = &last_release_gid;
    args.last_release_pid = &last_release_pid;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, requested);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }

    old_flags = fcntl(f, F_GETFL);
    if (old_flags < 0) {
        printf("[FAIL] fcntl(F_GETFL): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (fcntl(f, F_SETFL, old_flags | O_NONBLOCK) != 0) {
        printf("[FAIL] fcntl(F_SETFL): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    memset(buf, 0, sizeof(buf));
    if (read(f, buf, 5) != 5 || memcmp(buf, "hello", 5) != 0) {
        printf("[FAIL] read after F_SETFL got='%.*s' errno=%d\n", 5, buf, errno);
        goto fail;
    }
    if (write(f, "X", 1) != 1) {
        printf("[FAIL] write after F_SETFL: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(f);
    f = -1;
    usleep(100 * 1000);

    expected_setfl = (uint32_t)(old_flags | O_NONBLOCK);
    if (open_count != 1 || read_count != 1 || write_count != 1 || flush_count != 1 ||
        release_count != 1) {
        printf("[FAIL] counters open=%u read=%u write=%u flush=%u release=%u\n", open_count,
               read_count, write_count, flush_count, release_count);
        goto fail;
    }
    if (last_open_flags != expected_open) {
        printf("[FAIL] open flags got=0%o expected=0%o\n", last_open_flags, expected_open);
        goto fail;
    }
    if ((last_read_flags & O_NONBLOCK) == 0 || last_write_flags != expected_setfl ||
        last_release_flags != expected_setfl) {
        printf("[FAIL] updated flags read=0%o write=0%o release=0%o expected=0%o\n",
               last_read_flags, last_write_flags, last_release_flags, expected_setfl);
        goto fail;
    }
    if (last_flush_uid != 0 || last_flush_gid != 0 || last_flush_pid == 0) {
        printf("[FAIL] flush should use caller credentials uid=%u gid=%u pid=%u\n",
               last_flush_uid, last_flush_gid, last_flush_pid);
        goto fail;
    }
    if (last_release_uid != 0 || last_release_gid != 0 || last_release_pid != 0) {
        printf("[FAIL] release should use nocreds uid=%u gid=%u pid=%u\n", last_release_uid,
               last_release_gid, last_release_pid);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_fsetfl_updates_fuse_dev_nonblock() {
    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }

    int old_flags = fcntl(fd, F_GETFL);
    if (old_flags < 0) {
        printf("[FAIL] fcntl(F_GETFL): %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        return -1;
    }
    if ((old_flags & O_NONBLOCK) != 0) {
        printf("[FAIL] /dev/fuse unexpectedly opened nonblocking: flags=0%o\n", old_flags);
        close(fd);
        return -1;
    }
    if (fcntl(fd, F_SETFL, old_flags | O_NONBLOCK) != 0) {
        printf("[FAIL] fcntl(F_SETFL O_NONBLOCK): %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        return -1;
    }

    pid_t child = fork();
    if (child < 0) {
        printf("[FAIL] fork: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        return -1;
    }
    if (child == 0) {
        unsigned char *buf = (unsigned char *)malloc(FUSE_TEST_BUF_SIZE);
        if (!buf) {
            _exit(11);
        }
        ssize_t n = read(fd, buf, FUSE_TEST_BUF_SIZE);
        int saved_errno = errno;
        free(buf);
        if (n < 0 && (saved_errno == EAGAIN || saved_errno == EWOULDBLOCK)) {
            _exit(0);
        }
        _exit(12);
    }

    for (int i = 0; i < 50; i++) {
        int status = 0;
        pid_t got = waitpid(child, &status, WNOHANG);
        if (got == child) {
            close(fd);
            if (WIFEXITED(status) && WEXITSTATUS(status) == 0) {
                return 0;
            }
            printf("[FAIL] child read did not return EAGAIN, status=%d\n", status);
            return -1;
        }
        if (got < 0) {
            printf("[FAIL] waitpid: %s (errno=%d)\n", strerror(errno), errno);
            close(fd);
            return -1;
        }
        usleep(20 * 1000);
    }

    kill(child, SIGKILL);
    waitpid(child, NULL, 0);
    close(fd);
    printf("[FAIL] /dev/fuse read blocked after F_SETFL O_NONBLOCK\n");
    return -1;
}

static int ext_test_fopen_noflush_skips_flush() {
    const char *mp = "/tmp/test_fuse_noflush";
    int f = -1;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t flush_count = 0;
    volatile uint32_t release_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.flush_count = &flush_count;
    args.release_count = &release_count;
    args.hello_open_out_flags = FOPEN_NOFLUSH;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    close(f);
    f = -1;

    usleep(100 * 1000);
    if (flush_count != 0 || release_count != 1) {
        printf("[FAIL] noflush counters flush=%u release=%u\n", flush_count, release_count);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_close_returns_flush_error_and_closes_fd() {
    const char *mp = "/tmp/test_fuse_close_flush_error";
    int f = -1;
    int oldfd = -1;
    int rc = 0;
    char tmp = 0;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t flush_count = 0;
    volatile uint32_t release_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.flush_count = &flush_count;
    args.release_count = &release_count;
    args.force_flush_errno = EIO;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }

    errno = 0;
    oldfd = f;
    rc = close(f);
    f = -1;
    if (rc != -1 || errno != EIO) {
        printf("[FAIL] close should return EIO rc=%d errno=%d\n", rc, errno);
        goto fail;
    }
    errno = 0;
    if (read(oldfd, &tmp, 1) != -1 || errno != EBADF) {
        printf("[FAIL] close error must still close fd read_errno=%d\n", errno);
        goto fail;
    }

    usleep(100 * 1000);
    if (flush_count != 1 || release_count != 1) {
        printf("[FAIL] close flush error counters flush=%u release=%u\n", flush_count,
               release_count);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_flush_enosys_cached_success() {
    const char *mp = "/tmp/test_fuse_flush_enosys";
    int f = -1;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t flush_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.flush_count = &flush_count;
    args.force_flush_errno = ENOSYS;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    for (int i = 0; i < 2; ++i) {
        f = open(path, O_RDONLY);
        if (f < 0) {
            printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
            goto fail;
        }
        if (close(f) != 0) {
            printf("[FAIL] close after FLUSH ENOSYS: %s (errno=%d)\n", strerror(errno), errno);
            f = -1;
            goto fail;
        }
        f = -1;
    }

    usleep(100 * 1000);
    if (flush_count != 1) {
        printf("[FAIL] FLUSH ENOSYS should be cached, flush_count=%u\n", flush_count);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_fopen_nonseekable_mode(uint32_t open_out_flags, const char *mp,
                                           int expect_stream) {
    int f = -1;
    char buf[8];
    ssize_t n = -1;
    volatile uint64_t last_write_offset = UINT64_MAX;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.hello_open_out_flags = open_out_flags;
    args.last_write_offset = &last_write_offset;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }

    errno = 0;
    if (lseek(f, 0, SEEK_SET) >= 0 || errno != ESPIPE) {
        printf("[FAIL] lseek expected ESPIPE, ret errno=%d (%s)\n", errno, strerror(errno));
        goto fail;
    }
    errno = 0;
    if (pread(f, buf, 1, 0) >= 0 || errno != ESPIPE) {
        printf("[FAIL] pread expected ESPIPE, errno=%d (%s)\n", errno, strerror(errno));
        goto fail;
    }
    errno = 0;
    if (pwrite(f, "x", 1, 0) >= 0 || errno != ESPIPE) {
        printf("[FAIL] pwrite expected ESPIPE, errno=%d (%s)\n", errno, strerror(errno));
        goto fail;
    }

    memset(buf, 0, sizeof(buf));
    if (read(f, buf, 5) != 5 || memcmp(buf, "hello", 5) != 0) {
        printf("[FAIL] ordinary read failed got='%.*s' errno=%d\n", 5, buf, errno);
        goto fail;
    }
    memset(buf, 0, sizeof(buf));
    n = read(f, buf, 5);
    if (expect_stream) {
        if (n != 5 || memcmp(buf, "hello", 5) != 0) {
            printf("[FAIL] stream read did not restart at offset 0 got n=%zd data='%.*s' errno=%d\n",
                   n, 5, buf, errno);
            goto fail;
        }
        if (write(f, "Z", 1) != 1) {
            printf("[FAIL] stream write failed: %s (errno=%d)\n", strerror(errno), errno);
            goto fail;
        }
        if (last_write_offset != 0) {
            printf("[FAIL] stream write offset expected 0 got %llu\n",
                   (unsigned long long)last_write_offset);
            goto fail;
        }
    } else if (n != 5 || memcmp(buf, " from", 5) != 0) {
        printf("[FAIL] nonseekable sequential read should advance offset got n=%zd data='%.*s'\n", n,
               5, buf);
        goto fail;
    }

    close(f);
    f = -1;
    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_fopen_nonseekable_dir_mode(uint32_t open_out_flags, const char *mp) {
    int f = -1;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t releasedir_count = 0;
    volatile uint32_t last_releasedir_uid = UINT32_MAX;
    volatile uint32_t last_releasedir_gid = UINT32_MAX;
    volatile uint32_t last_releasedir_pid = UINT32_MAX;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.root_open_out_flags = open_out_flags;
    args.releasedir_count = &releasedir_count;
    args.last_releasedir_uid = &last_releasedir_uid;
    args.last_releasedir_gid = &last_releasedir_gid;
    args.last_releasedir_pid = &last_releasedir_pid;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    f = open(mp, O_RDONLY | O_DIRECTORY);
    if (f < 0) {
        printf("[FAIL] open(%s, O_DIRECTORY): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }

    errno = 0;
    if (lseek(f, 0, SEEK_SET) >= 0 || errno != ESPIPE) {
        printf("[FAIL] dir lseek expected ESPIPE, errno=%d (%s)\n", errno, strerror(errno));
        goto fail;
    }

    close(f);
    f = -1;
    usleep(100 * 1000);

    if (releasedir_count != 1 || last_releasedir_uid != 0 || last_releasedir_gid != 0 ||
        last_releasedir_pid != 0) {
        printf("[FAIL] releasedir nocreds count=%u uid=%u gid=%u pid=%u\n", releasedir_count,
               last_releasedir_uid, last_releasedir_gid, last_releasedir_pid);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_atomic_otrunc_uses_open_without_setattr() {
    const char *mp = "/tmp/test_fuse_atomic_otrunc";
    int requested = O_RDWR | O_TRUNC;
    int f = -1;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t last_open_flags = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t setattr_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 0;
    args.stop_on_destroy = 1;
    args.open_count = &open_count;
    args.setattr_count = &setattr_count;
    args.last_open_in_flags = &last_open_flags;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_ATOMIC_O_TRUNC;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, requested);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    close(f);
    f = -1;

    usleep(100 * 1000);
    if (open_count != 1 || (last_open_flags & O_TRUNC) == 0) {
        printf("[FAIL] open counters/flags open=%u flags=0%o\n", open_count, last_open_flags);
        goto fail;
    }
    if (setattr_count != 0) {
        printf("[FAIL] atomic O_TRUNC unexpectedly sent SETATTR count=%u\n", setattr_count);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_ftruncate_setattr_uses_open_fh() {
    const char *mp = "/tmp/test_fuse_ftruncate_fh";
    int f = -1;
    char fallocate_verify[17] = {};
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t setattr_count = 0;
    volatile uint32_t fallocate_count = 0;
    volatile uint32_t write_count = 0;
    volatile uint64_t last_open_fh = 0;
    volatile uint32_t last_setattr_valid = 0;
    volatile uint64_t last_setattr_fh = 0;
    volatile uint64_t last_setattr_size = 0;
    volatile uint64_t last_setattr_lock_owner = 0;
    volatile uint64_t last_fallocate_fh = 0;
    volatile uint64_t last_fallocate_offset = 0;
    volatile uint64_t last_fallocate_length = 0;
    volatile uint32_t last_fallocate_mode = 0;
    volatile uint64_t last_write_offset = 0;
    volatile uint32_t last_write_size = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.open_count = &open_count;
    args.setattr_count = &setattr_count;
    args.fallocate_count = &fallocate_count;
    args.write_count = &write_count;
    args.last_open_fh = &last_open_fh;
    args.last_setattr_valid = &last_setattr_valid;
    args.last_setattr_fh = &last_setattr_fh;
    args.last_setattr_size = &last_setattr_size;
    args.last_setattr_lock_owner = &last_setattr_lock_owner;
    args.last_fallocate_fh = &last_fallocate_fh;
    args.last_fallocate_offset = &last_fallocate_offset;
    args.last_fallocate_length = &last_fallocate_length;
    args.last_fallocate_mode = &last_fallocate_mode;
    args.last_write_offset = &last_write_offset;
    args.last_write_size = &last_write_size;
    args.next_open_fh = 940;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if (ftruncate(f, 7) != 0) {
        printf("[FAIL] ftruncate: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(f);
    f = -1;

    usleep(100 * 1000);
    if (open_count != 1 || setattr_count != 1) {
        printf("[FAIL] counters open=%u setattr=%u\n", open_count, setattr_count);
        goto fail;
    }
    if ((last_setattr_valid & FATTR_SIZE) == 0 || (last_setattr_valid & FATTR_FH) == 0 ||
        (last_setattr_valid & FATTR_LOCKOWNER) == 0 || last_setattr_fh != 940 ||
        last_setattr_size != 7 || last_setattr_lock_owner == 0) {
        printf("[FAIL] setattr valid=0x%x fh=%llu size=%llu lock_owner=%llu\n",
               last_setattr_valid, (unsigned long long)last_setattr_fh,
               (unsigned long long)last_setattr_size,
               (unsigned long long)last_setattr_lock_owner);
        goto fail;
    }

    last_setattr_valid = 0;
    last_setattr_fh = 0;
    last_setattr_size = 0;
    last_setattr_lock_owner = 0;
    if (truncate(path, 5) != 0) {
        printf("[FAIL] truncate(path): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    usleep(100 * 1000);
    if (setattr_count != 2) {
        printf("[FAIL] path truncate setattr_count=%u\n", setattr_count);
        goto fail;
    }
    if ((last_setattr_valid & FATTR_SIZE) == 0 || (last_setattr_valid & FATTR_FH) != 0 ||
        (last_setattr_valid & FATTR_LOCKOWNER) == 0 || last_setattr_size != 5 ||
        last_setattr_lock_owner == 0) {
        printf("[FAIL] path setattr valid=0x%x fh=%llu size=%llu lock_owner=%llu\n",
               last_setattr_valid, (unsigned long long)last_setattr_fh,
               (unsigned long long)last_setattr_size,
               (unsigned long long)last_setattr_lock_owner);
        goto fail;
    }

    last_setattr_valid = 0;
    last_setattr_fh = 0;
    last_setattr_size = 0;
    last_setattr_lock_owner = 0;
    f = open(path, O_RDWR | O_TRUNC);
    if (f < 0) {
        printf("[FAIL] open(O_TRUNC): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(f);
    f = -1;
    usleep(100 * 1000);
    if (setattr_count != 3) {
        printf("[FAIL] open(O_TRUNC) setattr_count=%u\n", setattr_count);
        goto fail;
    }
    if ((last_setattr_valid & FATTR_SIZE) == 0 || (last_setattr_valid & FATTR_FH) != 0 ||
        (last_setattr_valid & FATTR_LOCKOWNER) == 0 || last_setattr_size != 0 ||
        last_setattr_lock_owner == 0) {
        printf("[FAIL] open truncate setattr valid=0x%x fh=%llu size=%llu lock_owner=%llu\n",
               last_setattr_valid, (unsigned long long)last_setattr_fh,
               (unsigned long long)last_setattr_size,
               (unsigned long long)last_setattr_lock_owner);
        goto fail;
    }

    setattr_count = 0;
    fallocate_count = 0;
    last_open_fh = 0;
    last_fallocate_fh = 0;
    last_fallocate_offset = 0;
    last_fallocate_length = 0;
    last_fallocate_mode = 0;
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open for fallocate: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (syscall(SYS_fallocate, f, 0, 0, 16) != 0) {
        printf("[FAIL] fallocate: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(f);
    f = -1;
    usleep(100 * 1000);
    if (setattr_count != 0 || fallocate_count != 1 || last_fallocate_fh != last_open_fh ||
        last_fallocate_offset != 0 || last_fallocate_length != 16 || last_fallocate_mode != 0) {
        printf("[FAIL] fallocate counters setattr=%u fallocate=%u fh=%llu open_fh=%llu "
               "offset=%llu length=%llu mode=%u\n",
               setattr_count, fallocate_count, (unsigned long long)last_fallocate_fh,
               (unsigned long long)last_open_fh, (unsigned long long)last_fallocate_offset,
               (unsigned long long)last_fallocate_length, last_fallocate_mode);
        goto fail;
    }
    struct stat st;
    if (stat(path, &st) != 0 || st.st_size != 16) {
        printf("[FAIL] stat after fallocate rc/size errno=%d (%s) size=%lld\n", errno,
               strerror(errno), (long long)st.st_size);
        goto fail;
    }

    static const char fallocate_pattern[] = "abcdefghijklmnop";
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] reopen for fallocate modes: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (pwrite(f, fallocate_pattern, sizeof(fallocate_pattern) - 1, 0) !=
        (ssize_t)(sizeof(fallocate_pattern) - 1)) {
        printf("[FAIL] seed fallocate cache test: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (pread(f, fallocate_verify, sizeof(fallocate_pattern) - 1, 0) !=
        (ssize_t)(sizeof(fallocate_pattern) - 1)) {
        printf("[FAIL] prime fallocate cache: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    fallocate_count = 0;
    if (syscall(SYS_fallocate, f, FALLOC_FL_KEEP_SIZE, 0, 64) != 0) {
        printf("[FAIL] fallocate KEEP_SIZE: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (stat(path, &st) != 0 || st.st_size != 16 || fallocate_count != 1 ||
        last_fallocate_mode != FALLOC_FL_KEEP_SIZE) {
        printf("[FAIL] KEEP_SIZE semantics size=%lld count=%u mode=0x%x\n",
               (long long)st.st_size, fallocate_count, last_fallocate_mode);
        goto fail;
    }

    fallocate_count = 0;
    if (syscall(SYS_fallocate, f, FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE, 4, 4) != 0) {
        printf("[FAIL] fallocate ZERO_RANGE: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    memset(fallocate_verify, 0xff, sizeof(fallocate_verify));
    if (pread(f, fallocate_verify, sizeof(fallocate_pattern) - 1, 0) !=
        (ssize_t)(sizeof(fallocate_pattern) - 1) ||
        memcmp(fallocate_verify, "abcd\0\0\0\0ijklmnop", sizeof(fallocate_pattern) - 1) != 0 ||
        fallocate_count != 1 ||
        last_fallocate_mode != (FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE)) {
        printf("[FAIL] ZERO_RANGE cache/forwarding count=%u mode=0x%x\n", fallocate_count,
               last_fallocate_mode);
        goto fail;
    }

    fallocate_count = 0;
    if (syscall(SYS_fallocate, f, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 8, 4) != 0) {
        printf("[FAIL] fallocate PUNCH_HOLE: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    memset(fallocate_verify, 0xff, sizeof(fallocate_verify));
    if (pread(f, fallocate_verify, sizeof(fallocate_pattern) - 1, 0) !=
        (ssize_t)(sizeof(fallocate_pattern) - 1) ||
        memcmp(fallocate_verify, "abcd\0\0\0\0\0\0\0\0mnop", sizeof(fallocate_pattern) - 1) !=
            0 ||
        fallocate_count != 1 ||
        last_fallocate_mode != (FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE)) {
        printf("[FAIL] PUNCH_HOLE cache/forwarding count=%u mode=0x%x\n", fallocate_count,
               last_fallocate_mode);
        goto fail;
    }
    close(f);
    f = -1;

    setattr_count = 0;
    fallocate_count = 0;
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open for fallocate overflow: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (syscall(SYS_fallocate, f, 0, INT64_MAX - 1, 4) == 0 || errno != EFBIG) {
        printf("[FAIL] fallocate overflow expected EFBIG, errno=%d (%s)\n", errno,
               strerror(errno));
        goto fail;
    }
    close(f);
    f = -1;
    usleep(100 * 1000);
    if (setattr_count != 0 || fallocate_count != 0) {
        printf("[FAIL] fallocate overflow sent requests setattr=%u fallocate=%u\n", setattr_count,
               fallocate_count);
        goto fail;
    }

    setattr_count = 0;
    last_setattr_valid = 0;
    last_setattr_fh = 0;
    last_setattr_size = 0;
    last_setattr_lock_owner = 0;
    write_count = 0;
    last_write_offset = 0;
    last_write_size = 0;
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open for pwrite: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (pwrite(f, "xy", 2, 9) != 2) {
        printf("[FAIL] pwrite hole: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(f);
    f = -1;
    usleep(100 * 1000);
    if (setattr_count != 0 || write_count != 1 || last_write_offset != 9 || last_write_size != 2) {
        printf("[FAIL] pwrite hole counters setattr=%u write=%u offset=%llu size=%u\n",
               setattr_count, write_count, (unsigned long long)last_write_offset,
               last_write_size);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_init_requests_linux_no_open_support() {
    const char *mp = "/tmp/test_fuse_init_flags";
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t init_flags = 0;
    volatile uint32_t init_flags2 = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.init_in_flags = &init_flags;
    args.init_in_flags2 = &init_flags2;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    if ((init_flags & FUSE_NO_OPEN_SUPPORT) == 0 ||
        (init_flags & FUSE_NO_OPENDIR_SUPPORT) == 0 ||
        (init_flags & FUSE_WRITEBACK_CACHE) == 0 ||
        (init_flags2 & (1u << (35 - 32))) == 0) {
        printf("[FAIL] INIT flags missing no-open/writeback/expire-only support bits: "
               "flags=0x%x flags2=0x%x\n",
               init_flags, init_flags2);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_p4_subtype_mount() {
    const char *mp = "/tmp/test_fuse_p4_subtype";
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 0;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse.fuse3_demo", 0, opts) != 0) {
        printf("[FAIL] mount(fuse.fuse3_demo): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    for (int i = 0; i < 200; i++) {
        if (init_done) {
            break;
        }
        usleep(10 * 1000);
    }
    if (!init_done) {
        printf("[FAIL] init handshake timeout\n");
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    char file_path[256];
    snprintf(file_path, sizeof(file_path), "%s/hello.txt", mp);

    char buf[128];
    if (fuseg_read_file_cstr(file_path, buf, sizeof(buf)) < 0) {
        printf("[FAIL] read(%s): %s (errno=%d)\n", file_path, strerror(errno), errno);
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (strcmp(buf, "hello from fuse\n") != 0) {
        printf("[FAIL] content mismatch: got='%s'\n", buf);
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;
}

static int ext_run_child_drop_priv_and_stat(const char *mp, int expect_errno, int expect_success) {
    pid_t pid = fork();
    if (pid < 0) {
        return -1;
    }
    if (pid == 0) {
        if (setgid(1000) != 0) {
            _exit(30);
        }
        if (setuid(1000) != 0) {
            _exit(31);
        }

        struct stat st;
        int r = stat(mp, &st);
        if (expect_success) {
            if (r != 0)
                _exit(10);
            char p[256];
            snprintf(p, sizeof(p), "%s/hello.txt", mp);
            int fd = open(p, O_RDONLY);
            if (fd < 0)
                _exit(11);
            char buf[64];
            ssize_t n = read(fd, buf, sizeof(buf) - 1);
            close(fd);
            if (n < 0)
                _exit(12);
            buf[n] = '\0';
            if (strcmp(buf, "hello from fuse\n") != 0)
                _exit(13);
            _exit(0);
        }

        if (r != 0 && errno == expect_errno) {
            _exit(0);
        }
        if (r != 0) {
            _exit(21);
        }

        /*
         * Linux 语义下，目录本身的 stat 可能成功；真正的拒绝点通常体现在
         * 访问目录内对象（例如 open/stat 子路径）。
         */
        char p[256];
        snprintf(p, sizeof(p), "%s/hello.txt", mp);
        int fd = open(p, O_RDONLY);
        if (fd >= 0) {
            close(fd);
            _exit(22);
        }
        if (errno != expect_errno) {
            _exit(23);
        }
        _exit(0);
    }

    int status = 0;
    if (waitpid(pid, &status, 0) < 0) {
        return -1;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        errno = ECHILD;
        return -1;
    }
    return 0;
}

static int ext_run_permission_case(const char *mp, const char *opts, uint32_t root_mode_override,
                                   uint32_t hello_mode_override, int expect_errno,
                                   int expect_success) {
    if (ensure_dir(mp) != 0) {
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 0;
    args.exit_after_init = 0;
    args.stop_on_destroy = 1;
    args.root_mode_override = root_mode_override;
    args.hello_mode_override = hello_mode_override;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        close(fd);
        rmdir(mp);
        return -1;
    }

    char full_opts[512];
    snprintf(full_opts, sizeof(full_opts), "fd=%d,%s", fd, opts);
    if (mount("none", mp, "fuse", 0, full_opts) != 0) {
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    if (fuseg_wait_init(&init_done) != 0) {
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    if (ext_run_child_drop_priv_and_stat(mp, expect_errno, expect_success) != 0) {
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    umount(mp);
    rmdir(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    return 0;
}

static int ext_test_permissions() {
    const uint32_t DIR_NO_PERM = 0040000;
    const uint32_t REG_NO_PERM = 0100000;

    {
        const char *mp = "/tmp/test_fuse_perm_owner";
        if (ext_run_permission_case(mp, "rootmode=040755,user_id=0,group_id=0", 0, 0, EACCES, 0) !=
            0) {
            printf("[FAIL] mount owner restriction\n");
            return -1;
        }
    }

    {
        const char *mp = "/tmp/test_fuse_perm_default";
        if (ext_run_permission_case(
                mp, "rootmode=040000,user_id=0,group_id=0,allow_other,default_permissions",
                DIR_NO_PERM, REG_NO_PERM, EACCES, 0) != 0) {
            printf("[FAIL] default_permissions deny\n");
            return -1;
        }
    }

    {
        const char *mp = "/tmp/test_fuse_perm_remote";
        if (ext_run_permission_case(mp, "rootmode=040000,user_id=0,group_id=0,allow_other",
                                    DIR_NO_PERM, REG_NO_PERM, 0, 1) != 0) {
            printf("[FAIL] remote permission model allow\n");
            return -1;
        }
    }

    return 0;
}

static int ext_test_clone() {
    const char *mp = "/tmp/test_fuse_clone";
    DIR *d = NULL;
    int found = 0;
    struct dirent *de = NULL;
    char p[256];
    struct stat st;
    char buf[128];
    int n = -1;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int master_fd = open("/dev/fuse", O_RDWR);
    if (master_fd < 0) {
        printf("[FAIL] open(/dev/fuse master): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;

    struct fuse_daemon_args master_args;
    memset(&master_args, 0, sizeof(master_args));
    master_args.fd = master_fd;
    master_args.stop = &stop;
    master_args.init_done = &init_done;
    master_args.enable_write_ops = 0;
    master_args.exit_after_init = 1;

    pthread_t master_th;
    if (pthread_create(&master_th, NULL, fuse_daemon_thread, &master_args) != 0) {
        printf("[FAIL] pthread_create(master)\n");
        close(master_fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", master_fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(master_fd);
        pthread_join(master_th, NULL);
        rmdir(mp);
        return -1;
    }

    for (int i = 0; i < 100; i++) {
        if (init_done)
            break;
        usleep(10 * 1000);
    }
    if (!init_done) {
        printf("[FAIL] init handshake timeout\n");
        umount(mp);
        stop = 1;
        close(master_fd);
        pthread_join(master_th, NULL);
        rmdir(mp);
        return -1;
    }

    pthread_join(master_th, NULL);

    int clone_fd = open("/dev/fuse", O_RDWR);
    if (clone_fd < 0) {
        printf("[FAIL] open(/dev/fuse clone): %s (errno=%d)\n", strerror(errno), errno);
        umount(mp);
        close(master_fd);
        rmdir(mp);
        return -1;
    }

    uint32_t oldfd_u32 = (uint32_t)master_fd;
    if (ioctl(clone_fd, FUSE_DEV_IOC_CLONE, &oldfd_u32) != 0) {
        printf("[FAIL] ioctl(FUSE_DEV_IOC_CLONE): %s (errno=%d)\n", strerror(errno), errno);
        umount(mp);
        close(clone_fd);
        close(master_fd);
        rmdir(mp);
        return -1;
    }

    struct fuse_daemon_args clone_args;
    memset(&clone_args, 0, sizeof(clone_args));
    clone_args.fd = clone_fd;
    clone_args.stop = &stop;
    clone_args.init_done = &init_done;
    clone_args.enable_write_ops = 0;
    clone_args.exit_after_init = 0;
    clone_args.stop_on_destroy = 1;

    pthread_t clone_th;
    if (pthread_create(&clone_th, NULL, fuse_daemon_thread, &clone_args) != 0) {
        printf("[FAIL] pthread_create(clone)\n");
        umount(mp);
        close(clone_fd);
        close(master_fd);
        rmdir(mp);
        return -1;
    }

    d = opendir(mp);
    if (!d) {
        printf("[FAIL] opendir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }
    found = 0;
    while ((de = readdir(d)) != NULL) {
        if (strcmp(de->d_name, "hello.txt") == 0) {
            found = 1;
            break;
        }
    }
    closedir(d);
    if (!found) {
        printf("[FAIL] readdir: hello.txt not found\n");
        goto fail;
    }

    snprintf(p, sizeof(p), "%s/hello.txt", mp);
    if (stat(p, &st) != 0) {
        printf("[FAIL] stat(%s): %s (errno=%d)\n", p, strerror(errno), errno);
        goto fail;
    }
    if (!S_ISREG(st.st_mode)) {
        printf("[FAIL] stat: expected regular file\n");
        goto fail;
    }

    n = fuseg_read_file_cstr(p, buf, sizeof(buf));
    if (n < 0) {
        printf("[FAIL] read(%s): %s (errno=%d)\n", p, strerror(errno), errno);
        goto fail;
    }
    if (strcmp(buf, "hello from fuse\n") != 0) {
        printf("[FAIL] content mismatch: got='%s'\n", buf);
        goto fail;
    }

    umount(mp);
    rmdir(mp);
    stop = 1;
    close(clone_fd);
    close(master_fd);
    pthread_join(clone_th, NULL);
    return 0;

fail:
    umount(mp);
    stop = 1;
    close(clone_fd);
    close(master_fd);
    pthread_join(clone_th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_large_read_over_max_write() {
    const char *mp = "/tmp/test_fuse_large_read";
    const size_t data_size = 6000;
    char path[256];
    char *buf = NULL;
    int n = -1;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t read_count = 0;
    volatile uint64_t read_offsets[4] = {0};
    volatile uint32_t read_sizes[4] = {0};

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.read_count = &read_count;
    args.read_offsets = read_offsets;
    args.read_sizes = read_sizes;
    args.read_trace_capacity = 4;
    args.hello_data_size_override = data_size;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    buf = (char *)malloc(data_size);
    if (!buf) {
        printf("[FAIL] malloc read buffer\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    n = fuseg_read_file(path, buf, data_size);
    if (n < 0) {
        printf("[FAIL] read(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if ((size_t)n != data_size) {
        printf("[FAIL] read size mismatch: got=%d expected=%zu read_count=%u\n", n, data_size,
               read_count);
        goto fail;
    }
    for (size_t i = 0; i < data_size; i++) {
        char expected = (char)('A' + (i % 26));
        if (buf[i] != expected) {
            printf("[FAIL] read data mismatch at %zu: got=%d expected=%d\n", i, buf[i],
                   expected);
            goto fail;
        }
    }
    if (read_count != 2 || read_offsets[0] != 0 || read_offsets[1] != 4096 ||
        read_sizes[0] != 4096 || read_sizes[1] > 4096 || read_sizes[1] == 0) {
        printf("[FAIL] unexpected FUSE_READ split: count=%u off0=%llu size0=%u off1=%llu size1=%u\n",
               read_count, (unsigned long long)read_offsets[0], read_sizes[0],
               (unsigned long long)read_offsets[1], read_sizes[1]);
        goto fail;
    }

    free(buf);
    buf = NULL;
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (buf) {
        free(buf);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_cached_read_pipelines_requests() {
    const char *mp = "/tmp/test_fuse_read_pipeline";
    const size_t data_size = 64 * 1024;
    char *buf = NULL;
    int n = -1;
    int ok = 0;
    if (ensure_dir(mp) != 0)
        return -1;
    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        rmdir(mp);
        return -1;
    }
    volatile int stop = 0, init_done = 0, saw_pipeline = 0;
    volatile uint32_t read_count = 0;
    volatile uint32_t init_in_flags = 0;
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.init_in_flags = &init_in_flags;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_ASYNC_READ;
    args.hello_generated_size_override = data_size;
    args.read_count = &read_count;
    args.defer_first_read_reply = 2;
    args.saw_pipelined_read = &saw_pipeline;
    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0)
        goto fail_no_thread;
    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0 || fuseg_wait_init(&init_done) != 0)
        goto fail;
    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    buf = (char *)malloc(data_size);
    if (!buf)
        goto fail;
    n = fuseg_read_file(path, buf, 4096);
    ok = n == 4096 && (init_in_flags & FUSE_ASYNC_READ) != 0 && saw_pipeline && read_count >= 2;
    for (size_t i = 0; ok && i < 4096; ++i)
        ok = buf[i] == (char)('A' + (i % 26));
    free(buf);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return ok ? 0 : -1;
fail:
    free(buf);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
fail_no_thread:
    close(fd);
    rmdir(mp);
    return -1;
}

struct fuseg_async_read_args {
    const char *path;
    char *buf;
    size_t size;
    int result;
};

static void *fuseg_async_read_thread(void *opaque) {
    struct fuseg_async_read_args *args = (struct fuseg_async_read_args *)opaque;
    args->result = fuseg_read_file(args->path, args->buf, args->size);
    return NULL;
}

static int ext_test_cached_read_without_async_is_serial() {
    const char *mp = "/tmp/test_fuse_read_serial";
    const size_t data_size = 64 * 1024;
    char *buf = NULL;
    int n = -1;
    int ok = 0;
    int wait_rc = 0;
    int mounted = 0, client_started = 0;
    char opts[256], path[256];
    struct fuseg_async_read_args read_args;
    pthread_t th, client_th;
    struct timespec gate_deadline;
    if (ensure_dir(mp) != 0)
        return -1;
    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        rmdir(mp);
        return -1;
    }
    volatile int stop = 0, init_done = 0;
    volatile uint32_t read_count = 0, init_in_flags = 0;
    volatile uint64_t read_offsets[4] = {0};
    pthread_mutex_t first_read_gate_mutex = PTHREAD_MUTEX_INITIALIZER;
    pthread_cond_t first_read_gate_cond = PTHREAD_COND_INITIALIZER;
    int first_read_captured = 0;
    int first_read_gate_state = -1;
    int daemon_waiting_after_first_read = 0;
    int saw_early_read = 0;
    int first_read_reply_result = -9999;
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.init_in_flags = &init_in_flags;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES;
    args.hello_generated_size_override = data_size;
    args.read_count = &read_count;
    args.read_offsets = read_offsets;
    args.read_trace_capacity = 4;
    args.first_read_gate_mutex = &first_read_gate_mutex;
    args.first_read_gate_cond = &first_read_gate_cond;
    args.first_read_captured = &first_read_captured;
    args.first_read_gate_state = &first_read_gate_state;
    args.daemon_waiting_after_first_read = &daemon_waiting_after_first_read;
    args.saw_read_before_first_reply = &saw_early_read;
    args.first_read_reply_result = &first_read_reply_result;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0)
        goto fail_no_thread;
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0)
        goto fail;
    mounted = 1;
    if (fuseg_wait_init(&init_done) != 0)
        goto fail;
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    buf = (char *)malloc(data_size);
    if (!buf)
        goto fail;
    memset(&read_args, 0, sizeof(read_args));
    read_args.path = path;
    read_args.buf = buf;
    read_args.size = 8192;
    if (pthread_create(&client_th, NULL, fuseg_async_read_thread, &read_args) != 0)
        goto fail;
    client_started = 1;
    clock_gettime(CLOCK_REALTIME, &gate_deadline);
    gate_deadline.tv_sec += 5;
    pthread_mutex_lock(&first_read_gate_mutex);
    wait_rc = 0;
    while (!first_read_captured && wait_rc == 0)
        wait_rc = pthread_cond_timedwait(&first_read_gate_cond, &first_read_gate_mutex,
                                         &gate_deadline);
    if (!first_read_captured) {
        pthread_mutex_unlock(&first_read_gate_mutex);
        goto fail;
    }
    wait_rc = 0;
    while (!daemon_waiting_after_first_read && wait_rc == 0)
        wait_rc = pthread_cond_timedwait(&first_read_gate_cond, &first_read_gate_mutex,
                                         &gate_deadline);
    if (!daemon_waiting_after_first_read) {
        pthread_mutex_unlock(&first_read_gate_mutex);
        goto fail;
    }
    wait_rc = 0;
    while (__atomic_load_n(&first_read_gate_state, __ATOMIC_ACQUIRE) == 0 && wait_rc == 0)
        wait_rc = pthread_cond_timedwait(&first_read_gate_cond, &first_read_gate_mutex,
                                         &gate_deadline);
    ok = __atomic_load_n(&first_read_gate_state, __ATOMIC_ACQUIRE) == 3
         && first_read_reply_result == 0 && !saw_early_read;
    pthread_mutex_unlock(&first_read_gate_mutex);
    if (!ok)
        goto fail;
    pthread_join(client_th, NULL);
    client_started = 0;
    n = read_args.result;
    ok = n == 8192 && (init_in_flags & FUSE_ASYNC_READ) != 0 && !saw_early_read
         && read_count >= 2 && read_offsets[0] == 0 && read_offsets[1] == 4096
         && first_read_reply_result == 0
         && __atomic_load_n(&first_read_gate_state, __ATOMIC_ACQUIRE) == 3 && ok;
    for (size_t i = 0; ok && i < 8192; ++i)
        ok = buf[i] == (char)('A' + (i % 26));
    free(buf);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return ok ? 0 : -1;
fail:
    stop = 1;
    close(fd);
    if (client_started)
        pthread_join(client_th, NULL);
    free(buf);
    if (mounted)
        umount(mp);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
fail_no_thread:
    close(fd);
    rmdir(mp);
    return -1;
}

static int ext_run_cached_read_sync_error_case(const char *mp, uint64_t error_offset,
                                               int error_once, int case_kind) {
    const size_t data_size = 64 * 1024;
    int result = -1;
    int file_fd = -1;
    ssize_t first = -1, second = -1;
    int first_errno = 0, second_errno = 0;
    char buf[8192];
    if (ensure_dir(mp) != 0)
        return -1;
    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        rmdir(mp);
        return -1;
    }
    volatile int stop = 0, init_done = 0;
    volatile uint32_t init_in_flags = 0, read_count = 0;
    volatile uint64_t read_offsets[8] = {0};
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.init_in_flags = &init_in_flags;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES;
    args.hello_generated_size_override = data_size;
    args.read_count = &read_count;
    args.read_offsets = read_offsets;
    args.read_trace_capacity = 8;
    args.has_forced_read_error = 1;
    args.forced_read_errno = EIO;
    args.forced_read_error_once = error_once;
    args.forced_read_error_offset = error_offset;
    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0)
        goto fail_no_thread;
    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0 || fuseg_wait_init(&init_done) != 0)
        goto fail;
    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    file_fd = open(path, O_RDONLY);
    if (file_fd < 0 || (init_in_flags & FUSE_ASYNC_READ) == 0)
        goto fail;

    errno = 0;
    first = pread(file_fd, buf, case_kind == 2 ? 4096 : 8192, 0);
    first_errno = errno;
    if (case_kind == 2) {
        result = first == -1 && first_errno == EIO && read_count == 1 && read_offsets[0] == 0 ? 0
                                                                                           : -1;
        goto out;
    }
    if (first != 4096 || read_count != 2 || read_offsets[0] != 0 || read_offsets[1] != 4096)
        goto out;
    for (size_t i = 0; i < 4096; ++i) {
        if (buf[i] != (char)('A' + (i % 26)))
            goto out;
    }
    errno = 0;
    second = pread(file_fd, buf + 4096, 4096, 4096);
    second_errno = errno;
    if (!error_once) {
        result = second == -1 && second_errno == EIO && read_count == 3
                         && read_offsets[2] == 4096
                     ? 0
                     : -1;
    } else {
        result = second == 4096 && read_count == 3 && read_offsets[2] == 4096 ? 0 : -1;
        for (size_t i = 0; result == 0 && i < 4096; ++i) {
            if (buf[4096 + i] != (char)('A' + ((4096 + i) % 26)))
                result = -1;
        }
    }
out:
    if (file_fd >= 0)
        close(file_fd);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return result;
fail:
    if (file_fd >= 0)
        close(file_fd);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
fail_no_thread:
    close(fd);
    rmdir(mp);
    return -1;
}

static int ext_test_cached_read_sync_error_semantics() {
    if (ext_run_cached_read_sync_error_case("/tmp/test_fuse_read_eio_persistent", 4096, 0, 0)
        != 0)
        return -1;
    if (ext_run_cached_read_sync_error_case("/tmp/test_fuse_read_eio_once", 4096, 1, 1) != 0)
        return -1;
    return ext_run_cached_read_sync_error_case("/tmp/test_fuse_read_eio_first", 0, 0, 2);
}

static int ext_test_cached_read_uses_open_fh_without_extra_open() {
    const char *mp = "/tmp/test_fuse_cached_read_fh";
    char path[256];
    char buf[32];
    int f = -1;
    ssize_t n = -1;
    ssize_t first_n = -1;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t read_count = 0;
    volatile uint64_t read_fhs[4] = {0};

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.open_count = &open_count;
    args.read_count = &read_count;
    args.read_fhs = read_fhs;
    args.read_trace_capacity = 4;
    args.next_open_fh = 100;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    n = pread(f, buf, sizeof(buf), 0);
    if (n <= 0) {
        printf("[FAIL] first pread got=%zd errno=%d\n", n, errno);
        close(f);
        goto fail;
    }
    first_n = n;
    memset(buf, 0, sizeof(buf));
    n = pread(f, buf, sizeof(buf), 0);
    close(f);
    f = -1;
    if (n != first_n) {
        printf("[FAIL] second pread got=%zd errno=%d\n", n, errno);
        goto fail;
    }
    if (open_count != 1 || read_count != 1 || read_fhs[0] != 100) {
        printf("[FAIL] cached read counters open=%u read=%u fh0=%llu\n", open_count,
               read_count, (unsigned long long)read_fhs[0]);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_cached_short_read_updates_eof() {
    const char *mp = "/tmp/test_fuse_cached_short_read";
    char path[256];
    char buf[32];
    int f = -1;
    ssize_t n = -1;
    uint32_t reads_after_short = 0;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t read_count = 0;
    volatile uint64_t read_offsets[4] = {0};
    volatile uint32_t read_sizes[4] = {0};

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.read_count = &read_count;
    args.read_offsets = read_offsets;
    args.read_sizes = read_sizes;
    args.read_trace_capacity = 4;
    args.hello_data_size_override = 8192;
    args.hello_read_size_override = 5;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }

    memset(buf, 0x7f, sizeof(buf));
    n = pread(f, buf, sizeof(buf), 0);
    if (n != 5 || memcmp(buf, "ABCDE", 5) != 0) {
        printf("[FAIL] short cached pread got=%zd data='%.*s' read=%u errno=%d\n", n, 5, buf,
               read_count, errno);
        goto fail;
    }

    // The second READ is speculative readahead, so the foreground pread must
    // not wait for the daemon to consume it. Wait here before inspecting the
    // asynchronous trace rather than depending on daemon scheduling.
    for (int i = 0;
         i < 200 &&
         (read_count < 2 || read_offsets[1] != 4096 || read_sizes[1] != 4096);
         ++i) {
        usleep(5 * 1000);
    }
    if (read_count < 2 || read_offsets[0] != 0 || read_sizes[0] != 4096 ||
        read_offsets[1] != 4096 || read_sizes[1] != 4096) {
        printf("[FAIL] short read trace count=%u off0=%llu size0=%u off1=%llu size1=%u\n",
               read_count, (unsigned long long)read_offsets[0], read_sizes[0],
               (unsigned long long)read_offsets[1], read_sizes[1]);
        goto fail;
    }
    reads_after_short = read_count;

    memset(buf, 0x7f, sizeof(buf));
    n = pread(f, buf, sizeof(buf), 5);
    if (n != 0) {
        printf("[FAIL] EOF cached pread got=%zd read=%u errno=%d\n", n, read_count, errno);
        goto fail;
    }
    if (read_count != reads_after_short) {
        printf("[FAIL] cached EOF issued a new READ before=%u after=%u\n", reads_after_short,
               read_count);
        goto fail;
    }

    close(f);
    f = -1;
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_short_read_discards_old_pages_after_regrow() {
    const char *mp = "/tmp/test_fuse_short_read_regrow";
    char path[256];
    int f = -1;
    unsigned char byte = 0;
    if (ensure_dir(mp) != 0)
        return -1;
    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0, init_done = 0;
    volatile size_t visible_size = 8192;
    volatile size_t watched_offset = 4096;
    volatile unsigned char backend_byte = 'X';
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.hello_data_size_override = 8192;
    args.dynamic_hello_read_size = &visible_size;
    args.dynamic_hello_byte_offset = &watched_offset;
    args.dynamic_hello_byte = &backend_byte;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0)
        goto fail_no_thread;
    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0 || fuseg_wait_init(&init_done) != 0)
        goto fail;
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0)
        goto fail;

    if (pread(f, &byte, 1, 4096) != 1 || byte != 'X')
        goto fail;

    backend_byte = 'Y';
    visible_size = 5;
    if (pread(f, &byte, 1, 0) != 1 || byte != 'A')
        goto fail;

    visible_size = 8192;
    if (ftruncate(f, 8192) != 0)
        goto fail;
    byte = 0;
    if (pread(f, &byte, 1, 4096) != 1 || byte != 'Y') {
        printf("[FAIL] regrown read returned stale byte=%u expected=%u errno=%d\n", byte, 'Y',
               errno);
        goto fail;
    }

    close(f);
    f = -1;
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;
fail:
    if (f >= 0)
        close(f);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
fail_no_thread:
    close(fd);
    rmdir(mp);
    return -1;
}

static int ext_test_cached_read_sees_write_through_update() {
    const char *mp = "/tmp/test_fuse_cached_read_write";
    char path[256];
    char buf[16];
    int f = -1;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t read_count = 0;
    volatile uint32_t write_count = 0;
    volatile uint64_t last_write_fh = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.open_count = &open_count;
    args.read_count = &read_count;
    args.write_count = &write_count;
    args.last_write_fh = &last_write_fh;
    args.next_open_fh = 300;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    memset(buf, 0, sizeof(buf));
    if (pread(f, buf, 5, 0) != 5 || memcmp(buf, "hello", 5) != 0) {
        printf("[FAIL] first cached pread got='%.*s' read=%u errno=%d\n", 5, buf, read_count,
               errno);
        goto fail;
    }
    if (pwrite(f, "CACHE", 5, 0) != 5) {
        printf("[FAIL] pwrite CACHE: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    memset(buf, 0, sizeof(buf));
    if (pread(f, buf, 5, 0) != 5 || memcmp(buf, "CACHE", 5) != 0) {
        printf("[FAIL] second cached pread got='%.*s' read=%u errno=%d\n", 5, buf, read_count,
               errno);
        goto fail;
    }
    if (open_count != 1 || read_count != 1 || write_count != 1 || last_write_fh != 300) {
        printf("[FAIL] cached write counters open=%u read=%u write=%u wfh=%llu\n", open_count,
               read_count, write_count, (unsigned long long)last_write_fh);
        goto fail;
    }

    close(f);
    f = -1;
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_mmap_sees_write_through_update() {
    const char *mp = "/tmp/test_fuse_mmap_write_through";
    char path[256];
    char buf[16];
    int f = -1;
    void *addr = MAP_FAILED;
    pid_t child = -1;
    struct mmap_write_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
        volatile uint32_t write_count;
        volatile uint64_t last_write_fh;
        volatile uint64_t read_fhs[4];
    };
    struct mmap_write_shared_state *shared =
        (struct mmap_write_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                               MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    child = fork();
    if (child < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (child == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.stop_on_destroy = 1;
        child_args.enable_write_ops = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.write_count = &shared->write_count;
        child_args.last_write_fh = &shared->last_write_fh;
        child_args.read_fhs = shared->read_fhs;
        child_args.read_trace_capacity = 4;
        child_args.next_open_fh = 320;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(child, SIGTERM);
        waitpid(child, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if (((volatile char *)addr)[0] != 'h') {
        printf("[FAIL] mmap warmup first byte got=%d\n", ((volatile char *)addr)[0]);
        goto fail;
    }
    if (pwrite(f, "MMAP!", 5, 0) != 5) {
        printf("[FAIL] pwrite MMAP!: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (memcmp(addr, "MMAP!", 5) != 0) {
        printf("[FAIL] mmap page did not observe write-through update, got='%.*s'\n", 5,
               (char *)addr);
        goto fail;
    }
    memset(buf, 0, sizeof(buf));
    if (pread(f, buf, 5, 0) != 5 || memcmp(buf, "MMAP!", 5) != 0) {
        printf("[FAIL] cached pread after mmap write got='%.*s' read=%u errno=%d\n", 5, buf,
               shared->read_count, errno);
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 1 || shared->write_count != 1 ||
        shared->last_write_fh != 320 || shared->read_fhs[0] != 320) {
        printf("[FAIL] mmap write-through counters open=%u read=%u write=%u rfh=%llu wfh=%llu\n",
               shared->open_count, shared->read_count, shared->write_count,
               (unsigned long long)shared->read_fhs[0],
               (unsigned long long)shared->last_write_fh);
        goto fail;
    }

    munmap(addr, 4096);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(child, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (child > 0) {
        kill(child, SIGTERM);
        waitpid(child, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_mmap_fault_uses_open_fh_without_extra_open() {
    const char *mp = "/tmp/test_fuse_mmap_fh";
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t child = -1;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
        volatile uint64_t read_fhs[4];
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    child = fork();
    if (child < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (child == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.read_fhs = shared->read_fhs;
        child_args.read_trace_capacity = 4;
        child_args.next_open_fh = 200;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(child, SIGTERM);
        waitpid(child, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }
    c = ((volatile char *)addr)[0];
    if (c != 'h') {
        printf("[FAIL] mmap first byte got=%d\n", c);
        munmap(addr, 4096);
        close(f);
        goto fail;
    }
    munmap(addr, 4096);
    addr = MAP_FAILED;
    close(f);
    f = -1;

    if (shared->open_count != 1 || shared->read_count != 1 || shared->read_fhs[0] != 200) {
        printf("[FAIL] mmap counters open=%u read=%u fh0=%llu\n", shared->open_count,
               shared->read_count, (unsigned long long)shared->read_fhs[0]);
        goto fail;
    }

    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(child, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (child > 0) {
        kill(child, SIGTERM);
        waitpid(child, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_mmap_fault_batches_readaround_pages() {
    const char *mp = "/tmp/test_fuse_mmap_readaround";
    const size_t page_size = 4096;
    const size_t page_count = 8;
    const size_t map_len = page_size * page_count;
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile unsigned int checksum = 0;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t read_count = 0;
    volatile uint64_t read_offsets[8] = {0};
    volatile uint32_t read_sizes[8] = {0};

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.read_count = &read_count;
    args.read_offsets = read_offsets;
    args.read_sizes = read_sizes;
    args.read_trace_capacity = 8;
    args.hello_generated_size_override = map_len;
    args.init_out_max_write_override = map_len;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=32768",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, map_len, PROT_READ, MAP_PRIVATE, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }

    for (size_t i = 0; i < page_count; i++) {
        size_t offset = i * page_size;
        unsigned char c = ((volatile unsigned char *)addr)[offset];
        unsigned char expected = (unsigned char)('A' + (offset % 26));
        if (c != expected) {
            printf("[FAIL] mmap data mismatch page=%zu got=%u expected=%u read_count=%u\n", i, c,
                   expected, read_count);
            goto fail;
        }
        checksum += c;
    }
    if (checksum == 0) {
        printf("[FAIL] checksum unexpectedly zero\n");
        goto fail;
    }

    if (read_count != 1 || read_offsets[0] != 0 || read_sizes[0] != map_len) {
        printf("[FAIL] mmap readaround not batched: count=%u off0=%llu size0=%u off1=%llu size1=%u\n",
               read_count, (unsigned long long)read_offsets[0], read_sizes[0],
               (unsigned long long)read_offsets[1], read_sizes[1]);
        goto fail;
    }

    munmap(addr, map_len);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, map_len);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_direct_io_read_bypasses_page_cache() {
    const char *mp = "/tmp/test_fuse_direct_read";
    char path[256];
    char buf[32];
    int f = -1;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t read_count = 0;
    volatile uint64_t read_fhs[4] = {0};

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.open_count = &open_count;
    args.read_count = &read_count;
    args.read_fhs = read_fhs;
    args.read_trace_capacity = 4;
    args.next_open_fh = 700;
    args.hello_open_out_flags = FOPEN_DIRECT_IO;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    memset(buf, 0, sizeof(buf));
    if (pread(f, buf, 5, 0) != 5 || memcmp(buf, "hello", 5) != 0) {
        printf("[FAIL] first direct pread got='%.*s' read=%u errno=%d\n", 5, buf, read_count,
               errno);
        goto fail;
    }
    memset(buf, 0, sizeof(buf));
    if (pread(f, buf, 5, 0) != 5 || memcmp(buf, "hello", 5) != 0) {
        printf("[FAIL] second direct pread got='%.*s' read=%u errno=%d\n", 5, buf, read_count,
               errno);
        goto fail;
    }
    close(f);
    f = -1;

    if (open_count != 1 || read_count != 2 || read_fhs[0] != 700 || read_fhs[1] != 700) {
        printf("[FAIL] direct read counters open=%u read=%u fh0=%llu fh1=%llu\n", open_count,
               read_count, (unsigned long long)read_fhs[0], (unsigned long long)read_fhs[1]);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_direct_io_write_invalidates_cached_read() {
    const char *mp = "/tmp/test_fuse_direct_write_inval";
    char path[256];
    char buf[16];
    int cached_fd = -1;
    int direct_fd = -1;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t read_count = 0;
    volatile uint32_t write_count = 0;
    volatile uint32_t open_out_flags = 0;
    volatile uint64_t last_write_fh = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.open_count = &open_count;
    args.read_count = &read_count;
    args.write_count = &write_count;
    args.dynamic_hello_open_out_flags = &open_out_flags;
    args.last_write_fh = &last_write_fh;
    args.next_open_fh = 520;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    cached_fd = open(path, O_RDWR);
    if (cached_fd < 0) {
        printf("[FAIL] open cached fd: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    memset(buf, 0, sizeof(buf));
    if (pread(cached_fd, buf, 5, 0) != 5 || memcmp(buf, "hello", 5) != 0) {
        printf("[FAIL] initial cached pread got='%.*s' read=%u errno=%d\n", 5, buf, read_count,
               errno);
        goto fail;
    }

    open_out_flags = FOPEN_DIRECT_IO;
    direct_fd = open(path, O_WRONLY);
    if (direct_fd < 0) {
        printf("[FAIL] open direct fd: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (pwrite(direct_fd, "DIO!!", 5, 0) != 5) {
        printf("[FAIL] direct pwrite: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (pwrite(direct_fd, "TAIL!", 5, 20) != 5) {
        printf("[FAIL] direct pwrite extend: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(direct_fd);
    direct_fd = -1;
    open_out_flags = 0;

    memset(buf, 0, sizeof(buf));
    if (pread(cached_fd, buf, 5, 0) != 5 || memcmp(buf, "DIO!!", 5) != 0) {
        printf("[FAIL] cached pread after direct write got='%.*s' read=%u errno=%d\n", 5, buf,
               read_count, errno);
        goto fail;
    }
    memset(buf, 0, sizeof(buf));
    if (pread(cached_fd, buf, 5, 20) != 5 || memcmp(buf, "TAIL!", 5) != 0) {
        printf("[FAIL] cached pread after direct extend got='%.*s' read=%u errno=%d\n", 5, buf,
               read_count, errno);
        goto fail;
    }
    if (open_count != 2 || read_count != 2 || write_count != 2 || last_write_fh != 521) {
        printf("[FAIL] direct write counters open=%u read=%u write=%u wfh=%llu\n", open_count,
               read_count, write_count, (unsigned long long)last_write_fh);
        goto fail;
    }

    close(cached_fd);
    cached_fd = -1;
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (direct_fd >= 0) {
        close(direct_fd);
    }
    if (cached_fd >= 0) {
        close(cached_fd);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_direct_io_mmap_policy() {
    const char *mp = "/tmp/test_fuse_direct_mmap";
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    char warm = 0;
    pid_t child = -1;
    struct direct_mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_out_flags;
        volatile unsigned char first_byte;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
        volatile uint64_t read_fhs[4];
    };
    struct direct_mmap_shared_state *shared =
        (struct direct_mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                                MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    child = fork();
    if (child < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (child == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.read_fhs = shared->read_fhs;
        child_args.read_trace_capacity = 4;
        child_args.next_open_fh = 800;
        child_args.dynamic_hello_open_out_flags = &shared->open_out_flags;
        child_args.dynamic_hello_first_byte = &shared->first_byte;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(child, SIGTERM);
        waitpid(child, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if (pread(f, &warm, 1, 0) != 1 || warm != 'h') {
        printf("[FAIL] warm cached read got=%d read=%u errno=%d\n", warm, shared->read_count,
               errno);
        goto fail;
    }
    close(f);
    f = -1;

    shared->open_out_flags = FOPEN_DIRECT_IO;
    shared->first_byte = 'Z';

    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] direct open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }

    errno = 0;
    addr = mmap(NULL, 4096, PROT_READ, MAP_SHARED, f, 0);
    if (addr != MAP_FAILED) {
        printf("[FAIL] direct_io MAP_SHARED unexpectedly succeeded\n");
        munmap(addr, 4096);
        addr = MAP_FAILED;
        goto fail;
    }
    if (errno != ENODEV) {
        printf("[FAIL] direct_io MAP_SHARED errno=%d expected=%d\n", errno, ENODEV);
        goto fail;
    }

    addr = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] direct_io MAP_PRIVATE mmap: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    c = ((volatile char *)addr)[0];
    if (c != 'Z') {
        printf("[FAIL] direct_io MAP_PRIVATE first byte got=%d\n", c);
        goto fail;
    }
    if (shared->open_count != 2 || shared->read_count != 2 || shared->read_fhs[1] != 801) {
        printf("[FAIL] direct mmap counters open=%u read=%u fh0=%llu fh1=%llu\n",
               shared->open_count, shared->read_count, (unsigned long long)shared->read_fhs[0],
               (unsigned long long)shared->read_fhs[1]);
        goto fail;
    }

    munmap(addr, 4096);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(child, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (child > 0) {
        kill(child, SIGTERM);
        waitpid(child, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_shared_writable_mmap_msync_writeback() {
    const char *mp = "/tmp/test_fuse_mmap_shared_write";
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t daemon = -1;
    const uint32_t expected_writeback_flags = FUSE_WRITE_CACHE;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
        volatile uint32_t write_count;
        volatile uint64_t last_write_fh;
        volatile uint32_t last_open_pid;
        volatile uint64_t last_write_offset;
        volatile uint32_t last_write_size;
        volatile uint32_t last_write_flags;
        volatile uint32_t last_write_open_flags;
        volatile uint32_t last_write_uid;
        volatile uint32_t last_write_gid;
        volatile uint32_t last_write_pid;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.enable_write_ops = 1;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.write_count = &shared->write_count;
        child_args.last_write_fh = &shared->last_write_fh;
        child_args.last_open_pid = &shared->last_open_pid;
        child_args.last_write_offset = &shared->last_write_offset;
        child_args.last_write_size = &shared->last_write_size;
        child_args.last_write_flags = &shared->last_write_flags;
        child_args.last_write_open_flags = &shared->last_write_open_flags;
        child_args.last_write_uid = &shared->last_write_uid;
        child_args.last_write_gid = &shared->last_write_gid;
        child_args.last_write_pid = &shared->last_write_pid;
        child_args.next_open_fh = 900;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }

    c = ((volatile char *)addr)[0];
    if (c != 'h') {
        printf("[FAIL] shared writable mmap first byte got=%d\n", c);
        goto fail;
    }
    ((volatile char *)addr)[1] = 'M';
    if (msync(addr, 4096, MS_SYNC) != 0) {
        printf("[FAIL] msync(shared writable mmap): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 1 || shared->write_count != 1 ||
        shared->last_write_fh != 900 || shared->last_write_offset != 0 ||
        shared->last_write_size != 16 || shared->last_write_flags != expected_writeback_flags ||
        shared->last_write_open_flags != 0 || shared->last_write_uid != 0 ||
        shared->last_write_gid != 0 || shared->last_open_pid == 0 ||
        shared->last_write_pid != shared->last_open_pid) {
        printf("[FAIL] shared writable mmap counters open=%u read=%u write=%u wfh=%llu open_pid=%u off=%llu size=%u wflags=%u oflags=%u uid=%u gid=%u pid=%u\n",
               shared->open_count, shared->read_count, shared->write_count,
               (unsigned long long)shared->last_write_fh, shared->last_open_pid,
               (unsigned long long)shared->last_write_offset, shared->last_write_size,
               shared->last_write_flags, shared->last_write_open_flags, shared->last_write_uid,
               shared->last_write_gid, shared->last_write_pid);
        goto fail;
    }

    munmap(addr, 4096);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_shared_mmap_dirty_then_pwrite_keeps_latest_data() {
    const char *mp = "/tmp/test_fuse_mmap_dirty_pwrite";
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t daemon = -1;
    bool saw_direct_pwrite = false;
    bool saw_cache_writeback = false;
    int direct_pwrite_index = -1;
    int stale_cover_index = -1;
    unsigned char stale_cover_byte = 0;
    uint32_t traced_writes = 0;
    struct dirty_pwrite_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t read_count;
        volatile uint32_t write_count;
        volatile uint64_t last_write_offset;
        volatile uint32_t last_write_size;
        volatile uint32_t last_write_flags;
        volatile unsigned char last_write_watch_byte;
        volatile uint64_t write_offsets[8];
        volatile uint32_t write_sizes[8];
        volatile uint32_t write_flags[8];
        volatile unsigned char write_watch_bytes[8];
        volatile unsigned char write_covers_watch[8];
        volatile unsigned char backend_watch_byte;
    };
    struct dirty_pwrite_shared_state *shared =
        (struct dirty_pwrite_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                                 MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.enable_write_ops = 1;
        child_args.stop_on_destroy = 1;
        child_args.read_count = &shared->read_count;
        child_args.write_count = &shared->write_count;
        child_args.last_write_offset = &shared->last_write_offset;
        child_args.last_write_size = &shared->last_write_size;
        child_args.last_write_flags = &shared->last_write_flags;
        child_args.last_write_watch_byte = &shared->last_write_watch_byte;
        child_args.write_offsets = shared->write_offsets;
        child_args.write_sizes = shared->write_sizes;
        child_args.write_flags = shared->write_flags;
        child_args.write_watch_bytes = shared->write_watch_bytes;
        child_args.write_covers_watch = shared->write_covers_watch;
        child_args.backend_watch_byte = &shared->backend_watch_byte;
        child_args.write_trace_capacity = 8;
        child_args.write_watch_offset = 1;
        child_args.next_open_fh = 901;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }

    c = ((volatile char *)addr)[0];
    if (c != 'h') {
        printf("[FAIL] shared writable mmap first byte got=%d\n", c);
        goto fail;
    }
    ((volatile char *)addr)[1] = 'M';
    if (pwrite(f, "P", 1, 1) != 1) {
        printf("[FAIL] pwrite over dirty mmap byte: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (((volatile char *)addr)[1] != 'P') {
        printf("[FAIL] mmap cache was not updated by overlapping pwrite got=%d\n",
               ((volatile char *)addr)[1]);
        goto fail;
    }
    if (msync(addr, 4096, MS_SYNC) != 0) {
        printf("[FAIL] msync(shared dirty pwrite): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    __sync_synchronize();
    traced_writes = shared->write_count;
    if (traced_writes > 8) {
        printf("[FAIL] dirty mmap pwrite write trace truncated read=%u write=%u\n",
               shared->read_count, traced_writes);
        goto fail;
    }

    saw_direct_pwrite = false;
    saw_cache_writeback = false;
    direct_pwrite_index = -1;
    stale_cover_index = -1;
    stale_cover_byte = 0;
    for (uint32_t i = 0; i < traced_writes; ++i) {
        uint64_t off = shared->write_offsets[i];
        uint32_t size = shared->write_sizes[i];
        uint32_t flags = shared->write_flags[i];
        unsigned char watch = shared->write_watch_bytes[i];
        bool covers_watch = shared->write_covers_watch[i] != 0;

        if (off == 1 && size == 1 && flags == 0 && watch == 'P') {
            saw_direct_pwrite = true;
            direct_pwrite_index = (int)i;
        }
        if (off == 0 && size == 16 && flags == FUSE_WRITE_CACHE && covers_watch) {
            saw_cache_writeback = true;
        }
        if (direct_pwrite_index >= 0 && (int)i > direct_pwrite_index && covers_watch &&
            watch != 'P') {
            stale_cover_index = (int)i;
            stale_cover_byte = watch;
        }
    }

    if (traced_writes < 2 || !saw_direct_pwrite || !saw_cache_writeback ||
        stale_cover_index >= 0 || shared->backend_watch_byte != 'P') {
        printf("[FAIL] dirty mmap pwrite counters read=%u write=%u last_off=%llu last_size=%u last_flags=%u last_watched=%u backend=%u saw_pwrite=%d saw_cache=%d stale_index=%d stale_byte=%u\n",
               shared->read_count, traced_writes,
               (unsigned long long)shared->last_write_offset, shared->last_write_size,
               shared->last_write_flags, shared->last_write_watch_byte,
               shared->backend_watch_byte, saw_direct_pwrite ? 1 : 0,
               saw_cache_writeback ? 1 : 0, stale_cover_index, stale_cover_byte);
        for (uint32_t i = 0; i < traced_writes; ++i) {
            printf("[FAIL] dirty mmap pwrite write[%u] off=%llu size=%u flags=%u covers=%u watched=%u\n",
                   i, (unsigned long long)shared->write_offsets[i], shared->write_sizes[i],
                   shared->write_flags[i], shared->write_covers_watch[i],
                   shared->write_watch_bytes[i]);
        }
        goto fail;
    }

    munmap(addr, 4096);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_shared_writable_mmap_osync_writeback() {
    const char *mp = "/tmp/test_fuse_mmap_shared_osync";
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t daemon = -1;
    const uint32_t expected_writeback_flags = FUSE_WRITE_CACHE;
    const size_t page_size = 4096;
    const size_t map_len = page_size * 2;
    const char marker = 'Z';
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
        volatile uint32_t write_count;
        volatile uint32_t fsync_count;
        volatile uint64_t last_write_fh;
        volatile uint64_t last_write_offset;
        volatile uint32_t last_write_size;
        volatile uint32_t last_write_flags;
        volatile uint32_t last_write_open_flags;
        volatile uint64_t last_fsync_fh;
        volatile uint32_t write_count_at_fsync;
        volatile uint32_t last_write_flags_at_fsync;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.enable_write_ops = 1;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.write_count = &shared->write_count;
        child_args.fsync_count = &shared->fsync_count;
        child_args.last_write_fh = &shared->last_write_fh;
        child_args.last_write_offset = &shared->last_write_offset;
        child_args.last_write_size = &shared->last_write_size;
        child_args.last_write_flags = &shared->last_write_flags;
        child_args.last_write_open_flags = &shared->last_write_open_flags;
        child_args.last_fsync_fh = &shared->last_fsync_fh;
        child_args.write_count_at_fsync = &shared->write_count_at_fsync;
        child_args.last_write_flags_at_fsync = &shared->last_write_flags_at_fsync;
        child_args.next_open_fh = 930;
        child_args.hello_data_size_override = map_len;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR | O_SYNC);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, map_len, PROT_READ | PROT_WRITE, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }

    c = ((volatile char *)addr)[0];
    if (c != 'A') {
        printf("[FAIL] shared writable mmap first byte got=%d\n", c);
        goto fail;
    }
    ((volatile char *)addr)[2] = 'F';
    if (pwrite(f, &marker, 1, (off_t)page_size) != 1) {
        printf("[FAIL] pwrite(O_SYNC): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    if (shared->open_count != 1 || shared->read_count != 1 || shared->write_count != 2 ||
        shared->fsync_count != 1 || shared->last_write_fh != 930 || shared->last_fsync_fh != 930 ||
        shared->last_write_offset != 0 || shared->last_write_size != page_size ||
        shared->last_write_flags != expected_writeback_flags || shared->last_write_open_flags != 0 ||
        shared->write_count_at_fsync != 2 ||
        shared->last_write_flags_at_fsync != expected_writeback_flags) {
        printf("[FAIL] shared mmap osync counters open=%u read=%u write=%u fsync=%u wfh=%llu fsh=%llu off=%llu size=%u wflags=%u oflags=%u fsync_writes=%u fsync_wflags=%u\n",
               shared->open_count, shared->read_count, shared->write_count, shared->fsync_count,
               (unsigned long long)shared->last_write_fh,
               (unsigned long long)shared->last_fsync_fh,
               (unsigned long long)shared->last_write_offset, shared->last_write_size,
               shared->last_write_flags, shared->last_write_open_flags,
               shared->write_count_at_fsync, shared->last_write_flags_at_fsync);
        goto fail;
    }

    munmap(addr, map_len);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, map_len);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_shared_mmap_mprotect_writeback() {
    const char *mp = "/tmp/test_fuse_mmap_mprotect_write";
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t daemon = -1;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
        volatile uint32_t write_count;
        volatile uint64_t last_write_fh;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.enable_write_ops = 1;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.write_count = &shared->write_count;
        child_args.last_write_fh = &shared->last_write_fh;
        child_args.next_open_fh = 910;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, 4096, PROT_READ, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }

    c = ((volatile char *)addr)[0];
    if (c != 'h') {
        printf("[FAIL] mmap first byte got=%d\n", c);
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 1) {
        printf("[FAIL] before mprotect counters open=%u read=%u\n", shared->open_count,
               shared->read_count);
        goto fail;
    }
    if (mprotect(addr, 4096, PROT_READ | PROT_WRITE) != 0) {
        printf("[FAIL] mprotect shared writable FUSE mapping: %s (errno=%d)\n", strerror(errno),
               errno);
        goto fail;
    }
    ((volatile char *)addr)[2] = 'P';
    if (msync(addr, 4096, MS_SYNC) != 0) {
        printf("[FAIL] msync(after mprotect): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 1 || shared->write_count != 1 ||
        shared->last_write_fh != 910) {
        printf("[FAIL] after mprotect counters open=%u read=%u write=%u wfh=%llu\n",
               shared->open_count, shared->read_count, shared->write_count,
               (unsigned long long)shared->last_write_fh);
        goto fail;
    }

    munmap(addr, 4096);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_shared_mmap_readonly_fd_mprotect_write_denied() {
    const char *mp = "/tmp/test_fuse_mmap_readonly_mprotect";
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t daemon = -1;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
        volatile uint32_t write_count;
        volatile uint64_t last_write_fh;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.enable_write_ops = 1;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.write_count = &shared->write_count;
        child_args.last_write_fh = &shared->last_write_fh;
        child_args.next_open_fh = 930;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s, O_RDONLY): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, 4096, PROT_READ, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }

    c = ((volatile char *)addr)[0];
    if (c != 'h') {
        printf("[FAIL] readonly shared mmap first byte got=%d\n", c);
        goto fail;
    }
    errno = 0;
    if (mprotect(addr, 4096, PROT_READ | PROT_WRITE) == 0) {
        printf("[FAIL] mprotect unexpectedly allowed write upgrade on readonly fd\n");
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 1 || shared->write_count != 0 ||
        shared->last_write_fh != 0) {
        printf("[FAIL] readonly mprotect counters open=%u read=%u write=%u wfh=%llu\n",
               shared->open_count, shared->read_count, shared->write_count,
               (unsigned long long)shared->last_write_fh);
        goto fail;
    }

    munmap(addr, 4096);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_shared_writable_mmap_munmap_writeback_without_msync() {
    const char *mp = "/tmp/test_fuse_mmap_munmap_writeback";
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    const uint32_t expected_writeback_flags = FUSE_WRITE_CACHE;
    pid_t daemon = -1;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
        volatile uint32_t write_count;
        volatile uint64_t last_write_fh;
        volatile uint64_t last_write_offset;
        volatile uint32_t last_write_size;
        volatile uint32_t last_write_flags;
        volatile uint32_t last_write_open_flags;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.enable_write_ops = 1;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.write_count = &shared->write_count;
        child_args.last_write_fh = &shared->last_write_fh;
        child_args.last_write_offset = &shared->last_write_offset;
        child_args.last_write_size = &shared->last_write_size;
        child_args.last_write_flags = &shared->last_write_flags;
        child_args.last_write_open_flags = &shared->last_write_open_flags;
        child_args.next_open_fh = 940;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }

    if (((volatile char *)addr)[0] != 'h') {
        printf("[FAIL] shared close-writeback mmap first byte got=%d\n",
               ((volatile char *)addr)[0]);
        goto fail;
    }
    ((volatile char *)addr)[3] = 'C';
    if (munmap(addr, 4096) != 0) {
        printf("[FAIL] munmap(shared writable mmap): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    addr = MAP_FAILED;

    if (shared->open_count != 1 || shared->read_count != 1 || shared->write_count != 1 ||
        shared->last_write_fh != 940 || shared->last_write_offset != 0 ||
        shared->last_write_size != 16 || shared->last_write_flags != expected_writeback_flags ||
        shared->last_write_open_flags != 0) {
        printf("[FAIL] munmap writeback counters open=%u read=%u write=%u wfh=%llu off=%llu size=%u wflags=%u oflags=%u\n",
               shared->open_count, shared->read_count, shared->write_count,
               (unsigned long long)shared->last_write_fh,
               (unsigned long long)shared->last_write_offset, shared->last_write_size,
               shared->last_write_flags, shared->last_write_open_flags);
        goto fail;
    }

    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_shared_mmap_subrange_mprotect_writeback_preserves_vma() {
    const char *mp = "/tmp/test_fuse_mmap_mprotect_subrange";
    const size_t page_size = 4096;
    const size_t map_len = page_size * 2;
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t daemon = -1;
    struct sigaction old_segv;
    bool segv_handler_installed = false;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
        volatile uint32_t write_count;
        volatile uint64_t last_write_fh;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.stop_on_destroy = 1;
        child_args.enable_write_ops = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.write_count = &shared->write_count;
        child_args.last_write_fh = &shared->last_write_fh;
        child_args.hello_data_size_override = map_len;
        child_args.next_open_fh = 920;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, map_len, PROT_READ, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }

    c = ((volatile char *)addr)[0];
    if (c != 'A') {
        printf("[FAIL] first page byte got=%d\n", c);
        goto fail;
    }
    c = ((volatile char *)addr)[page_size];
    if (c != 'O') {
        printf("[FAIL] second page byte got=%d\n", c);
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 2) {
        printf("[FAIL] before subrange mprotect counters open=%u read=%u\n",
               shared->open_count, shared->read_count);
        goto fail;
    }
    if (mprotect((char *)addr + page_size, page_size, PROT_READ | PROT_WRITE) != 0) {
        printf("[FAIL] subrange mprotect(shared writable): %s (errno=%d)\n", strerror(errno),
               errno);
        goto fail;
    }
    ((volatile char *)addr)[page_size + 1] = 'S';
    if (msync((char *)addr + page_size, page_size, MS_SYNC) != 0) {
        printf("[FAIL] msync(subrange shared writable): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (shared->write_count != 1 || shared->last_write_fh != 920) {
        printf("[FAIL] subrange writeback counters write=%u wfh=%llu\n", shared->write_count,
               (unsigned long long)shared->last_write_fh);
        goto fail;
    }
    if (mprotect(addr, page_size, PROT_NONE) != 0) {
        printf("[FAIL] mprotect(PROT_NONE first page): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = fuse_sigsegv_longjmp_handler;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGSEGV, &sa, &old_segv) != 0) {
        printf("[FAIL] sigaction(SIGSEGV): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    segv_handler_installed = true;
    g_fuse_sigsegv_seen = 0;
    if (sigsetjmp(g_fuse_sigsegv_jmp, 1) == 0) {
        c = ((volatile char *)addr)[0];
        (void)c;
    }
    sigaction(SIGSEGV, &old_segv, NULL);
    segv_handler_installed = false;
    if (!g_fuse_sigsegv_seen) {
        printf("[FAIL] first page remained readable after PROT_NONE\n");
        goto fail;
    }

    munmap(addr, map_len);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (segv_handler_installed) {
        sigaction(SIGSEGV, &old_segv, NULL);
    }
    if (addr != MAP_FAILED) {
        munmap(addr, map_len);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_shared_mmap_unfaulted_mprotect_prot_none() {
    const char *mp = "/tmp/test_fuse_mmap_unfaulted_mprotect";
    const size_t page_size = 4096;
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t daemon = -1;
    struct sigaction old_segv;
    bool segv_handler_installed = false;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.hello_data_size_override = page_size;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, page_size, PROT_READ, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 0) {
        printf("[FAIL] before unfaulted mprotect counters open=%u read=%u\n",
               shared->open_count, shared->read_count);
        goto fail;
    }
    if (mprotect(addr, page_size, PROT_NONE) != 0) {
        printf("[FAIL] mprotect(PROT_NONE unfaulted): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = fuse_sigsegv_longjmp_handler;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGSEGV, &sa, &old_segv) != 0) {
        printf("[FAIL] sigaction(SIGSEGV): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    segv_handler_installed = true;
    g_fuse_sigsegv_seen = 0;
    if (sigsetjmp(g_fuse_sigsegv_jmp, 1) == 0) {
        c = ((volatile char *)addr)[0];
        (void)c;
    }
    sigaction(SIGSEGV, &old_segv, NULL);
    segv_handler_installed = false;
    if (!g_fuse_sigsegv_seen) {
        printf("[FAIL] unfaulted PROT_NONE mapping remained readable\n");
        goto fail;
    }
    if (shared->read_count != 0) {
        printf("[FAIL] unfaulted PROT_NONE triggered read_count=%u\n", shared->read_count);
        goto fail;
    }

    munmap(addr, page_size);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (segv_handler_installed) {
        sigaction(SIGSEGV, &old_segv, NULL);
    }
    if (addr != MAP_FAILED) {
        munmap(addr, page_size);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_mmap_truncate_unmaps_stale_page() {
    const char *mp = "/tmp/test_fuse_mmap_truncate";
    const size_t page_size = 4096;
    const size_t map_len = page_size * 2;
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t daemon = -1;
    struct sigaction old_bus;
    bool bus_handler_installed = false;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.stop_on_destroy = 1;
        child_args.enable_write_ops = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.hello_data_size_override = map_len;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, map_len, PROT_READ, MAP_PRIVATE, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }

    c = ((volatile char *)addr)[page_size];
    if (c != 'O') {
        printf("[FAIL] second page byte before truncate got=%d\n", c);
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 1) {
        printf("[FAIL] before truncate counters open=%u read=%u\n", shared->open_count,
               shared->read_count);
        goto fail;
    }
    if (ftruncate(f, page_size) != 0) {
        printf("[FAIL] ftruncate: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = fuse_sigbus_longjmp_handler;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGBUS, &sa, &old_bus) != 0) {
        printf("[FAIL] sigaction(SIGBUS): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    bus_handler_installed = true;
    g_fuse_sigbus_seen = 0;
    if (sigsetjmp(g_fuse_sigbus_jmp, 1) == 0) {
        c = ((volatile char *)addr)[page_size];
        (void)c;
    }
    sigaction(SIGBUS, &old_bus, NULL);
    bus_handler_installed = false;
    if (!g_fuse_sigbus_seen) {
        printf("[FAIL] truncated second page remained readable read=%u\n", shared->read_count);
        goto fail;
    }
    if (shared->read_count != 1) {
        printf("[FAIL] truncated EOF fault issued extra FUSE_READ count=%u\n", shared->read_count);
        goto fail;
    }

    munmap(addr, map_len);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (bus_handler_installed) {
        sigaction(SIGBUS, &old_bus, NULL);
    }
    if (addr != MAP_FAILED) {
        munmap(addr, map_len);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_fadvise_without_page_cache() {
    const char *mp = "/tmp/test_fuse_fadvise";
    char path[256];
    int f = -1;
    const int advices[] = {
        POSIX_FADV_NORMAL,     POSIX_FADV_RANDOM, POSIX_FADV_SEQUENTIAL,
        POSIX_FADV_WILLNEED,   POSIX_FADV_DONTNEED,
        POSIX_FADV_NOREUSE,
    };

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }

    for (size_t i = 0; i < sizeof(advices) / sizeof(advices[0]); i++) {
        int rc = posix_fadvise(f, 0, 0, advices[i]);
        if (rc != 0) {
            printf("[FAIL] posix_fadvise(advice=%d): rc=%d\n", advices[i], rc);
            goto fail;
        }
    }

    if (posix_fadvise(f, 0, -1, POSIX_FADV_NORMAL) != EINVAL) {
        printf("[FAIL] posix_fadvise negative len should return EINVAL\n");
        goto fail;
    }

    close(f);
    f = -1;
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_mount_on_fuse_dir_uses_namespace_path() {
    const char *mp = "/tmp/test_fuse_mount_target";
    char dir_path[512];
    char marker_path[1024];
    int ramfs_mounted = 0;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(dir_path, sizeof(dir_path), "%s/ramfs_target", mp);
    if (mkdir(dir_path, 0755) != 0) {
        printf("[FAIL] mkdir(%s): %s (errno=%d)\n", dir_path, strerror(errno), errno);
        goto fail;
    }

    if (mount("", dir_path, "ramfs", 0, NULL) != 0) {
        printf("[FAIL] mount(ramfs on fuse dir): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    ramfs_mounted = 1;

    snprintf(marker_path, sizeof(marker_path), "%s/marker", dir_path);
    if (fuseg_write_file(marker_path, "mounted") != 0) {
        printf("[FAIL] write marker under ramfs: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    if (umount(dir_path) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", dir_path, strerror(errno), errno);
        goto fail_no_ramfs_umount;
    }
    ramfs_mounted = 0;
    if (rmdir(dir_path) != 0) {
        printf("[FAIL] rmdir(%s): %s (errno=%d)\n", dir_path, strerror(errno), errno);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (ramfs_mounted) {
        umount(dir_path);
    }
fail_no_ramfs_umount:
    rmdir(dir_path);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_rename_updates_fuse_dir_cwd_path() {
    const char *mp = "/tmp/test_fuse_rename_path";
    char old_path[512];
    char new_path[512];
    char cwd[512];
    int dir_fd = -1;
    int ramfs_mounted = 0;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(old_path, sizeof(old_path), "%s/old_dir", mp);
    snprintf(new_path, sizeof(new_path), "%s/new_dir", mp);
    if (mkdir(old_path, 0755) != 0) {
        printf("[FAIL] mkdir(%s): %s (errno=%d)\n", old_path, strerror(errno), errno);
        goto fail;
    }
    dir_fd = open(old_path, O_RDONLY | O_DIRECTORY);
    if (dir_fd < 0) {
        printf("[FAIL] open dir fd %s: %s (errno=%d)\n", old_path, strerror(errno), errno);
        goto fail;
    }
    if (rename(old_path, new_path) != 0) {
        printf("[FAIL] rename(%s -> %s): %s (errno=%d)\n", old_path, new_path, strerror(errno),
               errno);
        goto fail;
    }
    if (fchdir(dir_fd) != 0) {
        printf("[FAIL] fchdir renamed dir fd: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (!getcwd(cwd, sizeof(cwd))) {
        printf("[FAIL] getcwd after rename: %s (errno=%d)\n", strerror(errno), errno);
        goto fail_chdir_root;
    }
    if (strcmp(cwd, new_path) != 0) {
        printf("[FAIL] getcwd after rename: got '%s', want '%s'\n", cwd, new_path);
        goto fail_chdir_root;
    }
    if (chdir("/") != 0) {
        printf("[FAIL] chdir(/): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(dir_fd);
    dir_fd = -1;

    if (mount("", new_path, "ramfs", 0, NULL) != 0) {
        printf("[FAIL] mount(ramfs on renamed fuse dir): %s (errno=%d)\n", strerror(errno),
               errno);
        goto fail;
    }
    ramfs_mounted = 1;
    if (umount(new_path) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", new_path, strerror(errno), errno);
        goto fail_no_ramfs_umount;
    }
    ramfs_mounted = 0;
    if (rmdir(new_path) != 0) {
        printf("[FAIL] rmdir(%s): %s (errno=%d)\n", new_path, strerror(errno), errno);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail_chdir_root:
    {
        int ignored_chdir = chdir("/");
        (void)ignored_chdir;
    }
fail:
    if (dir_fd >= 0) {
        close(dir_fd);
    }
    if (ramfs_mounted) {
        umount(new_path);
    }
fail_no_ramfs_umount:
    rmdir(new_path);
    rmdir(old_path);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_lookup_nodes_forgotten_before_umount_when_unreferenced() {
    const char *mp = "/tmp/test_fuse_lookup_lifetime";
    char parent_path[512];
    char child_path[512];
    struct stat st;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t forget_count = 0;
    volatile uint64_t forget_nlookup_sum = 0;
    volatile uint64_t forget_trace_nodeids[32] = {0};
    volatile uint64_t forget_trace_nlookups[32] = {0};
    volatile uint32_t destroy_count = 0;
    uint32_t forget_count_before_umount = 0;
    uint64_t forget_sum_before_umount = 0;
    uint32_t distinct_nonroot_before_umount = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.forget_count = &forget_count;
    args.forget_nlookup_sum = &forget_nlookup_sum;
    args.forget_trace_nodeids = forget_trace_nodeids;
    args.forget_trace_nlookups = forget_trace_nlookups;
    args.forget_trace_capacity = 32;
    args.destroy_count = &destroy_count;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(parent_path, sizeof(parent_path), "%s/parent", mp);
    snprintf(child_path, sizeof(child_path), "%s/parent/child", mp);
    if (mkdir(parent_path, 0755) != 0) {
        printf("[FAIL] mkdir(%s): %s (errno=%d)\n", parent_path, strerror(errno), errno);
        goto fail;
    }
    if (mkdir(child_path, 0755) != 0) {
        printf("[FAIL] mkdir(%s): %s (errno=%d)\n", child_path, strerror(errno), errno);
        goto fail;
    }
    if (stat(child_path, &st) != 0 || !S_ISDIR(st.st_mode)) {
        printf("[FAIL] stat(%s): %s (errno=%d)\n", child_path, strerror(errno), errno);
        goto fail;
    }
    if (stat(parent_path, &st) != 0 || !S_ISDIR(st.st_mode)) {
        printf("[FAIL] stat(%s) after child lookup: %s (errno=%d)\n", parent_path,
               strerror(errno), errno);
        goto fail;
    }

    for (int i = 0; i < 200 && forget_nlookup_sum < 2; i++) {
        usleep(10 * 1000);
    }
    if (forget_count == 0 || forget_nlookup_sum < 2) {
        printf("[FAIL] unreferenced FUSE lookup nodes not forgotten before umount: "
               "count=%u nlookup=%llu\n",
               forget_count, (unsigned long long)forget_nlookup_sum);
        goto fail;
    }
    for (uint32_t i = 0; i < forget_count && i < 32; i++) {
        if (forget_trace_nodeids[i] == 1) {
            printf("[FAIL] root node unexpectedly forgotten before umount at index=%u "
                   "nlookup=%llu\n",
                   i, (unsigned long long)forget_trace_nlookups[i]);
            goto fail;
        }
    }
    distinct_nonroot_before_umount = 0;
    for (uint32_t i = 0; i < forget_count && i < 32; i++) {
        if (forget_trace_nodeids[i] == 0 || forget_trace_nodeids[i] == 1) {
            continue;
        }
        bool seen = false;
        for (uint32_t j = 0; j < i; j++) {
            if (forget_trace_nodeids[j] == forget_trace_nodeids[i]) {
                seen = true;
                break;
            }
        }
        if (!seen) {
            distinct_nonroot_before_umount++;
        }
    }
    if (distinct_nonroot_before_umount < 2) {
        printf("[FAIL] expected at least two distinct non-root nodes forgotten before umount, "
               "got=%u count=%u nlookup=%llu\n",
               distinct_nonroot_before_umount, forget_count,
               (unsigned long long)forget_nlookup_sum);
        goto fail;
    }

    forget_count_before_umount = forget_count;
    forget_sum_before_umount = forget_nlookup_sum;

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }
    for (int i = 0; i < 200 && destroy_count == 0; i++) {
        usleep(10 * 1000);
    }
    if (destroy_count == 0) {
        printf("[FAIL] timed out waiting for FUSE_DESTROY after umount\n");
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    close(fd);
    pthread_join(th, NULL);
    if (destroy_count != 1 || forget_count < forget_count_before_umount ||
        forget_nlookup_sum < forget_sum_before_umount) {
        printf("[FAIL] FUSE teardown lost forget accounting or missed destroy: "
               "forget=%u/%u nlookup=%llu/%llu destroy=%u\n",
               forget_count, forget_count_before_umount, (unsigned long long)forget_nlookup_sum,
               (unsigned long long)forget_sum_before_umount, destroy_count);
        rmdir(mp);
        return -1;
    }
    for (uint32_t i = 0; i < forget_count && i < 32; i++) {
        if (forget_trace_nodeids[i] == 1) {
            printf("[FAIL] root node unexpectedly forgotten at index=%u nlookup=%llu\n", i,
                   (unsigned long long)forget_trace_nlookups[i]);
            rmdir(mp);
            return -1;
        }
    }
    rmdir(mp);
    return 0;

fail:
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static bool forget_trace_contains(volatile uint64_t *nodeids, uint32_t count, uint64_t nodeid) {
    for (uint32_t i = 0; i < count && i < 32; i++) {
        if (nodeids[i] == nodeid) {
            return true;
        }
    }
    return false;
}

static bool forget_trace_contains_pair(volatile uint64_t *nodeids,
                                       volatile uint64_t *nlookups,
                                       uint32_t count, uint64_t nodeid,
                                       uint64_t nlookup) {
    for (uint32_t i = 0; i < count && i < 32; i++) {
        if (nodeids[i] == nodeid && nlookups[i] == nlookup) {
            return true;
        }
    }
    return false;
}

static int ext_test_positive_lookup_cache_expires_and_forgets_before_umount() {
    const char *mp = "/tmp/test_fuse_positive_lookup_lifetime";
    char parent_path[512];
    char child_path[512];
    char hello_path[512];
    struct stat st;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t forget_count = 0;
    volatile uint64_t forget_nlookup_sum = 0;
    volatile uint64_t forget_trace_nodeids[32] = {0};
    volatile uint64_t forget_trace_nlookups[32] = {0};
    volatile uint32_t destroy_count = 0;
    uint32_t forget_count_before_umount = 0;
    uint64_t forget_sum_before_umount = 0;
    uint64_t parent_nodeid = 0;
    uint64_t child_nodeid = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.forget_count = &forget_count;
    args.forget_nlookup_sum = &forget_nlookup_sum;
    args.forget_trace_nodeids = forget_trace_nodeids;
    args.forget_trace_nlookups = forget_trace_nlookups;
    args.forget_trace_capacity = 32;
    args.destroy_count = &destroy_count;
    args.entry_valid_sec = 1;
    args.attr_valid_sec = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(parent_path, sizeof(parent_path), "%s/parent", mp);
    snprintf(child_path, sizeof(child_path), "%s/parent/child", mp);
    snprintf(hello_path, sizeof(hello_path), "%s/hello.txt", mp);
    if (mkdir(parent_path, 0755) != 0) {
        printf("[FAIL] mkdir(%s): %s (errno=%d)\n", parent_path, strerror(errno), errno);
        goto fail;
    }
    if (stat(parent_path, &st) != 0 || !S_ISDIR(st.st_mode)) {
        printf("[FAIL] stat(%s): %s (errno=%d)\n", parent_path, strerror(errno), errno);
        goto fail;
    }
    parent_nodeid = (uint64_t)st.st_ino;
    if (mkdir(child_path, 0755) != 0) {
        printf("[FAIL] mkdir(%s): %s (errno=%d)\n", child_path, strerror(errno), errno);
        goto fail;
    }
    if (stat(child_path, &st) != 0 || !S_ISDIR(st.st_mode)) {
        printf("[FAIL] stat(%s): %s (errno=%d)\n", child_path, strerror(errno), errno);
        goto fail;
    }
    child_nodeid = (uint64_t)st.st_ino;

    usleep(2000 * 1000);
    if (stat(hello_path, &st) != 0 || !S_ISREG(st.st_mode)) {
        printf("[FAIL] stat(%s) after TTL: %s (errno=%d)\n", hello_path, strerror(errno), errno);
        goto fail;
    }

    for (int i = 0; i < 200; i++) {
        uint32_t count = forget_count;
        if (forget_trace_contains(forget_trace_nodeids, count, parent_nodeid) &&
            forget_trace_contains(forget_trace_nodeids, count, child_nodeid)) {
            break;
        }
        usleep(10 * 1000);
    }
    if (!forget_trace_contains(forget_trace_nodeids, forget_count, parent_nodeid) ||
        !forget_trace_contains(forget_trace_nodeids, forget_count, child_nodeid)) {
        printf("[FAIL] positive TTL cache-only nodes were not forgotten before umount: "
               "count=%u nlookup=%llu parent=%llu child=%llu saw_parent=%d saw_child=%d\n",
               forget_count, (unsigned long long)forget_nlookup_sum,
               (unsigned long long)parent_nodeid, (unsigned long long)child_nodeid,
               forget_trace_contains(forget_trace_nodeids, forget_count, parent_nodeid),
               forget_trace_contains(forget_trace_nodeids, forget_count, child_nodeid));
        goto fail;
    }
    if (forget_trace_contains(forget_trace_nodeids, forget_count, 1)) {
        printf("[FAIL] root node unexpectedly forgotten before umount\n");
        goto fail;
    }

    forget_count_before_umount = forget_count;
    forget_sum_before_umount = forget_nlookup_sum;
    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }
    for (int i = 0; i < 200 && destroy_count == 0; i++) {
        usleep(10 * 1000);
    }
    if (destroy_count == 0) {
        printf("[FAIL] timed out waiting for FUSE_DESTROY after umount\n");
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    close(fd);
    pthread_join(th, NULL);
    if (destroy_count != 1 || forget_count < forget_count_before_umount ||
        forget_nlookup_sum < forget_sum_before_umount ||
        forget_trace_contains(forget_trace_nodeids, forget_count, 1)) {
        printf("[FAIL] FUSE teardown regressed: forget=%u/%u nlookup=%llu/%llu destroy=%u "
               "root_forget=%d\n",
               forget_count, forget_count_before_umount, (unsigned long long)forget_nlookup_sum,
               (unsigned long long)forget_sum_before_umount, destroy_count,
               forget_trace_contains(forget_trace_nodeids, forget_count, 1));
        rmdir(mp);
        return -1;
    }
    rmdir(mp);
    return 0;

fail:
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_active_directory_parent_survives_lookup_cache_prune() {
    const char *mp = "/tmp/test_fuse_active_parent_prune";
    char parent_path[512];
    char child_path[512];
    char hello_path[512];
    struct stat st;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t destroy_count = 0;
    volatile uint32_t lookup_count = 0;
    uint32_t lookup_count_before_parent_relookup = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.destroy_count = &destroy_count;
    args.lookup_count = &lookup_count;
    args.entry_valid_sec = 1;
    args.attr_valid_sec = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(parent_path, sizeof(parent_path), "%s/parent", mp);
    snprintf(child_path, sizeof(child_path), "%s/parent/child", mp);
    snprintf(hello_path, sizeof(hello_path), "%s/hello.txt", mp);
    if (mkdir(parent_path, 0755) != 0 || mkdir(child_path, 0755) != 0) {
        printf("[FAIL] mkdir active parent tree: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (chdir(child_path) != 0) {
        printf("[FAIL] chdir(%s): %s (errno=%d)\n", child_path, strerror(errno), errno);
        goto fail;
    }
    usleep(2000 * 1000);
    if (stat(hello_path, &st) != 0 || !S_ISREG(st.st_mode)) {
        printf("[FAIL] stat(%s) after TTL: %s (errno=%d)\n", hello_path, strerror(errno), errno);
        goto fail_chdir;
    }
    lookup_count_before_parent_relookup = lookup_count;
    if (stat(parent_path, &st) != 0 || !S_ISDIR(st.st_mode)) {
        printf("[FAIL] stat(%s) after prune: %s (errno=%d)\n", parent_path, strerror(errno),
               errno);
        goto fail_chdir;
    }
    if (lookup_count <= lookup_count_before_parent_relookup) {
        printf("[FAIL] parent cache entry was not pruned: before=%u after=%u\n",
               lookup_count_before_parent_relookup, lookup_count);
        goto fail_chdir;
    }
    if (stat("..", &st) != 0 || !S_ISDIR(st.st_mode)) {
        printf("[FAIL] stat(..) after cache prune: %s (errno=%d)\n", strerror(errno), errno);
        goto fail_chdir;
    }

    if (chdir("/") != 0) {
        printf("[FAIL] chdir(/): %s (errno=%d)\n", strerror(errno), errno);
        goto fail_chdir;
    }
    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    for (int i = 0; i < 200 && destroy_count == 0; i++) {
        usleep(10 * 1000);
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    if (destroy_count == 0) {
        printf("[FAIL] timed out waiting for FUSE_DESTROY after umount\n");
        return -1;
    }
    return 0;

fail_chdir:
    if (chdir("/") != 0) {
        printf("[FAIL] cleanup chdir(/): %s (errno=%d)\n", strerror(errno), errno);
    }
fail:
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_lookup_self_alias_rejected_and_forgotten() {
    const char *mp = "/tmp/test_fuse_self_alias";
    char parent_path[512];
    char alias_path[512];
    struct stat st;
    uint64_t parent_nodeid = 0;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t forget_count = 0;
    volatile uint64_t forget_nlookup_sum = 0;
    volatile uint64_t forget_trace_nodeids[32] = {0};
    volatile uint64_t forget_trace_nlookups[32] = {0};

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.lookup_self_alias = 1;
    args.forget_count = &forget_count;
    args.forget_nlookup_sum = &forget_nlookup_sum;
    args.forget_trace_nodeids = forget_trace_nodeids;
    args.forget_trace_nlookups = forget_trace_nlookups;
    args.forget_trace_capacity = 32;
    args.entry_valid_sec = 60;
    args.attr_valid_sec = 60;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(parent_path, sizeof(parent_path), "%s/parent", mp);
    snprintf(alias_path, sizeof(alias_path), "%s/parent/self_alias", mp);
    if (mkdir(parent_path, 0755) != 0) {
        printf("[FAIL] mkdir(%s): %s (errno=%d)\n", parent_path, strerror(errno), errno);
        goto fail;
    }
    if (stat(parent_path, &st) != 0 || !S_ISDIR(st.st_mode)) {
        printf("[FAIL] stat(%s): %s (errno=%d)\n", parent_path, strerror(errno), errno);
        goto fail;
    }
    parent_nodeid = (uint64_t)st.st_ino;

    errno = 0;
    if (stat(alias_path, &st) == 0 || errno != EIO) {
        printf("[FAIL] self alias lookup expected EIO, ret_errno=%d\n", errno);
        goto fail;
    }
    for (int i = 0; i < 200; i++) {
        if (forget_trace_contains(forget_trace_nodeids, forget_count, parent_nodeid)) {
            break;
        }
        usleep(10 * 1000);
    }
    if (!forget_trace_contains(forget_trace_nodeids, forget_count, parent_nodeid) ||
        forget_nlookup_sum == 0) {
        printf("[FAIL] self alias lookup ref was not forgotten: parent=%llu count=%u sum=%llu\n",
               (unsigned long long)parent_nodeid, forget_count,
               (unsigned long long)forget_nlookup_sum);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_same_generation_type_mismatch_stales_old_node() {
    const char *mp = "/tmp/test_fuse_type_mismatch";
    char file_path[512];
    int old_fd = -1;
    struct stat st;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(file_path, sizeof(file_path), "%s/hello.txt", mp);
    old_fd = open(file_path, O_RDONLY);
    if (old_fd < 0) {
        printf("[FAIL] open old hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    char buf[64];
    if (read(old_fd, buf, sizeof(buf)) <= 0) {
        printf("[FAIL] initial read old hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    args.fs.nodes[1].is_dir = 1;
    args.fs.nodes[1].mode = S_IFDIR | 0755;
    args.fs.nodes[1].size = 0;
    if (stat(file_path, &st) != 0 || !S_ISDIR(st.st_mode)) {
        printf("[FAIL] stat same-generation replacement dir: %s (errno=%d) mode=%o\n",
               strerror(errno), errno, st.st_mode);
        goto fail;
    }

    errno = 0;
    if (pread(old_fd, buf, sizeof(buf), 0) >= 0 || errno != ESTALE) {
        printf("[FAIL] old fd after type mismatch expected ESTALE, errno=%d\n", errno);
        goto fail;
    }
    close(old_fd);
    old_fd = -1;

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (old_fd >= 0) {
        close(old_fd);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_readdirplus_generation_mismatch_stales_old_node() {
    const char *mp = "/tmp/test_fuse_readdirplus_generation";
    char file_path[512];
    int old_fd = -1;
    int new_fd = -1;
    DIR *dir = NULL;
    int saw = 0;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t readdirplus_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.readdirplus_count = &readdirplus_count;
    args.force_opendir_enosys = 1;
    args.init_out_flags_override =
        FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_NO_OPENDIR_SUPPORT | FUSE_DO_READDIRPLUS;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(file_path, sizeof(file_path), "%s/hello.txt", mp);
    old_fd = open(file_path, O_RDONLY);
    if (old_fd < 0) {
        printf("[FAIL] open old hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    char buf[64];
    if (read(old_fd, buf, sizeof(buf)) <= 0) {
        printf("[FAIL] initial read old hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    args.fs.nodes[1].generation = 2;
    dir = opendir(mp);
    if (!dir) {
        printf("[FAIL] opendir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }
    struct dirent *de;
    while ((de = readdir(dir)) != NULL) {
        if (strcmp(de->d_name, "hello.txt") == 0) {
            saw = 1;
        }
    }
    closedir(dir);
    dir = NULL;
    if (!saw || readdirplus_count == 0) {
        printf("[FAIL] expected hello.txt from READDIRPLUS, saw=%d count=%u\n", saw,
               readdirplus_count);
        goto fail;
    }

    errno = 0;
    if (pread(old_fd, buf, sizeof(buf), 0) >= 0) {
        printf("[FAIL] stale old fd read unexpectedly succeeded\n");
        goto fail;
    }
    close(old_fd);
    old_fd = -1;

    new_fd = open(file_path, O_RDONLY);
    if (new_fd < 0) {
        printf("[FAIL] open fresh hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (read(new_fd, buf, sizeof(buf)) <= 0) {
        printf("[FAIL] read fresh hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(new_fd);
    new_fd = -1;

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (dir) {
        closedir(dir);
    }
    if (new_fd >= 0) {
        close(new_fd);
    }
    if (old_fd >= 0) {
        close(old_fd);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_readdirplus_invalid_attr_forgets_unconsumed_entry() {
    const char *mp = "/tmp/test_fuse_readdirplus_invalid_attr";
    DIR *dir = NULL;
    int saw = 0;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t readdirplus_count = 0;
    volatile uint32_t forget_count = 0;
    volatile uint64_t forget_trace_nodeids[32] = {0};
    volatile uint64_t forget_trace_nlookups[32] = {0};

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.force_opendir_enosys = 1;
    args.init_out_flags_override =
        FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_NO_OPENDIR_SUPPORT | FUSE_DO_READDIRPLUS;
    args.entry_valid_sec = 60;
    args.attr_valid_sec = 60;
    args.readdirplus_count = &readdirplus_count;
    args.readdirplus_invalid_attr_name = "hello.txt";
    args.readdirplus_invalid_attr_size = 0x8000000000000000ULL;
    args.forget_count = &forget_count;
    args.forget_trace_nodeids = forget_trace_nodeids;
    args.forget_trace_nlookups = forget_trace_nlookups;
    args.forget_trace_capacity = 32;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    dir = opendir(mp);
    if (!dir) {
        printf("[FAIL] opendir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }
    struct dirent *de;
    while ((de = readdir(dir)) != NULL) {
        if (strcmp(de->d_name, "hello.txt") == 0) {
            saw = 1;
        }
    }
    closedir(dir);
    dir = NULL;
    if (!saw || readdirplus_count == 0) {
        printf("[FAIL] expected hello.txt from READDIRPLUS, saw=%d count=%u\n", saw,
               readdirplus_count);
        goto fail;
    }

    for (int i = 0; i < 200; i++) {
        if (forget_trace_contains_pair(forget_trace_nodeids, forget_trace_nlookups, forget_count, 2,
                                       1)) {
            break;
        }
        usleep(10 * 1000);
    }
    if (!forget_trace_contains_pair(forget_trace_nodeids, forget_trace_nlookups, forget_count, 2,
                                    1)) {
        printf("[FAIL] invalid READDIRPLUS entry was not forgotten before umount: count=%u\n",
               forget_count);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (dir) {
        closedir(dir);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_create_generation_mismatch_stales_old_node() {
    const char *mp = "/tmp/test_fuse_create_generation";
    char old_path[512];
    char new_path[512];
    int old_fd = -1;
    int new_fd = -1;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(old_path, sizeof(old_path), "%s/hello.txt", mp);
    snprintf(new_path, sizeof(new_path), "%s/reused.txt", mp);
    old_fd = open(old_path, O_RDONLY);
    if (old_fd < 0) {
        printf("[FAIL] open old hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (unlink(old_path) != 0) {
        printf("[FAIL] unlink old hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    args.create_reuse_nodeid = 2;
    args.create_generation_override = 2;
    new_fd = open(new_path, O_CREAT | O_RDWR, 0644);
    if (new_fd < 0) {
        printf("[FAIL] create reused node: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    char buf[64];
    errno = 0;
    if (pread(old_fd, buf, sizeof(buf), 0) >= 0) {
        printf("[FAIL] stale old fd after create unexpectedly succeeded\n");
        goto fail;
    }
    close(old_fd);
    old_fd = -1;
    close(new_fd);
    new_fd = -1;

    if (unlink(new_path) != 0) {
        printf("[FAIL] unlink reused node: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (new_fd >= 0) {
        close(new_fd);
    }
    if (old_fd >= 0) {
        close(old_fd);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_link_generation_mismatch_stales_old_node() {
    const char *mp = "/tmp/test_fuse_link_generation";
    char old_path[512];
    char hard_path[512];
    int old_fd = -1;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.link_reuse_old_nodeid = 1;
    args.link_generation_override = 2;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(old_path, sizeof(old_path), "%s/hello.txt", mp);
    snprintf(hard_path, sizeof(hard_path), "%s/hard.txt", mp);
    old_fd = open(old_path, O_RDONLY);
    if (old_fd < 0) {
        printf("[FAIL] open old hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (link(old_path, hard_path) != 0) {
        printf("[FAIL] link reused node: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    char buf[64];
    errno = 0;
    if (pread(old_fd, buf, sizeof(buf), 0) >= 0) {
        printf("[FAIL] stale old fd after link unexpectedly succeeded\n");
        goto fail;
    }
    close(old_fd);
    old_fd = -1;

    unlink(hard_path);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (old_fd >= 0) {
        close(old_fd);
    }
    unlink(hard_path);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_rename_replace_clears_old_target_path() {
    const char *mp = "/tmp/test_fuse_rename_replace";
    char old_path[512];
    char victim_path[512];
    char cwd[512];
    int old_fd = -1;
    int victim_fd = -1;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.allow_rename_replace = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(old_path, sizeof(old_path), "%s/old_dir", mp);
    snprintf(victim_path, sizeof(victim_path), "%s/victim_dir", mp);
    if (mkdir(old_path, 0755) != 0 || mkdir(victim_path, 0755) != 0) {
        printf("[FAIL] mkdir rename-replace dirs: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    old_fd = open(old_path, O_RDONLY | O_DIRECTORY);
    victim_fd = open(victim_path, O_RDONLY | O_DIRECTORY);
    if (old_fd < 0 || victim_fd < 0) {
        printf("[FAIL] open rename-replace dirs: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (rename(old_path, victim_path) != 0) {
        printf("[FAIL] rename replace: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (fchdir(old_fd) != 0 || !getcwd(cwd, sizeof(cwd)) || strcmp(cwd, victim_path) != 0) {
        printf("[FAIL] source fd path after rename replace: cwd='%s' errno=%d (%s)\n", cwd, errno,
               strerror(errno));
        goto fail_chdir_root;
    }
    if (chdir("/") != 0) {
        printf("[FAIL] chdir(/): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    errno = 0;
    if (fchdir(victim_fd) == 0) {
        printf("[FAIL] replaced target fd still resolved to a path\n");
        goto fail_chdir_root;
    }
    close(old_fd);
    close(victim_fd);
    old_fd = -1;
    victim_fd = -1;

    rmdir(victim_path);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail_chdir_root:
    {
        int ignored_chdir = chdir("/");
        (void)ignored_chdir;
    }
fail:
    if (old_fd >= 0) {
        close(old_fd);
    }
    if (victim_fd >= 0) {
        close(victim_fd);
    }
    rmdir(victim_path);
    rmdir(old_path);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

TEST(FuseExtended, OpsAccessCreateSymlinkLinkRename2FlushFsync) {
    ASSERT_EQ(0, ext_test_p2_ops());
}

TEST(FuseExtended, PositiveLookupCacheRespectsEntryTtl) {
    ASSERT_EQ(0, ext_test_positive_lookup_cache_respects_entry_ttl());
}

TEST(FuseExtended, XattrOps) {
    ASSERT_EQ(0, ext_test_xattr_ops());
}

TEST(FuseExtended, XattrEnosysIsCached) {
    ASSERT_EQ(0, ext_test_xattr_enosys_is_cached());
}

TEST(FuseExtended, InterruptDeliversFuseInterrupt) {
    ASSERT_EQ(0, ext_test_p3_interrupt());
}

TEST(FuseExtended, NoOpenNoOpendirReaddirplusNotify) {
    ASSERT_EQ(0, ext_test_p3_noopen_readdirplus_notify());
}

TEST(FuseExtended, OpenReturnsZeroFhIsValid) {
    ASSERT_EQ(0, ext_test_open_zero_fh_valid());
}

TEST(FuseExtended, LargeReadSplitsOverMaxWrite) {
    ASSERT_EQ(0, ext_test_large_read_over_max_write());
}

TEST(FuseExtended, CachedReadUsesOpenFhWithoutExtraOpen) {
    ASSERT_EQ(0, ext_test_cached_read_uses_open_fh_without_extra_open());
}

TEST(FuseExtended, CachedShortReadUpdatesEof) {
    ASSERT_EQ(0, ext_test_cached_short_read_updates_eof());
}

TEST(FuseExtended, ShortReadDiscardsOldPagesAfterRegrow) {
    ASSERT_EQ(0, ext_test_short_read_discards_old_pages_after_regrow());
}

TEST(FuseExtended, CachedReadSeesWriteThroughUpdate) {
    ASSERT_EQ(0, ext_test_cached_read_sees_write_through_update());
}

TEST(FuseExtended, MmapSeesWriteThroughUpdate) {
    ASSERT_EQ(0, ext_test_mmap_sees_write_through_update());
}

TEST(FuseExtended, MmapFaultUsesOpenFhWithoutExtraOpen) {
    ASSERT_EQ(0, ext_test_mmap_fault_uses_open_fh_without_extra_open());
}

TEST(FuseExtended, MmapFaultBatchesReadaroundPages) {
    ASSERT_EQ(0, ext_test_mmap_fault_batches_readaround_pages());
}

TEST(FuseExtended, DirectIoReadBypassesPageCache) {
    ASSERT_EQ(0, ext_test_direct_io_read_bypasses_page_cache());
}

TEST(FuseExtended, DirectIoWriteInvalidatesCachedRead) {
    ASSERT_EQ(0, ext_test_direct_io_write_invalidates_cached_read());
}

TEST(FuseExtended, DirectIoMmapPolicy) {
    ASSERT_EQ(0, ext_test_direct_io_mmap_policy());
}

TEST(FuseExtended, SharedWritableMmapMsyncWriteback) {
    ASSERT_EQ(0, ext_test_shared_writable_mmap_msync_writeback());
}

TEST(FuseExtended, SharedMmapDirtyThenPwriteKeepsLatestData) {
    ASSERT_EQ(0, ext_test_shared_mmap_dirty_then_pwrite_keeps_latest_data());
}

TEST(FuseExtended, SharedWritableMmapOSyncWriteback) {
    ASSERT_EQ(0, ext_test_shared_writable_mmap_osync_writeback());
}

TEST(FuseExtended, SharedMmapMprotectWriteback) {
    ASSERT_EQ(0, ext_test_shared_mmap_mprotect_writeback());
}

TEST(FuseExtended, SharedMmapReadonlyFdMprotectWriteDenied) {
    ASSERT_EQ(0, ext_test_shared_mmap_readonly_fd_mprotect_write_denied());
}

TEST(FuseExtended, SharedWritableMmapMunmapWritebackWithoutMsync) {
    ASSERT_EQ(0, ext_test_shared_writable_mmap_munmap_writeback_without_msync());
}

TEST(FuseExtended, SharedMmapSubrangeMprotectWritebackPreservesVma) {
    ASSERT_EQ(0, ext_test_shared_mmap_subrange_mprotect_writeback_preserves_vma());
}

TEST(FuseExtended, SharedMmapUnfaultedMprotectProtNone) {
    ASSERT_EQ(0, ext_test_shared_mmap_unfaulted_mprotect_prot_none());
}

TEST(FuseExtended, MmapTruncateUnmapsStalePage) {
    ASSERT_EQ(0, ext_test_mmap_truncate_unmaps_stale_page());
}

TEST(FuseExtended, FadviseWithoutPageCacheSucceeds) {
    ASSERT_EQ(0, ext_test_fadvise_without_page_cache());
}

TEST(FuseExtended, MountRamfsOnFuseDirectoryUsesNamespacePath) {
    ASSERT_EQ(0, ext_test_mount_on_fuse_dir_uses_namespace_path());
}

TEST(FuseExtended, LookupNodesForgottenBeforeUmountWhenUnreferenced) {
    ASSERT_EQ(0, ext_test_lookup_nodes_forgotten_before_umount_when_unreferenced());
}

TEST(FuseExtended, PositiveLookupCacheExpiresAndForgetsBeforeUmount) {
    ASSERT_EQ(0, ext_test_positive_lookup_cache_expires_and_forgets_before_umount());
}

TEST(FuseExtended, ActiveDirectoryParentSurvivesLookupCachePrune) {
    ASSERT_EQ(0, ext_test_active_directory_parent_survives_lookup_cache_prune());
}

TEST(FuseExtended, LookupSelfAliasRejectedAndForgotten) {
    ASSERT_EQ(0, ext_test_lookup_self_alias_rejected_and_forgotten());
}

TEST(FuseExtended, RenameUpdatesFuseDirectoryCwdPath) {
    ASSERT_EQ(0, ext_test_rename_updates_fuse_dir_cwd_path());
}

TEST(FuseExtended, ReaddirplusGenerationMismatchStalesOldNode) {
    ASSERT_EQ(0, ext_test_readdirplus_generation_mismatch_stales_old_node());
}

TEST(FuseExtended, ReaddirplusInvalidAttrForgetsUnconsumedEntry) {
    ASSERT_EQ(0, ext_test_readdirplus_invalid_attr_forgets_unconsumed_entry());
}

TEST(FuseExtended, SameGenerationTypeMismatchStalesOldNode) {
    ASSERT_EQ(0, ext_test_same_generation_type_mismatch_stales_old_node());
}

TEST(FuseExtended, CreateGenerationMismatchStalesOldNode) {
    ASSERT_EQ(0, ext_test_create_generation_mismatch_stales_old_node());
}

TEST(FuseExtended, LinkGenerationMismatchStalesOldNode) {
    ASSERT_EQ(0, ext_test_link_generation_mismatch_stales_old_node());
}

TEST(FuseExtended, RenameReplaceClearsOldTargetPath) {
    ASSERT_EQ(0, ext_test_rename_replace_clears_old_target_path());
}

TEST(FuseExtended, NoOpenFsyncUsesZeroFh) {
    ASSERT_EQ(0, ext_test_noopen_fsync_uses_zero_fh());
}

TEST(FuseExtended, FsyncEnosysCachedSuccess) {
    ASSERT_EQ(0, ext_test_fsync_enosys_cached_success());
}

TEST(FuseExtended, OpenFlagsMatchLinuxMask) {
    ASSERT_EQ(0, ext_test_open_release_flags_match_linux());
}

TEST(FuseExtended, CreateReusesFuseHandleWithoutOpen) {
    ASSERT_EQ(0, ext_test_create_reuses_fuse_handle());
}

TEST(FuseExtended, CreateEnosysFallsBackAndCaches) {
    ASSERT_EQ(0, ext_test_create_enosys_falls_back_and_caches());
}

TEST(FuseExtended, InvalidCreateReplyCleansResources) {
    ASSERT_EQ(0, ext_test_invalid_create_reply_cleans_resources());
}

TEST(FuseExtended, FsetflUpdatesFuseIoFlags) {
    ASSERT_EQ(0, ext_test_fsetfl_updates_fuse_io_flags());
}

TEST(FuseExtended, FsetflUpdatesFuseDevNonblock) {
    ASSERT_EQ(0, ext_test_fsetfl_updates_fuse_dev_nonblock());
}

TEST(FuseExtended, FopenNoFlushSkipsFlush) {
    ASSERT_EQ(0, ext_test_fopen_noflush_skips_flush());
}

TEST(FuseExtended, CloseReturnsFlushErrorAndClosesFd) {
    ASSERT_EQ(0, ext_test_close_returns_flush_error_and_closes_fd());
}

TEST(FuseExtended, FlushEnosysCachedSuccess) {
    ASSERT_EQ(0, ext_test_flush_enosys_cached_success());
}

TEST(FuseExtended, FopenNonseekableDisablesRandomIo) {
    ASSERT_EQ(0,
              ext_test_fopen_nonseekable_mode(FOPEN_NONSEEKABLE, "/tmp/test_fuse_nonseek", 0));
}

TEST(FuseExtended, FopenStreamDisablesRandomIo) {
    ASSERT_EQ(0, ext_test_fopen_nonseekable_mode(FOPEN_STREAM, "/tmp/test_fuse_stream", 1));
}

TEST(FuseExtended, FopenNonseekableDirectoryDisablesLseek) {
    ASSERT_EQ(0,
              ext_test_fopen_nonseekable_dir_mode(FOPEN_NONSEEKABLE, "/tmp/test_fuse_dir_nonseek"));
}

TEST(FuseExtended, AtomicOTruncUsesOpenWithoutSetattr) {
    ASSERT_EQ(0, ext_test_atomic_otrunc_uses_open_without_setattr());
}

TEST(FuseExtended, FtruncateSetattrUsesOpenFh) {
    ASSERT_EQ(0, ext_test_ftruncate_setattr_uses_open_fh());
}

TEST(FuseExtended, InitRequestsLinuxNoOpenSupport) {
    ASSERT_EQ(0, ext_test_init_requests_linux_no_open_support());
}

TEST(FuseExtended, SubtypeMountFuseDotSubtype) {
    ASSERT_EQ(0, ext_test_p4_subtype_mount());
}

TEST(FuseExtended, PermissionModelAllowOtherDefaultPermissions) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires root to execute setuid/setgid permission cases";
    }
    ASSERT_EQ(0, ext_test_permissions());
}

TEST(FuseExtended, DevCloneAttachAndServe) {
    ASSERT_EQ(0, ext_test_clone());
}

TEST(FuseExtended, CachedReadPipelinesRequests) {
    ASSERT_EQ(0, ext_test_cached_read_pipelines_requests());
}

TEST(FuseExtended, CachedReadWithoutAsyncIsSerial) {
    ASSERT_EQ(0, ext_test_cached_read_without_async_is_serial());
}

TEST(FuseExtended, CachedReadSyncErrorSemantics) {
    ASSERT_EQ(0, ext_test_cached_read_sync_error_semantics());
}

int main(int argc, char **argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
