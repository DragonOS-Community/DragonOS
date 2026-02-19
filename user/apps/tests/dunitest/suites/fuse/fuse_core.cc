#include <gtest/gtest.h>

#include "fuse_gtest_common.h"

static int core_test_nonblock_read_empty() {
    int fd = open("/dev/fuse", O_RDWR | O_NONBLOCK);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }

    unsigned char small[FUSE_MIN_READ_BUFFER / 2];
    ssize_t n = read(fd, small, sizeof(small));
    if (n != -1 || errno != EINVAL) {
        printf("[FAIL] nonblock read with small buffer: n=%zd errno=%d (%s)\n", n, errno,
               strerror(errno));
        close(fd);
        return -1;
    }

    unsigned char *big = (unsigned char *)malloc(FUSE_TEST_BUF_SIZE);
    if (!big) {
        printf("[FAIL] malloc big buffer failed\n");
        close(fd);
        return -1;
    }
    memset(big, 0, FUSE_TEST_BUF_SIZE);

    n = read(fd, big, FUSE_TEST_BUF_SIZE);
    if (n != -1 || (errno != EAGAIN && errno != EWOULDBLOCK)) {
        printf("[FAIL] nonblock read empty: n=%zd errno=%d (%s)\n", n, errno, strerror(errno));
        free(big);
        close(fd);
        return -1;
    }

    struct pollfd pfd;
    pfd.fd = fd;
    pfd.events = POLLIN;
    int pr = poll(&pfd, 1, 100);
    if (pr != 0) {
        printf("[FAIL] poll empty expected timeout: pr=%d revents=%x errno=%d (%s)\n", pr,
               pfd.revents, errno, strerror(errno));
        free(big);
        close(fd);
        return -1;
    }

    free(big);
    close(fd);
    return 0;
}

static int core_test_mount_init_single_use_fd() {
    const char *mp = "/tmp/test_fuse_mp";
    const char *mp2 = "/tmp/test_fuse_mp2";

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }
    if (ensure_dir(mp2) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp2, strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR | O_NONBLOCK);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        rmdir(mp2);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);

    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        rmdir(mp);
        rmdir(mp2);
        return -1;
    }

    if (fuseg_do_init_handshake_basic(fd) != 0) {
        printf("[FAIL] init handshake: %s (errno=%d)\n", strerror(errno), errno);
        umount(mp);
        close(fd);
        rmdir(mp);
        rmdir(mp2);
        return -1;
    }

    unsigned char *tmp = (unsigned char *)malloc(FUSE_TEST_BUF_SIZE);
    if (!tmp) {
        printf("[FAIL] malloc tmp buffer failed\n");
        umount(mp);
        close(fd);
        rmdir(mp);
        rmdir(mp2);
        return -1;
    }
    memset(tmp, 0, FUSE_TEST_BUF_SIZE);

    ssize_t rn = read(fd, tmp, FUSE_TEST_BUF_SIZE);
    if (rn != -1 || (errno != EAGAIN && errno != EWOULDBLOCK)) {
        printf("[FAIL] expected EAGAIN after init: rn=%zd errno=%d (%s)\n", rn, errno,
               strerror(errno));
        free(tmp);
        umount(mp);
        close(fd);
        rmdir(mp);
        rmdir(mp2);
        return -1;
    }
    free(tmp);

    if (mount("none", mp2, "fuse", 0, opts) == 0) {
        printf("[FAIL] second mount with same fd unexpectedly succeeded\n");
        umount(mp);
        umount(mp2);
        close(fd);
        rmdir(mp);
        rmdir(mp2);
        return -1;
    }
    if (errno != EINVAL) {
        printf("[FAIL] second mount expected EINVAL got errno=%d (%s)\n", errno, strerror(errno));
        umount(mp);
        close(fd);
        rmdir(mp);
        rmdir(mp2);
        return -1;
    }

    umount(mp);
    rmdir(mp);
    rmdir(mp2);
    close(fd);
    return 0;
}

