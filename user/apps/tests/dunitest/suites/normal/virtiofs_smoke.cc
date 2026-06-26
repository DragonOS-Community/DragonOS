#include <gtest/gtest.h>

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>

#ifndef EOPNOTSUPP
#define EOPNOTSUPP 95
#endif

namespace {

int ensure_dir(const char* path) {
    struct stat st = {};
    if (stat(path, &st) == 0) {
        return S_ISDIR(st.st_mode) ? 0 : -1;
    }
    return mkdir(path, 0755);
}

void best_effort_umount(const char* path) {
    if (umount(path) != 0 && errno != EINVAL && errno != ENOENT) {
        ADD_FAILURE() << "umount(" << path << ") failed: errno=" << errno << " ("
                      << strerror(errno) << ")";
    }
}

void best_effort_rmdir(const char* path) {
    if (rmdir(path) != 0 && errno != ENOENT && errno != ENOTEMPTY) {
        ADD_FAILURE() << "rmdir(" << path << ") failed: errno=" << errno << " ("
                      << strerror(errno) << ")";
    }
}

void cleanup_tree(const char* root) {
    char path[256] = {};
    snprintf(path, sizeof(path), "%s/local_busybox", root);
    unlink(path);
    snprintf(path, sizeof(path), "%s/merged", root);
    best_effort_umount(path);
    best_effort_rmdir(path);
    snprintf(path, sizeof(path), "%s/mnt", root);
    best_effort_umount(path);
    best_effort_rmdir(path);
    snprintf(path, sizeof(path), "%s/upper", root);
    best_effort_rmdir(path);
    snprintf(path, sizeof(path), "%s/work", root);
    best_effort_rmdir(path);
    best_effort_rmdir(root);
}

void assert_file_contains_prefix(const char* path, const char* expected) {
    int fd = open(path, O_RDONLY);
    ASSERT_GE(fd, 0) << "open(" << path << ") failed: " << strerror(errno);

    char buf[128] = {};
    ssize_t n = read(fd, buf, sizeof(buf) - 1);
    int saved_errno = errno;
    close(fd);

    ASSERT_GT(n, 0) << "read(" << path << ") failed: " << strerror(saved_errno);
    EXPECT_EQ(0, strncmp(buf, expected, strlen(expected)))
        << "path=" << path << " content=" << buf;
}

void assert_copy_file(const char* src, const char* dst) {
    int in = open(src, O_RDONLY);
    ASSERT_GE(in, 0) << "open(" << src << ") failed: " << strerror(errno);
    int out = open(dst, O_CREAT | O_TRUNC | O_WRONLY, 0755);
    ASSERT_GE(out, 0) << "open(" << dst << ") failed: " << strerror(errno);

    char buf[8192];
    for (;;) {
        ssize_t n = read(in, buf, sizeof(buf));
        ASSERT_GE(n, 0) << "read(" << src << ") failed: " << strerror(errno);
        if (n == 0) {
            break;
        }
        ssize_t off = 0;
        while (off < n) {
            ssize_t written = write(out, buf + off, n - off);
            ASSERT_GT(written, 0) << "write(" << dst << ") failed: " << strerror(errno);
            off += written;
        }
    }
    ASSERT_EQ(0, close(out)) << "close(" << dst << ") failed: " << strerror(errno);
    ASSERT_EQ(0, close(in)) << "close(" << src << ") failed: " << strerror(errno);
    ASSERT_EQ(0, chmod(dst, 0755)) << "chmod(" << dst << ") failed: " << strerror(errno);
}

void assert_mmap_matches_pread(const char* path) {
    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: " << strerror(errno);

    if (child == 0) {
        int fd = open(path, O_RDONLY);
        if (fd < 0) {
            fprintf(stderr, "open(%s) failed: %s\n", path, strerror(errno));
            _exit(101);
        }
        struct stat st = {};
        if (fstat(fd, &st) != 0 || st.st_size <= 0) {
            fprintf(stderr, "fstat(%s) failed or empty: %s\n", path, strerror(errno));
            _exit(102);
        }
        void* map = mmap(nullptr, st.st_size, PROT_READ | PROT_EXEC, MAP_PRIVATE, fd, 0);
        if (map == MAP_FAILED) {
            fprintf(stderr, "mmap(%s) failed: %s\n", path, strerror(errno));
            _exit(103);
        }

        char buf[4096];
        const unsigned char* mapped = static_cast<const unsigned char*>(map);
        for (off_t off = 0; off < st.st_size; off += sizeof(buf)) {
            size_t want = st.st_size - off < (off_t)sizeof(buf) ? st.st_size - off : sizeof(buf);
            ssize_t n = pread(fd, buf, want, off);
            if (n != (ssize_t)want) {
                fprintf(stderr, "pread(%s, off=%ld) got %zd want %zu errno=%d\n", path,
                        (long)off, n, want, errno);
                _exit(104);
            }
            if (memcmp(mapped + off, buf, want) != 0) {
                fprintf(stderr, "mmap/pread mismatch path=%s off=%ld len=%zu\n", path, (long)off,
                        want);
                _exit(105);
            }
        }
        munmap(map, st.st_size);
        close(fd);
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << "waitpid failed: " << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status)) << "mmap compare child signaled, status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status)) << "mmap compare failed for " << path
                                      << " status=" << status;
}

void assert_dir_has_entry(const char* path, const char* name) {
    DIR* dir = opendir(path);
    ASSERT_NE(nullptr, dir) << "opendir(" << path << ") failed: " << strerror(errno);

    bool found = false;
    while (dirent* ent = readdir(dir)) {
        if (strcmp(ent->d_name, name) == 0) {
            found = true;
            break;
        }
    }
    closedir(dir);
    EXPECT_TRUE(found) << path << " missing " << name;
}

