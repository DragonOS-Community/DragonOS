#include <gtest/gtest.h>

#include <atomic>
#include <cerrno>
#include <climits>
#include <cstdlib>
#include <cstdio>
#include <cstring>
#include <fcntl.h>
#include <sched.h>
#include <string>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <thread>
#include <unistd.h>

#ifndef CLONE_NEWUSER
#define CLONE_NEWUSER 0x10000000
#endif
#ifndef CLONE_NEWNS
#define CLONE_NEWNS 0x00020000
#endif
#ifndef MS_REC
#define MS_REC 16384
#endif
#ifndef MS_PRIVATE
#define MS_PRIVATE (1UL << 18)
#endif
#ifndef MS_SHARED
#define MS_SHARED (1UL << 20)
#endif
#ifndef MS_MOVE
#define MS_MOVE 8192
#endif

namespace {

constexpr char kMountMaxPath[] = "/proc/sys/fs/mount-max";

int ensure_dir(const std::string& path) {
    struct stat st = {};
    if (stat(path.c_str(), &st) == 0) {
        return S_ISDIR(st.st_mode) ? 0 : -1;
    }
    return mkdir(path.c_str(), 0755);
}

int read_mount_max() {
    int fd = open(kMountMaxPath, O_RDONLY);
    if (fd < 0) {
        return -1;
    }
    char buf[64] = {};
    const ssize_t n = read(fd, buf, sizeof(buf) - 1);
    const int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    if (n <= 0) {
        return -1;
    }
    char* end = nullptr;
    const long value = strtol(buf, &end, 10);
    return end == buf || value < 1 || value > INT_MAX ? -1 : static_cast<int>(value);
}

int write_mount_max_text(const char* text) {
    int fd = open(kMountMaxPath, O_WRONLY);
    if (fd < 0) {
        return -1;
    }
    const size_t len = strlen(text);
    const ssize_t written = write(fd, text, len);
    const int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return written == static_cast<ssize_t>(len) ? 0 : -1;
}

ssize_t write_mount_max_text_raw(const char* text) {
    int fd = open(kMountMaxPath, O_WRONLY);
    if (fd < 0) {
        return -1;
    }
    const ssize_t written = write(fd, text, strlen(text));
    const int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return written;
}

int write_mount_max(int value) {
    char buf[32] = {};
    snprintf(buf, sizeof(buf), "%d\n", value);
    return write_mount_max_text(buf);
}

int mount_count() {
    FILE* file = fopen("/proc/self/mountinfo", "r");
    if (file == nullptr) {
        return -1;
    }
    int count = 0;
    char line[2048] = {};
    while (fgets(line, sizeof(line), file) != nullptr) {
        ++count;
    }
    fclose(file);
    return count;
}

int mountpoint_count(const std::string& path) {
    FILE* file = fopen("/proc/self/mountinfo", "r");
    if (file == nullptr) {
        return -1;
    }
    int count = 0;
    char line[2048] = {};
    while (fgets(line, sizeof(line), file) != nullptr) {
        char mountpoint[1024] = {};
        if (sscanf(line, "%*s %*s %*s %*s %1023s", mountpoint) == 1 &&
            path == mountpoint) {
            ++count;
        }
    }
    fclose(file);
    return count;
}

void detach_all(const std::string& path) {
    for (int i = 0; i < 64; ++i) {
        if (umount2(path.c_str(), MNT_DETACH) == 0) {
            continue;
        }
        if (errno == EINVAL || errno == ENOENT) {
            return;
        }
        return;
    }
}

class MountLimitTest : public ::testing::Test {
protected:
    std::string root_;
    int original_max_ = -1;

    void SetUp() override {
        original_max_ = read_mount_max();
        ASSERT_EQ(100000, original_max_);
        ASSERT_EQ(0, unshare(CLONE_NEWNS)) << strerror(errno);
        ASSERT_EQ(0, mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr))
            << strerror(errno);
        static std::atomic<unsigned int> sequence{0};
        root_ = "/tmp/mount_limit_" + std::to_string(getpid()) + "_" +
                std::to_string(sequence.fetch_add(1));
        ASSERT_EQ(0, ensure_dir("/tmp")) << strerror(errno);
        ASSERT_EQ(0, ensure_dir(root_)) << strerror(errno);
    }

    void TearDown() override {
        if (original_max_ > 0) {
            EXPECT_EQ(0, write_mount_max(original_max_)) << strerror(errno);
        }
        const char* suffixes[] = {
            "/a/child", "/b/child", "/a/mp", "/b/mp", "/rec/a/b",
            "/rec/a",   "/extra",   "/p1",   "/p2",   "/dst",
            "/src",     "/b",       "/a",    "/c",
        };
        for (const char* suffix : suffixes) {
            detach_all(root_ + suffix);
        }
    }