static int core_test_phase_c_read_path() {
    const char *mp = "/tmp/test_fuse_c";
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

    for (int i = 0; i < 100; i++) {
        if (init_done)
            break;
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

    DIR *d = opendir(mp);
    if (!d) {
        printf("[FAIL] opendir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    int found = 0;
    struct dirent *de;
    while ((de = readdir(d)) != NULL) {
        if (strcmp(de->d_name, "hello.txt") == 0) {
            found = 1;
            break;
        }
    }
    closedir(d);
    if (!found) {
        printf("[FAIL] readdir: hello.txt not found\n");
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    char p[256];
    snprintf(p, sizeof(p), "%s/hello.txt", mp);

    struct stat st;
    if (stat(p, &st) != 0) {
        printf("[FAIL] stat(%s): %s (errno=%d)\n", p, strerror(errno), errno);
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (!S_ISREG(st.st_mode)) {
        printf("[FAIL] stat: expected regular file\n");
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    char buf[128];
    int n = fuseg_read_file(p, buf, sizeof(buf) - 1);
    if (n < 0) {
        printf("[FAIL] read(%s): %s (errno=%d)\n", p, strerror(errno), errno);
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    buf[n] = '\0';
    if (strcmp(buf, "hello from fuse\n") != 0) {
        printf("[FAIL] content mismatch: got='%s'\n", buf);
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    // 回归覆盖：重复 open/read/close，若 close 路径存在锁反转，容易在这里卡住。
    for (int i = 0; i < 32; i++) {
        n = fuseg_read_file(p, buf, sizeof(buf) - 1);
        if (n < 0) {
            printf("[FAIL] repeated read(%s) iter=%d: %s (errno=%d)\n", p, i, strerror(errno),
                   errno);
            umount(mp);
            stop = 1;
            close(fd);
            pthread_join(th, NULL);
            rmdir(mp);
            return -1;
        }
        buf[n] = '\0';
        if (strcmp(buf, "hello from fuse\n") != 0) {
            printf("[FAIL] repeated content mismatch iter=%d got='%s'\n", i, buf);
            umount(mp);
            stop = 1;
            close(fd);
            pthread_join(th, NULL);
            rmdir(mp);
            return -1;
        }
    }

    umount(mp);
    rmdir(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    return 0;
}

static int core_test_phase_d_write_path() {
    const char *mp = "/tmp/test_fuse_d";
    int f = -1;
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
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;

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

    for (int i = 0; i < 100; i++) {
        if (init_done)
            break;
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

    char p1[256];
    snprintf(p1, sizeof(p1), "%s/new.txt", mp);
    if (fuseg_write_file(p1, "abcdef") != 0) {
        printf("[FAIL] write_all(%s): %s (errno=%d)\n", p1, strerror(errno), errno);
        goto fail;
    }

    f = open(p1, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open for truncate: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (ftruncate(f, 3) != 0) {
        printf("[FAIL] ftruncate: %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    close(f);

    char buf[64];
    n = fuseg_read_file(p1, buf, sizeof(buf) - 1);
    if (n < 0) {
        printf("[FAIL] read_all after truncate: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    buf[n] = '\0';
    if (strcmp(buf, "abc") != 0) {
        printf("[FAIL] truncate content mismatch got='%s'\n", buf);
        goto fail;
    }

    char p2[256];
    snprintf(p2, sizeof(p2), "%s/renamed.txt", mp);
    if (rename(p1, p2) != 0) {
        printf("[FAIL] rename: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    if (unlink(p2) != 0) {
        printf("[FAIL] unlink: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    char d1[256];
    snprintf(d1, sizeof(d1), "%s/dir", mp);
    if (mkdir(d1, 0755) != 0) {
        printf("[FAIL] mkdir: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (rmdir(d1) != 0) {
        printf("[FAIL] rmdir: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    umount(mp);
    rmdir(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    return 0;

fail:
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int core_test_lifecycle_forget_destroy() {
    const char *mp = "/tmp/test_fuse_p1_lifecycle";
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
    volatile uint32_t destroy_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 0;
    args.stop_on_destroy = 1;
    args.forget_count = &forget_count;
    args.forget_nlookup_sum = &forget_nlookup_sum;
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
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    char p[256];
    snprintf(p, sizeof(p), "%s/hello.txt", mp);
    for (int i = 0; i < 8; i++) {
        struct stat st;
        if (stat(p, &st) != 0) {
            printf("[FAIL] stat(%s): %s (errno=%d)\n", p, strerror(errno), errno);
            umount(mp);
            stop = 1;
            close(fd);
            pthread_join(th, NULL);
            rmdir(mp);
            return -1;
        }
    }

    usleep(100 * 1000);

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    for (int i = 0; i < 100; i++) {
        if (destroy_count > 0)
            break;
        usleep(10 * 1000);
    }

    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);

    if (forget_count == 0 || forget_nlookup_sum == 0) {
        printf("[FAIL] expected FORGET requests, got count=%u nlookup_sum=%llu\n", forget_count,
               (unsigned long long)forget_nlookup_sum);
        return -1;
    }

    if (destroy_count == 0) {
        printf("[FAIL] expected DESTROY request on umount\n");
        return -1;
    }

    return 0;
}

TEST(FuseCore, DevNonblockReadEmpty) {
    ASSERT_EQ(0, core_test_nonblock_read_empty());
}

TEST(FuseCore, MountInitAndSingleUseFd) {
    ASSERT_EQ(0, core_test_mount_init_single_use_fd());
}

TEST(FuseCore, ReadPathLookupGetattrReaddirOpenRead) {
    ASSERT_EQ(0, core_test_phase_c_read_path());
}

TEST(FuseCore, WritePathCreateTruncateRenameUnlinkMkdirRmdir) {
    ASSERT_EQ(0, core_test_phase_d_write_path());
}

TEST(FuseCore, LifecycleForgetAndDestroy) {
    ASSERT_EQ(0, core_test_lifecycle_forget_destroy());
}

int main(int argc, char **argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
