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
#include <sys/types.h>
#include <unistd.h>

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

static void cleanup_mount(const char *path) {
    umount(path);
    rmdir(path);
}

static bool mount_has_option(const char *mountpoint, const char *option) {
    FILE *fp;
    char *line = NULL;
    size_t cap = 0;
    bool found = false;

    fp = fopen("/proc/self/mounts", "r");
    if (fp == NULL) {
        return false;
    }

    while (getline(&line, &cap, fp) > 0) {
        char *saveptr = NULL;
        char *field = NULL;
        char *current_mountpoint;
        char *options;
        char *opt_saveptr = NULL;

        field = strtok_r(line, " ", &saveptr);
        if (field == NULL) {
            continue;
        }

        current_mountpoint = strtok_r(NULL, " ", &saveptr);
        if (current_mountpoint == NULL || strcmp(current_mountpoint, mountpoint) != 0) {
            continue;
        }

        strtok_r(NULL, " ", &saveptr);
        options = strtok_r(NULL, " ", &saveptr);
        if (options == NULL) {
            continue;
        }

        for (field = strtok_r(options, ",", &opt_saveptr); field != NULL;
             field = strtok_r(NULL, ",", &opt_saveptr)) {
            if (strcmp(field, option) == 0) {
                found = true;
                break;
            }
        }

        break;
    }

    free(line);
    fclose(fp);
    return found;
}

