#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

/*
 * 本测试覆盖 cgroup v2 MVP 的基础链路：
 * 1) /sys/fs/cgroup 存在，且可创建子 cgroup。
 * 2) 通过写 cgroup.procs 可以迁移当前进程。
 * 3) /proc/self/cgroup 能反映迁移后的相对路径。
 * 4) subtree_control 支持空写入，并拒绝未支持的控制器。
 * 5) 多挂载场景下，创建/删除在不同挂载点之间可见一致。
 */

static void fail(const char *step) {
    printf("[FAIL] %s: %s\n", step, strerror(errno));
    exit(1);
}

static int path_exists(const char *path) {
    struct stat st;
    return stat(path, &st) == 0;
}

static int ensure_dir(const char *path) {
    if (mkdir(path, 0755) == 0) {
        return 0;
    }
    if (errno == EEXIST) {
        return 0;
    }
    return -1;
}

static int write_text(const char *path, const char *text) {
    int fd = open(path, O_WRONLY);
    ssize_t n;
    if (fd < 0) {
        return -1;
    }
    n = write(fd, text, strlen(text));
    close(fd);
    return n == (ssize_t)strlen(text) ? 0 : -1;
}

static int read_text(const char *path, char *buf, size_t len) {
    int fd = open(path, O_RDONLY);
    ssize_t n;
    if (fd < 0) {
        return -1;
    }
    n = read(fd, buf, len - 1);
    close(fd);
    if (n < 0) {
        return -1;
    }
    buf[n] = '\0';
    return 0;
}

int main(void) {
    const char *root = "/sys/fs/cgroup";
    const char *grp = "/sys/fs/cgroup/mvp_basic";
    const char *procs = "/sys/fs/cgroup/mvp_basic/cgroup.procs";
    const char *sub = "/sys/fs/cgroup/mvp_basic/cgroup.subtree_control";
    const char *mnt_a = "/tmp/cgmm_a";
    const char *mnt_b = "/tmp/cgmm_b";
    const char *cg_name = "cg_multi_basic";
    char path_a[256];
    char path_b[256];
    char buf[512];

    if (!path_exists(root)) {
        errno = ENOENT;
        fail("check /sys/fs/cgroup exists");
    }

    if (ensure_dir(grp) != 0) {
        fail("mkdir /sys/fs/cgroup/mvp_basic");
    }

    if (write_text(procs, "0\n") != 0) {
        fail("migrate self by writing cgroup.procs");
    }

    if (read_text("/proc/self/cgroup", buf, sizeof(buf)) != 0) {
        fail("read /proc/self/cgroup");
    }
    if (strstr(buf, "0::/mvp_basic") == NULL) {
        printf("[FAIL] unexpected /proc/self/cgroup: %s\n", buf);
        return 1;
    }

    /* 空写入应当被接受，且 subtree_control 内容保持不变。 */
    if (write_text(sub, "\n") != 0) {
        fail("write empty subtree_control");
    }
    if (read_text(sub, buf, sizeof(buf)) != 0) {
        fail("read subtree_control");
    }
    if (strcmp(buf, "\n") != 0) {
        printf("[FAIL] unexpected subtree_control content: %s\n", buf);
        return 1;
    }

    /* MVP 阶段未支持的控制器应当返回失败。 */
    errno = 0;
    if (write_text(sub, "+cpu\n") == 0) {
        printf("[FAIL] +cpu unexpectedly succeeded\n");
        return 1;
    }
    if (errno != EINVAL && errno != EPERM) {
        printf("[FAIL] +cpu unexpected errno: %d\n", errno);
        return 1;
    }

    if (ensure_dir(mnt_a) != 0 || ensure_dir(mnt_b) != 0) {
        fail("prepare multi-mount directories");
    }

    if (mount("none", mnt_a, "cgroup2", 0, NULL) < 0) {
        fail("mount cgroup2 on mnt_a");
    }
    if (mount("none", mnt_b, "cgroup2", 0, NULL) < 0) {
        umount(mnt_a);
        fail("mount cgroup2 on mnt_b");
    }

    snprintf(path_a, sizeof(path_a), "%s/%s", mnt_a, cg_name);
    snprintf(path_b, sizeof(path_b), "%s/%s", mnt_b, cg_name);

    if (mkdir(path_a, 0755) < 0) {
        umount(mnt_b);
        umount(mnt_a);
        fail("create cgroup from mount A");
    }
    if (access(path_b, F_OK) != 0) {
        printf("[FAIL] cross-mount visibility missing: %s\n", path_b);
        umount(mnt_b);
        umount(mnt_a);
        return 1;
    }

    if (rmdir(path_b) < 0) {
        umount(mnt_b);
        umount(mnt_a);
        fail("remove cgroup from mount B");
    }
    if (access(path_a, F_OK) == 0 || errno != ENOENT) {
        printf("[FAIL] cross-mount removal visibility failed: %s\n", path_a);
        umount(mnt_b);
        umount(mnt_a);
        return 1;
    }

    if (umount(mnt_b) < 0 || umount(mnt_a) < 0) {
        fail("umount multi-mount points");
    }

    printf("[PASS] cgroup_mvp_basic\n");
    return 0;
}