    std::string dir(const char* suffix) {
        const std::string path = root_ + suffix;
        EXPECT_EQ(0, ensure_dir(path)) << path << ": " << strerror(errno);
        return path;
    }
};

TEST_F(MountLimitTest, SysctlMatchesLinuxNumericSemantics) {
    int fd = open(kMountMaxPath, O_RDWR);
    ASSERT_GE(fd, 0) << strerror(errno);
    char partial[4] = {};
    ASSERT_EQ(0, pread(fd, partial, 3, 1)) << strerror(errno);
    ASSERT_EQ(3, pwrite(fd, "123", 3, 1)) << strerror(errno);
    EXPECT_EQ(original_max_, read_mount_max());
    close(fd);

    errno = 0;
    EXPECT_EQ(-1, write_mount_max_text("0\n"));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, write_mount_max_text("-1\n"));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, write_mount_max_text("2147483648\n"));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, write_mount_max_text("000000000000000000001\n"));
    EXPECT_EQ(EINVAL, errno);
    ASSERT_EQ(3, write_mount_max_text_raw("12 garbage\n")) << strerror(errno);
    EXPECT_EQ(12, read_mount_max());
    ASSERT_EQ(0, write_mount_max_text(" 12345 \n")) << strerror(errno);
    EXPECT_EQ(12345, read_mount_max());
}

TEST_F(MountLimitTest, SysctlWritePermissionUsesGlobalEffectiveUid) {
    const int candidate = original_max_ - 1;
    char text[32] = {};
    const int text_len = snprintf(text, sizeof(text), "%d\n", candidate);
    ASSERT_GT(text_len, 0);
    ASSERT_LT(static_cast<size_t>(text_len), sizeof(text));

    int inherited_fd = open(kMountMaxPath, O_WRONLY);
    ASSERT_GE(inherited_fd, 0) << strerror(errno);
    pid_t child = fork();
    if (child < 0) {
        const int saved_errno = errno;
        close(inherited_fd);
        errno = saved_errno;
    }
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (setgid(1000) != 0) {
            _exit(10);
        }
        if (setuid(1000) != 0) {
            _exit(11);
        }
        if (unshare(CLONE_NEWUSER) != 0) {
            _exit(12);
        }

        errno = 0;
        if (pwrite(inherited_fd, text, static_cast<size_t>(text_len), 0) != -1 ||
            errno != EPERM) {
            _exit(13);
        }
        close(inherited_fd);

        errno = 0;
        int reopened_fd = open(kMountMaxPath, O_WRONLY);
        if (reopened_fd < 0) {
            _exit(errno == EACCES || errno == EPERM ? 0 : 14);
        }
        errno = 0;
        const ssize_t written = write(reopened_fd, text, static_cast<size_t>(text_len));
        const int write_errno = errno;
        close(reopened_fd);
        _exit(written == -1 && write_errno == EPERM ? 0 : 15);
    }
    close(inherited_fd);

    int status = 0;
    pid_t waited = -1;
    do {
        waited = waitpid(child, &status, 0);
    } while (waited < 0 && errno == EINTR);
    ASSERT_EQ(child, waited) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));
    EXPECT_EQ(original_max_, read_mount_max());
    ASSERT_EQ(0, write_mount_max(original_max_)) << strerror(errno);

    child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (unshare(CLONE_NEWUSER) != 0) {
            _exit(20);
        }
        _exit(write_mount_max(candidate) == 0 ? 0 : 21);
    }
    status = 0;
    do {
        waited = waitpid(child, &status, 0);
    } while (waited < 0 && errno == EINTR);
    ASSERT_EQ(child, waited) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));
    EXPECT_EQ(candidate, read_mount_max());
    ASSERT_EQ(0, write_mount_max(original_max_)) << strerror(errno);
}

TEST_F(MountLimitTest, SimpleBoundaryAndUnmountReleaseCapacity) {
    const std::string p1 = dir("/p1");
    const std::string p2 = dir("/p2");
    const int before = mount_count();
    ASSERT_GT(before, 0);
    ASSERT_EQ(0, write_mount_max(before + 1)) << strerror(errno);
    ASSERT_EQ(0, mount("none", p1.c_str(), "ramfs", 0, nullptr)) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, mount("none", p2.c_str(), "ramfs", 0, nullptr));
    EXPECT_EQ(ENOSPC, errno);
    EXPECT_EQ(before + 1, mount_count());
    ASSERT_EQ(0, umount(p1.c_str())) << strerror(errno);
    ASSERT_EQ(0, mount("none", p2.c_str(), "ramfs", 0, nullptr)) << strerror(errno);
}