void assert_listxattr_reaches_filesystem(const char* path) {
    errno = 0;
    ssize_t n = listxattr(path, nullptr, 0);
    if (n >= 0) {
        return;
    }
    ASSERT_NE(ENOSYS, errno) << "listxattr syscall is not registered for " << path;
    ASSERT_TRUE(errno == EOPNOTSUPP || errno == ENOTSUP || errno == ENODATA)
        << "listxattr(" << path << ") failed unexpectedly: errno=" << errno << " ("
        << strerror(errno) << ")";
}

void assert_exec_busybox(const char* busybox, const char* applet, int iteration) {
    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: " << strerror(errno);

    if (child == 0) {
        char* const argv[] = {
            const_cast<char*>("busybox"),
            const_cast<char*>(applet),
            const_cast<char*>("-a"),
            nullptr,
        };
        execv(busybox, argv);
        _exit(127);
    }

    int status = 0;
    for (int i = 0; i < 100; ++i) {
        pid_t ret = waitpid(child, &status, WNOHANG);
        if (ret == child) {
            ASSERT_TRUE(WIFEXITED(status))
                << "child status=" << status << " busybox=" << busybox << " applet=" << applet
                << " iteration=" << iteration;
            EXPECT_EQ(0, WEXITSTATUS(status)) << "child status=" << status
                                              << " busybox=" << busybox;
            return;
        }
        ASSERT_EQ(0, ret) << "waitpid failed: " << strerror(errno);
        usleep(100000);
    }

    kill(child, SIGKILL);
    waitpid(child, &status, 0);
    FAIL() << "exec timed out for " << busybox << " " << applet << " iteration=" << iteration;
}

void assert_repeated_busybox_exec(const char* busybox, const char* applet, int count) {
    for (int i = 0; i < count; ++i) {
        assert_exec_busybox(busybox, applet, i);
    }
}

void assert_directory_probe_loop(const char* path, int count, bool probe_xattr) {
    for (int i = 0; i < count; ++i) {
        assert_dir_has_entry(path, "busybox");
        if (probe_xattr) {
            assert_listxattr_reaches_filesystem(path);
        }

        char child[256] = {};
        snprintf(child, sizeof(child), "%s/hello.txt", path);
        struct stat st = {};
        ASSERT_EQ(0, lstat(child, &st)) << "lstat(" << child << ") failed: " << strerror(errno);
        ASSERT_TRUE(S_ISREG(st.st_mode)) << child << " is not a regular file";
    }
}

bool should_skip_missing_virtiofs(int err) {
    return err == ENODEV || err == ENOENT || err == EINVAL || err == EOPNOTSUPP || err == ENOSYS;
}

}  // namespace

TEST(VirtioFsSmoke, MountReadExecAndOverlayLower) {
    char root[128] = {};
    char mnt[160] = {};
    char upper[160] = {};
    char work[160] = {};
    char merged[160] = {};
    char path[256] = {};
    char options[512] = {};

    snprintf(root, sizeof(root), "/tmp/virtiofs_smoke_%d", getpid());
    snprintf(mnt, sizeof(mnt), "%s/mnt", root);
    snprintf(upper, sizeof(upper), "%s/upper", root);
    snprintf(work, sizeof(work), "%s/work", root);
    snprintf(merged, sizeof(merged), "%s/merged", root);

    ASSERT_EQ(0, ensure_dir("/tmp")) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(root)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(mnt)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(upper)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(work)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(merged)) << strerror(errno);

    assert_repeated_busybox_exec("/bin/busybox", "ls", 1);
    assert_repeated_busybox_exec("/bin/busybox", "uname", 1);

    if (mount("hostshare", mnt, "virtiofs", 0, nullptr) != 0) {
        int err = errno;
        cleanup_tree(root);
        if (should_skip_missing_virtiofs(err)) {
            GTEST_SKIP() << "virtiofs hostshare is unavailable: errno=" << err << " ("
                         << strerror(err) << ")";
        }
        FAIL() << "mount virtiofs failed: errno=" << err << " (" << strerror(err) << ")";
    }

    assert_directory_probe_loop(mnt, 3, true);
    snprintf(path, sizeof(path), "%s/hello.txt", mnt);
    assert_file_contains_prefix(path, "virtiofs-host-file");

    snprintf(path, sizeof(path), "%s/busybox", mnt);
    assert_mmap_matches_pread(path);
    char local_copy[160] = {};
    snprintf(local_copy, sizeof(local_copy), "%s/local_busybox", root);
    assert_copy_file(path, local_copy);
    assert_repeated_busybox_exec(local_copy, "ls", 1);
    assert_repeated_busybox_exec(local_copy, "uname", 1);
    assert_repeated_busybox_exec(path, "ls", 2);
    assert_repeated_busybox_exec(path, "uname", 3);

    snprintf(options, sizeof(options), "lowerdir=%s,upperdir=%s,workdir=%s", mnt, upper, work);
    ASSERT_EQ(0, mount("overlay", merged, "overlay", 0, options))
        << "mount overlay failed: " << strerror(errno);

    assert_directory_probe_loop(merged, 3, false);
    snprintf(path, sizeof(path), "%s/busybox", merged);
    assert_mmap_matches_pread(path);
    assert_repeated_busybox_exec(path, "ls", 2);
    assert_repeated_busybox_exec(path, "uname", 3);

    cleanup_tree(root);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
