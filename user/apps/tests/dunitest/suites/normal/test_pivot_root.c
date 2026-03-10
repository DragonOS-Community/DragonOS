#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

#ifndef SYS_pivot_root
#ifdef __NR_pivot_root
#define SYS_pivot_root __NR_pivot_root
#elif defined(__x86_64__)
#define SYS_pivot_root 155
#else
#define SYS_pivot_root 41
#endif
#endif

#ifndef CLONE_NEWNS
#define CLONE_NEWNS 0x00020000
#endif

#ifndef MS_REC
#define MS_REC 16384
#endif

static int tests_passed = 0;
static int tests_failed = 0;

#define TEST_PASS(name)                                                        \
    do {                                                                       \
        printf("[PASS] %s\n", name);                                           \
        tests_passed++;                                                        \
    } while (0)

#define TEST_FAIL(name, reason)                                                \
    do {                                                                       \
        printf("[FAIL] %s: %s\n", name, reason);                               \
        tests_failed++;                                                        \
    } while (0)

#define TEST_SKIP(name, reason)                                                \
    do {                                                                       \
        printf("[SKIP] %s: %s\n", name, reason);                               \
    } while (0)

static int ensure_dir(const char *path) {
    struct stat st;
    if (stat(path, &st) == 0) {
        return S_ISDIR(st.st_mode) ? 0 : -1;
    }
    return mkdir(path, 0755);
}

static void ensure_parent_tree(void) {
    ensure_dir("/tmp");
    ensure_dir("/tmp/test_pivot_root");
}

static void cleanup_mount(const char *path) {
    umount(path);
    rmdir(path);
}

static long do_pivot_root(const char *new_root, const char *put_old) {
    return syscall(SYS_pivot_root, new_root, put_old);
}

