#include <gtest/gtest.h>

#include <cerrno>
#include <cstring>
#include <string>
#include <linux/bpf.h>
#include <linux/perf_event.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/inotify.h>
#include <sys/signalfd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/timerfd.h>
#include <unistd.h>

namespace {

constexpr long kAnonInodeMagic = 0x09041934;

void CheckAnonFd(int fd, const char* expected_path) {
    ASSERT_GE(fd, 0) << strerror(errno);
    struct stat st = {};
    ASSERT_EQ(0, fstat(fd, &st)) << strerror(errno);
    EXPECT_EQ(0600U, st.st_mode & 07777);
    EXPECT_EQ(0U, st.st_mode & S_IFMT);
    struct statfs fs = {};
    ASSERT_EQ(0, fstatfs(fd, &fs)) << strerror(errno);
    EXPECT_EQ(kAnonInodeMagic, fs.f_type);
    char link[64];
    snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
    char target[128] = {};
    ssize_t n = readlink(link, target, sizeof(target) - 1);
    ASSERT_GT(n, 0) << strerror(errno);
    EXPECT_EQ(expected_path, std::string(target, n));
    EXPECT_EQ(0, close(fd));
}

TEST(AnonInodeSemantics, ExistingProvidersUseCommonFilesystem) {
    CheckAnonFd(eventfd(0, EFD_CLOEXEC), "anon_inode:[eventfd]");
    CheckAnonFd(timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC), "anon_inode:[timerfd]");
    CheckAnonFd(epoll_create1(EPOLL_CLOEXEC), "anon_inode:[eventpoll]");
    CheckAnonFd(inotify_init1(IN_CLOEXEC), "anon_inode:inotify");
    sigset_t mask;
    sigemptyset(&mask);
    CheckAnonFd(signalfd(-1, &mask, SFD_CLOEXEC), "anon_inode:[signalfd]");
    CheckAnonFd(static_cast<int>(syscall(SYS_pidfd_open, getpid(), 0)), "anon_inode:[pidfd]");
}

TEST(AnonInodeSemantics, SocketStatStillUsesSocketMode) {
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    ASSERT_GE(fd, 0) << strerror(errno);
    struct stat st = {};
    ASSERT_EQ(0, fstat(fd, &st)) << strerror(errno);
    EXPECT_TRUE(S_ISSOCK(st.st_mode));
    EXPECT_EQ(0, close(fd));
}

TEST(AnonInodeSemantics, BpfMapUsesCommonFilesystem) {
    union bpf_attr attr = {};
    attr.map_type = BPF_MAP_TYPE_ARRAY;
    attr.key_size = sizeof(uint32_t);
    attr.value_size = sizeof(uint64_t);
    attr.max_entries = 1;
    int fd = static_cast<int>(syscall(SYS_bpf, BPF_MAP_CREATE, &attr, sizeof(attr)));
    if (fd < 0 && (errno == EPERM || errno == EACCES)) {
        GTEST_SKIP() << "host BPF policy denied map creation";
    }
    CheckAnonFd(fd, "anon_inode:bpf-map");
}

TEST(AnonInodeSemantics, BpfProgramUsesCommonFilesystem) {
    const struct bpf_insn instructions[] = {
        {static_cast<__u8>(BPF_ALU | BPF_MOV | BPF_K), 0, 0, 0, 1},
        {static_cast<__u8>(BPF_JMP | BPF_EXIT), 0, 0, 0, 0},
    };
    static constexpr char license[] = "GPL";
    union bpf_attr attr = {};
    attr.prog_type = BPF_PROG_TYPE_CGROUP_DEVICE;
    attr.insn_cnt = 2;
    attr.insns = reinterpret_cast<uintptr_t>(instructions);
    attr.license = reinterpret_cast<uintptr_t>(license);
    int fd = static_cast<int>(syscall(SYS_bpf, BPF_PROG_LOAD, &attr, sizeof(attr)));
    if (fd < 0 && (errno == EPERM || errno == EACCES)) {
        GTEST_SKIP() << "host BPF policy denied program load";
    }
    CheckAnonFd(fd, "anon_inode:bpf-prog");
}

TEST(AnonInodeSemantics, PerfEventRetainsSpecialMmapFilesystem) {
    struct perf_event_attr attr = {};
    attr.type = PERF_TYPE_SOFTWARE;
    attr.size = sizeof(attr);
    attr.config = PERF_COUNT_SW_BPF_OUTPUT;
    attr.sample_type = PERF_SAMPLE_RAW;
    int fd = static_cast<int>(syscall(SYS_perf_event_open, &attr, -1, 0, -1, 0));
    if (fd < 0 && (errno == EPERM || errno == EACCES)) {
        GTEST_SKIP() << "host perf policy denied event creation";
    }
    CheckAnonFd(fd, "anon_inode:[perf_event]");
}

TEST(AnonInodeSemantics, FsContextUsesCommonFilesystem) {
    int fd = static_cast<int>(syscall(SYS_fsopen, "tmpfs", 1));  // FSOPEN_CLOEXEC
    if (fd < 0 && (errno == EPERM || errno == EACCES)) {
        GTEST_SKIP() << "host mount policy denied fsopen";
    }
    CheckAnonFd(fd, "anon_inode:[fscontext]");
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