TEST_F(MountLimitTest, RecursiveBindFailureIsInvisibleAndReservationRollsBack) {
    const std::string rec = dir("/rec");
    const std::string a = dir("/rec/a");
    const std::string b = dir("/rec/a/b");
    const std::string c = dir("/c");
    (void)rec;
    ASSERT_EQ(0, mount(a.c_str(), a.c_str(), nullptr, MS_BIND, nullptr)) << strerror(errno);
    const int before = mount_count();
    ASSERT_EQ(0, write_mount_max(before + 3)) << strerror(errno);
    ASSERT_EQ(0, mount(a.c_str(), b.c_str(), nullptr, MS_BIND | MS_REC, nullptr))
        << strerror(errno);
    ASSERT_EQ(0, mount(a.c_str(), b.c_str(), nullptr, MS_BIND | MS_REC, nullptr))
        << strerror(errno);
    const int at_limit = mount_count();
    ASSERT_EQ(before + 3, at_limit);
    errno = 0;
    EXPECT_EQ(-1, mount(a.c_str(), b.c_str(), nullptr, MS_BIND | MS_REC, nullptr));
    EXPECT_EQ(ENOSPC, errno);
    EXPECT_EQ(at_limit, mount_count());
    detach_all(b);
    ASSERT_EQ(0, mount("none", c.c_str(), "ramfs", 0, nullptr)) << strerror(errno);
}

TEST_F(MountLimitTest, PropagationAndSharedMoveReserveCompleteCopies) {
    const std::string a = dir("/a");
    const std::string b = dir("/b");
    const std::string src = dir("/src");
    ASSERT_EQ(0, mount("none", a.c_str(), "ramfs", 0, nullptr)) << strerror(errno);
    const std::string child = dir("/a/child");
    const std::string move_point = dir("/a/mp");
    ASSERT_EQ(0, mount(nullptr, a.c_str(), nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(a.c_str(), b.c_str(), nullptr, MS_BIND, nullptr)) << strerror(errno);
    const std::string peer_child = root_ + "/b/child";
    const std::string peer_move_point = root_ + "/b/mp";

    int before = mount_count();
    ASSERT_EQ(0, write_mount_max(before + 1)) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, mount("none", child.c_str(), "ramfs", 0, nullptr));
    EXPECT_EQ(ENOSPC, errno);
    EXPECT_EQ(before, mount_count());
    EXPECT_EQ(0, mountpoint_count(child));
    EXPECT_EQ(0, mountpoint_count(peer_child));

    ASSERT_EQ(0, write_mount_max(original_max_)) << strerror(errno);
    ASSERT_EQ(0, mount("none", src.c_str(), "ramfs", 0, nullptr)) << strerror(errno);
    before = mount_count();
    ASSERT_EQ(0, write_mount_max(before)) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, mount(src.c_str(), move_point.c_str(), nullptr, MS_MOVE, nullptr));
    EXPECT_EQ(ENOSPC, errno);
    EXPECT_EQ(1, mountpoint_count(src));
    EXPECT_EQ(0, mountpoint_count(move_point));
    EXPECT_EQ(0, mountpoint_count(peer_move_point));
    EXPECT_EQ(before, mount_count());
}

TEST_F(MountLimitTest, NamespaceCopyIgnoresLoweredLimitAndCreatorsRespectIt) {
    int before = mount_count();
    ASSERT_GT(before, 1);
    ASSERT_EQ(0, write_mount_max(before - 1)) << strerror(errno);
    ASSERT_EQ(0, unshare(CLONE_NEWNS)) << strerror(errno);
    EXPECT_EQ(before, mount_count());

    const std::string p1 = dir("/p1");
    const std::string p2 = dir("/p2");
    errno = 0;
    EXPECT_EQ(-1, mount("none", p1.c_str(), "ramfs", 0, nullptr));
    EXPECT_EQ(ENOSPC, errno);
    EXPECT_EQ(before, mount_count());

    ASSERT_EQ(0, write_mount_max(before)) << strerror(errno);
    ASSERT_EQ(0, unshare(CLONE_NEWNS)) << strerror(errno);
    EXPECT_EQ(before, mount_count());

    ASSERT_EQ(0, write_mount_max(before + 1)) << strerror(errno);
    std::atomic<int> ready{0};
    std::atomic<bool> go{false};
    int results[2] = {};
    int errors[2] = {};
    auto creator = [&](int index, const std::string& path) {
        ready.fetch_add(1);
        while (!go.load()) {
            std::this_thread::yield();
        }
        results[index] = mount("none", path.c_str(), "ramfs", 0, nullptr);
        errors[index] = errno;
    };
    std::thread first(creator, 0, p1);
    std::thread second(creator, 1, p2);
    while (ready.load() != 2) {
        std::this_thread::yield();
    }
    go.store(true);
    first.join();
    second.join();
    const int successes = (results[0] == 0) + (results[1] == 0);
    EXPECT_EQ(1, successes);
    EXPECT_TRUE((results[0] == -1 && errors[0] == ENOSPC) ||
                (results[1] == -1 && errors[1] == ENOSPC));
    EXPECT_EQ(before + 1, mount_count());
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
