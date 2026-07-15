#include <gtest/gtest.h>

#include <sched.h>
#include <sys/wait.h>

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

static int core_test_recursive_bind_cross_fs_mountpoint_collision() {
    const char *base = "/tmp/test_fuse_recursive_bind_collision";
    const char *source = "/tmp/test_fuse_recursive_bind_collision/source";
    const char *source_a = "/tmp/test_fuse_recursive_bind_collision/source/a";
    const char *source_b = "/tmp/test_fuse_recursive_bind_collision/source/b";
    const char *source_a_file = "/tmp/test_fuse_recursive_bind_collision/source/a/hello.txt";
    const char *source_b_file = "/tmp/test_fuse_recursive_bind_collision/source/b/hello.txt";
    const char *dest = "/tmp/test_fuse_recursive_bind_collision/dest";
    const char *dest_a = "/tmp/test_fuse_recursive_bind_collision/dest/a";
    const char *dest_b = "/tmp/test_fuse_recursive_bind_collision/dest/b";
    const char *dest_a_file = "/tmp/test_fuse_recursive_bind_collision/dest/a/hello.txt";
    const char *dest_b_file = "/tmp/test_fuse_recursive_bind_collision/dest/b/hello.txt";
    const char *payload_a = "/tmp/test_fuse_recursive_bind_collision/payload_a";
    const char *payload_b = "/tmp/test_fuse_recursive_bind_collision/payload_b";
    int fds[2] = {-1, -1};
    volatile int stops[2] = {0, 0};
    volatile int init_done[2] = {0, 0};
    struct fuse_daemon_args args[2];
    pthread_t threads[2] = {};
    bool thread_started[2] = {false, false};
    bool passed = false;

    memset(args, 0, sizeof(args));
    do {
        if (unshare(CLONE_NEWNS) != 0 ||
            mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr) != 0) {
            printf("[FAIL] isolate mount namespace: %s (errno=%d)\n", strerror(errno), errno);
            break;
        }
        if (ensure_dir(base) != 0 || ensure_dir(source) != 0 || ensure_dir(dest) != 0 ||
            mount("none", source, "ramfs", 0, nullptr) != 0 || ensure_dir(source_a) != 0 ||
            ensure_dir(source_b) != 0 || fuseg_write_file(payload_a, "A") != 0 ||
            fuseg_write_file(payload_b, "B") != 0) {
            printf("[FAIL] prepare collision topology: %s (errno=%d)\n", strerror(errno), errno);
            break;
        }

        for (int i = 0; i < 2; ++i) {
            fds[i] = open("/dev/fuse", O_RDWR);
            if (fds[i] < 0) {
                printf("[FAIL] open(/dev/fuse)[%d]: %s (errno=%d)\n", i, strerror(errno), errno);
                break;
            }
            args[i].fd = fds[i];
            args[i].stop = &stops[i];
            args[i].init_done = &init_done[i];
            if (pthread_create(&threads[i], nullptr, fuse_daemon_thread, &args[i]) != 0) {
                printf("[FAIL] pthread_create[%d]\n", i);
                break;
            }
            thread_started[i] = true;
        }
        if (!thread_started[0] || !thread_started[1]) {
            break;
        }

        char opts_a[256];
        char opts_b[256];
        snprintf(opts_a, sizeof(opts_a), "fd=%d,rootmode=040755,user_id=0,group_id=0", fds[0]);
        snprintf(opts_b, sizeof(opts_b), "fd=%d,rootmode=040755,user_id=0,group_id=0", fds[1]);
        if (mount("none", source_a, "fuse", 0, opts_a) != 0 ||
            mount("none", source_b, "fuse", 0, opts_b) != 0 ||
            fuseg_wait_init(&init_done[0]) != 0 || fuseg_wait_init(&init_done[1]) != 0) {
            printf("[FAIL] mount/init FUSE pair: %s (errno=%d)\n", strerror(errno), errno);
            break;
        }

        struct stat fuse_a = {};
        struct stat fuse_b = {};
        if (stat(source_a_file, &fuse_a) != 0 || stat(source_b_file, &fuse_b) != 0 ||
            fuse_a.st_ino != fuse_b.st_ino || fuse_a.st_ino != 2 || fuse_a.st_dev == fuse_b.st_dev) {
            printf("[FAIL] expected cross-filesystem FUSE mountpoint collision: "
                   "a=(dev=%llu,ino=%llu) b=(dev=%llu,ino=%llu) errno=%d\n",
                   (unsigned long long)fuse_a.st_dev, (unsigned long long)fuse_a.st_ino,
                   (unsigned long long)fuse_b.st_dev, (unsigned long long)fuse_b.st_ino, errno);
            break;
        }

        if (mount(payload_a, source_a_file, nullptr, MS_BIND, nullptr) != 0 ||
            mount(payload_b, source_b_file, nullptr, MS_BIND, nullptr) != 0 ||
            mount(source, dest, nullptr, MS_BIND | MS_REC, nullptr) != 0) {
            printf("[FAIL] construct/clone nested collision edges: %s (errno=%d)\n",
                   strerror(errno), errno);
            break;
        }

        char content_a[8] = {};
        char content_b[8] = {};
        struct stat source_nested_a = {};
        struct stat source_nested_b = {};
        struct stat dest_nested_a = {};
        struct stat dest_nested_b = {};
        const int len_a = fuseg_read_file(dest_a_file, content_a, sizeof(content_a));
        const int len_b = fuseg_read_file(dest_b_file, content_b, sizeof(content_b));
        if (len_a != 1 || len_b != 1 || content_a[0] != 'A' || content_b[0] != 'B' ||
            stat(source_a_file, &source_nested_a) != 0 ||
            stat(source_b_file, &source_nested_b) != 0 || stat(dest_a_file, &dest_nested_a) != 0 ||
            stat(dest_b_file, &dest_nested_b) != 0 ||
            source_nested_a.st_dev != dest_nested_a.st_dev ||
            source_nested_a.st_ino != dest_nested_a.st_ino ||
            source_nested_b.st_dev != dest_nested_b.st_dev ||
            source_nested_b.st_ino != dest_nested_b.st_ino ||
            source_nested_a.st_ino == source_nested_b.st_ino) {
            printf("[FAIL] recursive bind lost or swapped colliding nested edges\n");
            break;
        }
        passed = true;
    } while (false);

    // Drop every clone before its source FUSE connection. MNT_DETACH keeps
    // cleanup bounded even when the assertion path stopped mid-construction.
    umount2(dest_a_file, MNT_DETACH);
    umount2(dest_b_file, MNT_DETACH);
    umount2(dest_a, MNT_DETACH);
    umount2(dest_b, MNT_DETACH);
    umount2(dest, MNT_DETACH);
    umount2(source_a_file, MNT_DETACH);
    umount2(source_b_file, MNT_DETACH);
    umount2(source_a, MNT_DETACH);
    umount2(source_b, MNT_DETACH);
    umount2(source, MNT_DETACH);

    for (int i = 0; i < 2; ++i) {
        stops[i] = 1;
        if (fds[i] >= 0) {
            close(fds[i]);
        }
        if (thread_started[i]) {
            pthread_join(threads[i], nullptr);
        }
    }
    unlink(payload_a);
    unlink(payload_b);
    rmdir(dest);
    rmdir(source);
    rmdir(base);
    return passed ? 0 : -1;
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

TEST(FuseCore, RecursiveBindUsesCrossFilesystemMountpointIdentity) {
    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        _exit(core_test_recursive_bind_cross_fs_mountpoint_collision() == 0 ? 0 : 1);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

int main(int argc, char **argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
