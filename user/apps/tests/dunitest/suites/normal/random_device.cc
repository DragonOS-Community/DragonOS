#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <linux/random.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <vector>

namespace {

class Fd {
public:
    explicit Fd(const char* path) : value(open(path, O_RDWR | O_CLOEXEC)) {}
    ~Fd() { if (value >= 0) close(value); }
    int value;
};

bool DragonOSGuest() {
    utsname info{};
    return uname(&info) == 0 && strstr(info.release, "-dragonos") != nullptr;
}

long Random(void* output, size_t size, unsigned flags) {
    return syscall(SYS_getrandom, output, size, flags);
}

// Subprocesses contain credential changes and ensure a deadlocked descriptor
// cannot hang the remaining suite. No test resets the host's entropy pool.
int ChildResult(int (*body)()) {
    pid_t child = fork();
    if (child < 0) return -1;
    if (child == 0) _exit(body());
    int status = 0;
    for (int i = 0; i < 100; ++i) {
        pid_t result = waitpid(child, &status, WNOHANG);
        if (result == child) return WIFEXITED(status) ? WEXITSTATUS(status) : -2;
        if (result < 0) return -3;
        usleep(50000);
    }
    kill(child, SIGKILL);
    waitpid(child, &status, 0);
    return -4;
}

int DeniedIoctls(bool user_namespace) {
    Fd fd("/dev/random");
    if (fd.value < 0) return 1;
    if (user_namespace) {
        if (unshare(CLONE_NEWUSER) != 0) return errno == EPERM || errno == EACCES ? 77 : 2;
    } else if (geteuid() == 0 && setuid(65534) != 0) {
        return 3;
    }
    for (unsigned command : std::array<unsigned, 5>{RNDADDTOENTCNT, RNDADDENTROPY,
                              RNDRESEEDCRNG, RNDZAPENTCNT, RNDCLEARPOOL}) {
        errno = 0;
        if (ioctl(fd.value, command, reinterpret_cast<void*>(1)) != -1 || errno != EPERM)
            return 4;
    }
    return 0;
}

void* ReadSharedFd(void* pointer) {
    int fd = *static_cast<int*>(pointer);
    std::array<unsigned char, 256> bytes{};
    return read(fd, bytes.data(), bytes.size()) == static_cast<ssize_t>(bytes.size())
               ? nullptr : reinterpret_cast<void*>(1);
}

}  // namespace

TEST(RandomDevice, SharedPoolCountAndUnknownIoctl) {
    Fd random("/dev/random"), urandom("/dev/urandom");
    ASSERT_GE(random.value, 0);
    ASSERT_GE(urandom.value, 0);
    int first = -1, second = -1;
    ASSERT_EQ(ioctl(random.value, RNDGETENTCNT, &first), 0);
    ASSERT_EQ(ioctl(urandom.value, RNDGETENTCNT, &second), 0);
    EXPECT_GE(first, 0);
    EXPECT_LE(first, 256);
    EXPECT_EQ(first, second);
    errno = 0;
    EXPECT_EQ(ioctl(random.value, RNDGETENTCNT, reinterpret_cast<void*>(1)), -1);
    EXPECT_EQ(errno, EFAULT);
    errno = 0;
    EXPECT_EQ(ioctl(random.value, 0x52ff, 0), -1);
    EXPECT_EQ(errno, EINVAL);
}

TEST(RandomDevice, WritesDoNotCreditEntropy) {
    Fd fd("/dev/urandom");
    ASSERT_GE(fd.value, 0);
    int before = -1, after = -1;
    ASSERT_EQ(ioctl(fd.value, RNDGETENTCNT, &before), 0);
    // Public, deterministic input must never count as trusted entropy.
    std::array<unsigned char, 64> input{};
    ASSERT_EQ(write(fd.value, input.data(), input.size()), static_cast<ssize_t>(input.size()));
    ASSERT_EQ(ioctl(fd.value, RNDGETENTCNT, &after), 0);
    EXPECT_EQ(before, after);
}

TEST(RandomDevice, PrivilegedIoctlsCheckCapabilityBeforePointer) {
    EXPECT_EQ(ChildResult([] { return DeniedIoctls(false); }), 0);
}