static void test_bind_remount_readonly(void) {
    const char *name = "bind_remount_readonly";
    const char *base = "/tmp/test_mount_reconfigure";
    const char *source = "/tmp/test_mount_reconfigure/source";
    const char *target = "/tmp/test_mount_reconfigure/target";
    char src_file[256];
    char dst_file[256];
    int fd;

    ensure_dir("/tmp");
    ensure_dir(base);
    ensure_dir(source);
    ensure_dir(target);

    if (unshare(CLONE_NEWNS) != 0) {
        TEST_SKIP(name, strerror(errno));
        return;
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    if (mount("", source, "ramfs", 0, NULL) != 0) {
        TEST_SKIP(name, strerror(errno));
        return;
    }

    snprintf(src_file, sizeof(src_file), "%s/source.txt", source);
    fd = open(src_file, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        cleanup_mount(source);
        TEST_FAIL(name, "create source file failed");
        return;
    }
    close(fd);

    if (mount(source, target, NULL, MS_BIND, NULL) != 0) {
        cleanup_mount(source);
        TEST_FAIL(name, "bind mount failed");
        return;
    }

    if (mount(target, target, NULL,
              MS_BIND | MS_REMOUNT | MS_RDONLY | MS_NODEV | MS_NOSUID | MS_NOEXEC,
              NULL) != 0) {
        umount(target);
        cleanup_mount(source);
        TEST_FAIL(name, strerror(errno));
        return;
    }

    snprintf(dst_file, sizeof(dst_file), "%s/readonly.txt", target);
    fd = open(dst_file, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd >= 0) {
        close(fd);
        unlink(dst_file);
        umount(target);
        cleanup_mount(source);
        TEST_FAIL(name, "readonly bind mount still writable");
        return;
    }

    if (errno != EROFS) {
        umount(target);
        cleanup_mount(source);
        TEST_FAIL(name, "expected EROFS on readonly bind mount");
        return;
    }

    umount(target);
    cleanup_mount(source);
    TEST_PASS(name);
}

static void test_self_bind_subdir_remount_readonly(void) {
    const char *name = "self_bind_subdir_remount_readonly";
    const char *base = "/tmp/test_mount_reconfigure_subdir";
    const char *root = "/tmp/test_mount_reconfigure_subdir/root";
    const char *subdir = "/tmp/test_mount_reconfigure_subdir/root/proc_bus";
    char ro_file[256];
    int fd;

    ensure_dir("/tmp");
    ensure_dir(base);
    ensure_dir(root);

    if (unshare(CLONE_NEWNS) != 0) {
        TEST_SKIP(name, strerror(errno));
        return;
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    if (mount("", root, "ramfs", 0, NULL) != 0) {
        TEST_SKIP(name, strerror(errno));
        return;
    }

    if (ensure_dir(subdir) != 0) {
        cleanup_mount(root);
        TEST_FAIL(name, "create subdir failed");
        return;
    }

    if (mount(subdir, subdir, NULL, MS_BIND | MS_REC, NULL) != 0) {
        cleanup_mount(root);
        TEST_FAIL(name, "self bind mount failed");
        return;
    }

    if (mount(subdir, subdir, NULL,
              MS_BIND | MS_REC | MS_REMOUNT | MS_RDONLY,
              NULL) != 0) {
        umount(subdir);
        cleanup_mount(root);
        TEST_FAIL(name, strerror(errno));
        return;
    }

    snprintf(ro_file, sizeof(ro_file), "%s/ro.txt", subdir);
    fd = open(ro_file, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd >= 0) {
        close(fd);
        unlink(ro_file);
        umount(subdir);
        cleanup_mount(root);
        TEST_FAIL(name, "self bind remount is still writable");
        return;
    }

    if (errno != EROFS) {
        umount(subdir);
        cleanup_mount(root);
        TEST_FAIL(name, "expected EROFS after self bind remount");
        return;
    }

    if (umount(subdir) != 0) {
        cleanup_mount(root);
        TEST_FAIL(name, "umount(self bind subdir) failed");
        return;
    }
    cleanup_mount(root);
    TEST_PASS(name);
}

static void test_bind_subdir_preserves_subtree_root(void) {
    const char *name = "bind_subdir_preserves_subtree_root";
    const char *base = "/tmp/test_mount_bind_subtree";
    const char *root = "/tmp/test_mount_bind_subtree/root";
    const char *subdir = "/tmp/test_mount_bind_subtree/root/subdir";
    const char *target = "/tmp/test_mount_bind_subtree/target";
    char sub_only[256];
    char root_only[256];
    int fd;

    ensure_dir("/tmp");
    ensure_dir(base);
    ensure_dir(root);
    ensure_dir(target);

    if (unshare(CLONE_NEWNS) != 0) {
        TEST_SKIP(name, strerror(errno));
        return;
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    if (mount("", root, "ramfs", 0, NULL) != 0) {
        TEST_SKIP(name, strerror(errno));
        return;
    }

    if (ensure_dir(subdir) != 0) {
        cleanup_mount(root);
        TEST_FAIL(name, "create subdir failed");
        return;
    }

    snprintf(sub_only, sizeof(sub_only), "%s/sub_only", subdir);
    fd = open(sub_only, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        cleanup_mount(root);
        TEST_FAIL(name, "create subdir marker failed");
        return;
    }
    close(fd);

    snprintf(root_only, sizeof(root_only), "%s/root_only", root);
    fd = open(root_only, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        unlink(sub_only);
        cleanup_mount(root);
        TEST_FAIL(name, "create root marker failed");
        return;
    }
    close(fd);

    if (mount(subdir, target, NULL, MS_BIND, NULL) != 0) {
        unlink(root_only);
        unlink(sub_only);
        cleanup_mount(root);
        TEST_FAIL(name, "bind mount failed");
        return;
    }

    snprintf(sub_only, sizeof(sub_only), "%s/sub_only", target);
    snprintf(root_only, sizeof(root_only), "%s/root_only", target);
    if (access(sub_only, F_OK) != 0) {
        umount(target);
        cleanup_mount(root);
        TEST_FAIL(name, "subdir marker missing from bind target");
        return;
    }

    if (access(root_only, F_OK) == 0) {
        umount(target);
        cleanup_mount(root);
        TEST_FAIL(name, "bind target exposed source root instead of subdir root");
        return;
    }

    if (umount(target) != 0) {
        cleanup_mount(root);
        TEST_FAIL(name, "umount(bind subtree target) failed");
        return;
    }
    cleanup_mount(root);
    TEST_PASS(name);
}

static void test_bind_remount_preserves_noatime(void) {
    const char *name = "bind_remount_preserves_noatime";
    const char *base = "/tmp/test_mount_reconfigure_atime";
    const char *source = "/tmp/test_mount_reconfigure_atime/source";
    const char *target = "/tmp/test_mount_reconfigure_atime/target";

    ensure_dir("/tmp");
    ensure_dir(base);
    ensure_dir(source);
    ensure_dir(target);

    if (unshare(CLONE_NEWNS) != 0) {
        TEST_SKIP(name, strerror(errno));
        return;
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    if (mount("", source, "ramfs", 0, NULL) != 0) {
        TEST_SKIP(name, strerror(errno));
        return;
    }

    if (mount(source, target, NULL, MS_BIND, NULL) != 0) {
        cleanup_mount(source);
        TEST_FAIL(name, "bind mount failed");
        return;
    }

    if (mount(target, target, NULL, MS_BIND | MS_REMOUNT | MS_NOATIME, NULL) != 0) {
        umount(target);
        cleanup_mount(source);
        TEST_FAIL(name, "set noatime on bind mount failed");
        return;
    }

    if (!mount_has_option(target, "noatime") || mount_has_option(target, "relatime")) {
        umount(target);
        cleanup_mount(source);
        TEST_FAIL(name, "bind mount did not enter noatime state");
        return;
    }

    if (mount(target, target, NULL, MS_BIND | MS_REMOUNT | MS_RDONLY, NULL) != 0) {
        umount(target);
        cleanup_mount(source);
        TEST_FAIL(name, "readonly bind remount failed");
        return;
    }

    if (!mount_has_option(target, "noatime")) {
        umount(target);
        cleanup_mount(source);
        TEST_FAIL(name, "readonly bind remount lost noatime");
        return;
    }

    if (mount_has_option(target, "relatime")) {
        umount(target);
        cleanup_mount(source);
        TEST_FAIL(name, "readonly bind remount unexpectedly enabled relatime");
        return;
    }

    umount(target);
    cleanup_mount(source);
    TEST_PASS(name);
}

int main(void) {
    test_bind_remount_readonly();
    test_self_bind_subdir_remount_readonly();
    test_bind_subdir_preserves_subtree_root();
    test_bind_remount_preserves_noatime();
    printf("passed=%d failed=%d\n", tests_passed, tests_failed);
    return tests_failed == 0 ? 0 : 1;
}