static void test_success_path(void) {
    const char *name = "pivot_root_success";
    const char *base = "/tmp/test_pivot_root/success";
    const char *new_root = "/tmp/test_pivot_root/success/newroot";
    const char *put_old = "oldroot";
    const char *oldroot_abs = "/tmp/test_pivot_root/success/newroot/oldroot";
    const char *bin_dir = "/tmp/test_pivot_root/success/newroot/bin";
    char cwd[256];

    ensure_parent_tree();
    ensure_dir(base);
    ensure_dir(new_root);

    if (unshare(CLONE_NEWNS) != 0) {
        TEST_SKIP(name, strerror(errno));
        return;
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    if (mount("", new_root, "ramfs", 0, NULL) != 0) {
        TEST_SKIP(name, strerror(errno));
        return;
    }

    if (mkdir(oldroot_abs, 0755) != 0 && errno != EEXIST) {
        cleanup_mount(new_root);
        TEST_FAIL(name, "mkdir(oldroot) failed");
        return;
    }

    if (mkdir(bin_dir, 0755) != 0 && errno != EEXIST) {
        cleanup_mount(new_root);
        TEST_FAIL(name, "mkdir(bin) failed");
        return;
    }

    if (chdir(new_root) != 0) {
        cleanup_mount(new_root);
        TEST_FAIL(name, "chdir(new_root) failed");
        return;
    }

    if (do_pivot_root(".", put_old) != 0) {
        cleanup_mount(new_root);
        TEST_FAIL(name, strerror(errno));
        return;
    }

    if (getcwd(cwd, sizeof(cwd)) == NULL) {
        TEST_FAIL(name, "getcwd failed after pivot");
        return;
    }

    if (strcmp(cwd, "/") != 0) {
        TEST_FAIL(name, "cwd is not / after pivot");
        return;
    }

    if (access("/oldroot", F_OK) != 0) {
        TEST_FAIL(name, "old root not reachable under /oldroot");
        return;
    }

    if (access("/bin", F_OK) != 0) {
        TEST_FAIL(name, "new root is not visible via absolute path");
        return;
    }

    TEST_PASS(name);
}

static void test_dot_dot_path(void) {
    const char *name = "pivot_root_dot_dot";
    const char *base = "/tmp/test_pivot_root/dotdot";
    const char *new_root = "/tmp/test_pivot_root/dotdot/newroot";
    const char *bin_dir = "/tmp/test_pivot_root/dotdot/newroot/bin";
    int oldroot_fd;
    int newroot_fd;
    char cwd[256];

    ensure_parent_tree();
    ensure_dir(base);
    ensure_dir(new_root);

    if (unshare(CLONE_NEWNS) != 0) {
        TEST_SKIP(name, strerror(errno));
        return;
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    if (mount("", new_root, "ramfs", 0, NULL) != 0) {
        TEST_SKIP(name, strerror(errno));
        return;
    }

    if (mkdir(bin_dir, 0755) != 0 && errno != EEXIST) {
        cleanup_mount(new_root);
        TEST_FAIL(name, "mkdir(bin) failed");
        return;
    }

    oldroot_fd = open("/", O_DIRECTORY | O_RDONLY);
    if (oldroot_fd < 0) {
        cleanup_mount(new_root);
        TEST_FAIL(name, "open(oldroot) failed");
        return;
    }

    newroot_fd = open(new_root, O_DIRECTORY | O_RDONLY);
    if (newroot_fd < 0) {
        close(oldroot_fd);
        cleanup_mount(new_root);
        TEST_FAIL(name, "open(newroot) failed");
        return;
    }

    if (fchdir(newroot_fd) != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        cleanup_mount(new_root);
        TEST_FAIL(name, "fchdir(newroot) failed");
        return;
    }

    if (do_pivot_root(".", ".") != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        cleanup_mount(new_root);
        TEST_FAIL(name, strerror(errno));
        return;
    }

    if (fchdir(oldroot_fd) != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        TEST_FAIL(name, "fchdir(oldroot) failed after pivot");
        return;
    }

    if (umount2(".", MNT_DETACH) != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        TEST_FAIL(name, "umount2(oldroot) failed");
        return;
    }

    if (chdir("/") != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        TEST_FAIL(name, "chdir(/) failed after detach");
        return;
    }

    if (getcwd(cwd, sizeof(cwd)) == NULL || strcmp(cwd, "/") != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        TEST_FAIL(name, "cwd is not / after dot-dot pivot");
        return;
    }

    if (access("/bin", F_OK) != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        TEST_FAIL(name, "new root is not visible after dot-dot pivot");
        return;
    }

    close(newroot_fd);
    close(oldroot_fd);
    TEST_PASS(name);
}

static void test_dot_dot_rslave_detach(void) {
    const char *name = "pivot_root_dot_dot_rslave_detach";
    const char *base = "/tmp/test_pivot_root/dotdot_rslave";
    const char *new_root = "/tmp/test_pivot_root/dotdot_rslave/newroot";
    const char *bin_dir = "/tmp/test_pivot_root/dotdot_rslave/newroot/bin";
    int oldroot_fd;
    int newroot_fd;

    ensure_parent_tree();
    ensure_dir(base);
    ensure_dir(new_root);

    if (unshare(CLONE_NEWNS) != 0) {
        TEST_SKIP(name, strerror(errno));
        return;
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    if (mount("", new_root, "ramfs", 0, NULL) != 0) {
        TEST_SKIP(name, strerror(errno));
        return;
    }

    if (mkdir(bin_dir, 0755) != 0 && errno != EEXIST) {
        cleanup_mount(new_root);
        TEST_FAIL(name, "mkdir(bin) failed");
        return;
    }

    oldroot_fd = open("/", O_DIRECTORY | O_RDONLY);
    if (oldroot_fd < 0) {
        cleanup_mount(new_root);
        TEST_FAIL(name, "open(oldroot) failed");
        return;
    }

    newroot_fd = open(new_root, O_DIRECTORY | O_RDONLY);
    if (newroot_fd < 0) {
        close(oldroot_fd);
        cleanup_mount(new_root);
        TEST_FAIL(name, "open(newroot) failed");
        return;
    }

    if (fchdir(newroot_fd) != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        cleanup_mount(new_root);
        TEST_FAIL(name, "fchdir(newroot) failed");
        return;
    }

    if (do_pivot_root(".", ".") != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        cleanup_mount(new_root);
        TEST_FAIL(name, strerror(errno));
        return;
    }

    if (fchdir(oldroot_fd) != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        TEST_FAIL(name, "fchdir(oldroot) failed after pivot");
        return;
    }

    if (mount(NULL, ".", NULL, MS_REC | MS_SLAVE, NULL) != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        TEST_FAIL(name, "mount(make-rslave) failed");
        return;
    }

    if (umount2(".", MNT_DETACH) != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        TEST_FAIL(name, "umount2(oldroot) failed after make-rslave");
        return;
    }

    close(newroot_fd);
    close(oldroot_fd);
    TEST_PASS(name);
}

static void test_new_root_not_mountpoint(void) {
    const char *name = "pivot_root_new_root_not_mountpoint";
    const char *base = "/tmp/test_pivot_root/not_mountpoint";
    const char *new_root = "/tmp/test_pivot_root/not_mountpoint/newroot";
    const char *oldroot_abs = "/tmp/test_pivot_root/not_mountpoint/newroot/oldroot";

    ensure_parent_tree();
    ensure_dir(base);
    ensure_dir(new_root);
    ensure_dir(oldroot_abs);

    if (unshare(CLONE_NEWNS) != 0) {
        TEST_SKIP(name, strerror(errno));
        return;
    }

    if (do_pivot_root(new_root, oldroot_abs) == -1 && errno == EINVAL) {
        TEST_PASS(name);
    } else {
        TEST_FAIL(name, "expected EINVAL");
    }
}

static void test_put_old_outside_new_root(void) {
    const char *name = "pivot_root_put_old_outside_new_root";
    const char *base = "/tmp/test_pivot_root/put_old_outside";
    const char *new_root = "/tmp/test_pivot_root/put_old_outside/newroot";
    const char *outside = "/tmp/test_pivot_root/put_old_outside/outside";
    const char *inside = "/tmp/test_pivot_root/put_old_outside/newroot/inside";

    ensure_parent_tree();
    ensure_dir(base);
    ensure_dir(new_root);
    ensure_dir(outside);

    if (unshare(CLONE_NEWNS) != 0) {
        TEST_SKIP(name, strerror(errno));
        return;
    }

    if (mount("", new_root, "ramfs", 0, NULL) != 0) {
        TEST_SKIP(name, strerror(errno));
        return;
    }

    if (mkdir(inside, 0755) != 0 && errno != EEXIST) {
        cleanup_mount(new_root);
        TEST_FAIL(name, "mkdir(inside) failed");
        return;
    }

    if (do_pivot_root(new_root, outside) == -1 && errno == EINVAL) {
        cleanup_mount(new_root);
        TEST_PASS(name);
    } else {
        cleanup_mount(new_root);
        TEST_FAIL(name, "expected EINVAL");
    }
}

static void test_busy_target(void) {
    const char *name = "pivot_root_busy_target";
    const char *base = "/tmp/test_pivot_root/busy";
    const char *new_root = "/tmp/test_pivot_root/busy/newroot";
    const char *oldroot_abs = "/tmp/test_pivot_root/busy/newroot/oldroot";

    ensure_parent_tree();
    ensure_dir(base);
    ensure_dir(new_root);

    if (unshare(CLONE_NEWNS) != 0) {
        TEST_SKIP(name, strerror(errno));
        return;
    }

    if (mount("", new_root, "ramfs", 0, NULL) != 0) {
        TEST_SKIP(name, strerror(errno));
        return;
    }

    if (mkdir(oldroot_abs, 0755) != 0 && errno != EEXIST) {
        cleanup_mount(new_root);
        TEST_FAIL(name, "mkdir(oldroot) failed");
        return;
    }

    if (do_pivot_root(new_root, "/") == -1 && errno == EBUSY) {
        cleanup_mount(new_root);
        TEST_PASS(name);
    } else {
        cleanup_mount(new_root);
        TEST_FAIL(name, "expected EBUSY");
    }
}

static void test_permission_failure(void) {
    const char *name = "pivot_root_permission_failure";
    const char *base = "/tmp/test_pivot_root/perm";
    const char *new_root = "/tmp/test_pivot_root/perm/newroot";
    const char *oldroot_abs = "/tmp/test_pivot_root/perm/newroot/oldroot";

    ensure_parent_tree();
    ensure_dir(base);
    ensure_dir(new_root);

    if (mount("", new_root, "ramfs", 0, NULL) != 0) {
        TEST_SKIP(name, strerror(errno));
        return;
    }

    if (mkdir(oldroot_abs, 0755) != 0 && errno != EEXIST) {
        cleanup_mount(new_root);
        TEST_FAIL(name, "mkdir(oldroot) failed");
        return;
    }

    if (seteuid(65534) != 0) {
        cleanup_mount(new_root);
        TEST_SKIP(name, "seteuid failed");
        return;
    }

    if (do_pivot_root(new_root, oldroot_abs) == -1 && errno == EPERM) {
        TEST_PASS(name);
    } else {
        TEST_FAIL(name, "expected EPERM");
    }
}

int main(void) {
    test_success_path();
    test_dot_dot_path();
    test_dot_dot_rslave_detach();
    test_new_root_not_mountpoint();
    test_put_old_outside_new_root();
    test_busy_target();
    test_permission_failure();

    printf("passed=%d failed=%d\n", tests_passed, tests_failed);
    return tests_failed == 0 ? 0 : 1;
}