TEST(RandomDevice, ChildUserNamespaceCannotControlGlobalPool) {
    int result = ChildResult([] { return DeniedIoctls(true); });
    if (result == 77) GTEST_SKIP() << "host policy denies creation of a user namespace";
    EXPECT_EQ(result, 0);
}

TEST(RandomDevice, GetrandomValidatesAllFlagBitsAndConflicts) {
    unsigned char byte = 0;
    for (unsigned flags : std::array<unsigned, 4>{0x100u, 0x80000000u, GRND_RANDOM | GRND_INSECURE,
                           GRND_RANDOM | GRND_INSECURE | GRND_NONBLOCK}) {
        errno = 0;
        EXPECT_EQ(Random(&byte, 1, flags), -1) << flags;
        EXPECT_EQ(errno, EINVAL) << flags;
        errno = 0;
        EXPECT_EQ(Random(nullptr, 0, flags), -1) << flags;
        EXPECT_EQ(errno, EINVAL) << flags;
    }
    for (unsigned flags : std::array<unsigned, 6>{0u, GRND_NONBLOCK, GRND_RANDOM, GRND_INSECURE,
                           GRND_RANDOM | GRND_NONBLOCK, GRND_INSECURE | GRND_NONBLOCK}) {
        EXPECT_EQ(Random(nullptr, 0, flags), 0) << flags;
        EXPECT_EQ(Random(&byte, 1, flags), 1) << flags;
    }
#if __SIZEOF_LONG__ > 4
    // Linux's unsigned-int ABI ignores the syscall register's upper half,
    // while retaining unknown-bit validation throughout the lower 32 bits.
    EXPECT_EQ(syscall(SYS_getrandom, &byte, 1, 1UL << 32), 1);
    errno = 0;
    EXPECT_EQ(syscall(SYS_getrandom, &byte, 1, (1UL << 32) | 0x100UL), -1);
    EXPECT_EQ(errno, EINVAL);
#endif
}

TEST(RandomDevice, FaultAfterCopiedPageReturnsPartialCount) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    void* mapping = mmap(nullptr, page * 2, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(mapping, MAP_FAILED);
    ASSERT_EQ(mprotect(static_cast<char*>(mapping) + page, page, PROT_NONE), 0);
    // Ready, small getrandom requests must work independently of user faults.
    std::array<unsigned char, 256> initial{};
    ASSERT_EQ(Random(initial.data(), initial.size(), 0), static_cast<long>(initial.size()));
    errno = 0;
    EXPECT_EQ(Random(mapping, page * 2, 0), static_cast<long>(page));
    errno = 0;
    EXPECT_EQ(Random(static_cast<char*>(mapping) + page, 16, 0), -1);
    EXPECT_EQ(errno, EFAULT);
    EXPECT_EQ(munmap(mapping, page * 2), 0);
}

TEST(RandomDevice, ReadyRandomPollAndNonblockingReadAgree) {
    unsigned char byte;
    ASSERT_EQ(Random(&byte, 1, 0), 1);
    Fd fd("/dev/random");
    ASSERT_GE(fd.value, 0);
    pollfd event{fd.value, POLLIN | POLLOUT, 0};
    ASSERT_EQ(poll(&event, 1, 0), 1);
    EXPECT_NE(event.revents & POLLIN, 0);
    EXPECT_EQ(event.revents & POLLOUT, 0);
    ASSERT_EQ(fcntl(fd.value, F_SETFL, O_NONBLOCK), 0);
    EXPECT_EQ(read(fd.value, &byte, 1), 1);
    ASSERT_EQ(fcntl(fd.value, F_SETFL, 0), 0);
    EXPECT_EQ(read(fd.value, &byte, 1), 1);
}

TEST(RandomDevice, SharedDescriptorReadAndIoctlDoNotDeadlock) {
    EXPECT_EQ(ChildResult([] {
        Fd fd("/dev/random");
        if (fd.value < 0) return 1;
        pthread_t reader;
        if (pthread_create(&reader, nullptr, ReadSharedFd, &fd.value) != 0) return 2;
        for (int i = 0; i < 64; ++i) {
            int bits;
            if (ioctl(fd.value, RNDGETENTCNT, &bits) != 0) return 3;
        }
        void* result;
        if (pthread_join(reader, &result) != 0 || result != nullptr) return 4;
        return 0;
    }), 0);
}

TEST(RandomDevice, GuestPrivilegedSeedCreditReseedUsesInt32Abi) {
    if (!DragonOSGuest()) GTEST_SKIP() << "privileged pool mutations run only inside DragonOS";
    ASSERT_EQ(geteuid(), 0u);
    for (const char* path : {"/dev/random", "/dev/urandom"}) {
        Fd fd(path);
        ASSERT_GE(fd.value, 0);
        int negative = -1;
        errno = 0;
        EXPECT_EQ(ioctl(fd.value, RNDADDTOENTCNT, &negative), -1);
        EXPECT_EQ(errno, EINVAL);
        errno = 0;
        EXPECT_EQ(ioctl(fd.value, RNDADDTOENTCNT, reinterpret_cast<void*>(1)), -1);
        EXPECT_EQ(errno, EFAULT);
        std::array<unsigned char, 32> seed{};
        ASSERT_EQ(Random(seed.data(), seed.size(), 0), static_cast<long>(seed.size()));
        ASSERT_EQ(write(fd.value, seed.data(), seed.size()), static_cast<ssize_t>(seed.size()));
        // The agent passes a long pointer. Only its low native i32 is the ABI;
        // the following sentinel must not be read as a signed 64-bit count.
        struct { int32_t count; int32_t sentinel; } credit{256, -1};
        ASSERT_EQ(ioctl(fd.value, RNDADDTOENTCNT, &credit), 0);
        ASSERT_EQ(ioctl(fd.value, RNDRESEEDCRNG, 0), 0);
        int bits = -1;
        ASSERT_EQ(ioctl(fd.value, RNDGETENTCNT, &bits), 0);
        EXPECT_EQ(bits, 256);
        ASSERT_EQ(ioctl(fd.value, RNDZAPENTCNT, 0), 0);
        ASSERT_EQ(ioctl(fd.value, RNDCLEARPOOL, 0), 0);
        ASSERT_EQ(ioctl(fd.value, RNDGETENTCNT, &bits), 0);
        EXPECT_EQ(bits, 256);
        struct { int32_t count; int32_t size; unsigned char data[32]; } entropy{};
        entropy.count = 0;
        entropy.size = sizeof(entropy.data);
        ASSERT_EQ(ioctl(fd.value, RNDADDENTROPY, &entropy), 0);
        entropy.count = -1;
        errno = 0;
        EXPECT_EQ(ioctl(fd.value, RNDADDENTROPY, &entropy), -1);
        EXPECT_EQ(errno, EINVAL);
        entropy.count = 0;
        entropy.size = -1;
        errno = 0;
        EXPECT_EQ(ioctl(fd.value, RNDADDENTROPY, &entropy), -1);
        EXPECT_EQ(errno, EFAULT);
        entropy.size = 32;
        errno = 0;
        EXPECT_EQ(ioctl(fd.value, RNDADDENTROPY, reinterpret_cast<void*>(1)), -1);
        EXPECT_EQ(errno, EFAULT);

        const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
        void* boundary = mmap(nullptr, page * 2, PROT_READ | PROT_WRITE,
                              MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        ASSERT_NE(boundary, MAP_FAILED);
        auto* header = reinterpret_cast<int32_t*>(static_cast<char*>(boundary) + page - 8);
        header[0] = 0;
        header[1] = 32;
        ASSERT_EQ(mprotect(static_cast<char*>(boundary) + page, page, PROT_NONE), 0);
        errno = 0;
        EXPECT_EQ(ioctl(fd.value, RNDADDENTROPY, header), -1);
        EXPECT_EQ(errno, EFAULT);
        // The kernel must reject a negative first i32 before trying to read
        // the inaccessible second i32. Reading the whole header loses errno
        // precedence and violates the Linux ABI.
        header[1] = -1;
        errno = 0;
        EXPECT_EQ(ioctl(fd.value, RNDADDENTROPY, &header[1]), -1);
        EXPECT_EQ(errno, EINVAL);
        // A count ending exactly at the valid page boundary is still one i32.
        header[1] = 0;
        EXPECT_EQ(ioctl(fd.value, RNDADDTOENTCNT, &header[1]), 0);
        EXPECT_EQ(munmap(boundary, page * 2), 0);
    }
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
