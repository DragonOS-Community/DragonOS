#include <gtest/gtest.h>

#include "cap_common.h"

#include <cerrno>
#include <climits>
#include <csignal>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <fcntl.h>
#include <functional>
#include <limits>
#include <sched.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

namespace {

constexpr uint32_t kSysTime = 1U << 25;
constexpr int64_t kSettodSecMax = std::numeric_limits<int64_t>::max() / 1000000000 -
                                  int64_t(30) * 365 * 24 * 60 * 60;

int64_t MonoNow() {
    timespec now = {};
    clock_gettime(CLOCK_MONOTONIC, &now);
    return int64_t(now.tv_sec) * 1000000000 + now.tv_nsec;
}

bool InChild(const std::function<void()>& body, bool* skipped = nullptr) {
    fflush(nullptr);
    const pid_t child = fork();
    if (child < 0) return false;
    if (child == 0) {
        alarm(3);
        body();
        fflush(nullptr);
        _exit(::testing::Test::HasFailure() ? 1 : 0);
    }
    const int64_t deadline = MonoNow() + 5000000000;
    int status = 0;
    bool done = false;
    while (MonoNow() < deadline) {
        if (waitpid(child, &status, WNOHANG) == child) {
            done = true;
            break;
        }
        usleep(1000);
    }
    if (!done) {
        kill(child, SIGKILL);
        while (waitpid(child, &status, 0) < 0 && errno == EINTR) {}
    }
    if (done && WIFEXITED(status) && WEXITSTATUS(status) == 77 && skipped) {
        *skipped = true;
        return true;
    }
    return done && WIFEXITED(status) && WEXITSTATUS(status) == 0;
}

void ExpectError(clockid_t clock, const timespec* address, int expected) {
    errno = 0;
    const long result = syscall(SYS_clock_settime, clock, address);
    const int error = errno;
    EXPECT_EQ(-1, result) << "clock=" << clock;
    EXPECT_EQ(expected, error) << "clock=" << clock;
}

clockid_t ThreadCpuClock(unsigned kind) {
    return static_cast<clockid_t>((~uint32_t(0) << 3) | 4U | kind);
}

TEST(ClockSettime, StaticUnsettableClocksRejectBeforeUserCopy) {
    const timespec invalid = {-1, -1};
    const auto* inaccessible = reinterpret_cast<const timespec*>(uintptr_t(1));
    // MONOTONIC, static CPU aliases, RAW/COARSE, BOOTTIME, ALARM and TAI
    // clocks have no Linux clock_set callback. Unknown IDs do not either.
    for (clockid_t clock : {1, 2, 3, 4, 5, 6, 7, 8, 9, 11, INT_MAX}) {
        SCOPED_TRACE(clock);
        ExpectError(clock, nullptr, EINVAL);
        ExpectError(clock, inaccessible, EINVAL);
        ExpectError(clock, &invalid, EINVAL);
    }
}

TEST(ClockSettime, RealtimeCopiesEntireTimespecBeforeValidationOrPermission) {
    ExpectError(CLOCK_REALTIME, nullptr, EFAULT);
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    void* mapping = mmap(nullptr, 2 * page, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping);
    ASSERT_EQ(0, mprotect(static_cast<char*>(mapping) + page, page, PROT_NONE));
    ExpectError(CLOCK_REALTIME,
                reinterpret_cast<const timespec*>(static_cast<char*>(mapping) + page),
                EFAULT);
    // tv_sec is readable and invalid, but the inaccessible tv_nsec must make
    // the complete protected copy fail before formatted-value validation.
    auto* seconds = reinterpret_cast<int64_t*>(static_cast<char*>(mapping) + page - 8);
    *seconds = -1;
    ExpectError(CLOCK_REALTIME, reinterpret_cast<const timespec*>(seconds), EFAULT);
    EXPECT_EQ(0, munmap(mapping, 2 * page));
}

TEST(ClockSettime, RealtimeRejectsInvalidFormattedValuesBeforePermission) {
    for (const timespec value : {
             timespec{-1, 0}, timespec{0, -1}, timespec{0, 1000000000},
             timespec{kSettodSecMax, 0},
             timespec{std::numeric_limits<int64_t>::max(), 0}}) {
        ExpectError(CLOCK_REALTIME, &value, EINVAL);
    }
}

TEST(ClockSettime, EffectiveSysTimeCapabilityIsRequiredInInitialUserNamespace) {
    ASSERT_TRUE(InChild([] {
        cap_user_data_t caps[2] = {};
        ASSERT_EQ(0, capget_errno(_LINUX_CAPABILITY_VERSION_3, 0, caps));
        caps[0].effective &= ~kSysTime;
        ASSERT_EQ(0, capset_errno(_LINUX_CAPABILITY_VERSION_3, 0, caps));
        ASSERT_EQ(0, capget_errno(_LINUX_CAPABILITY_VERSION_3, 0, caps));
        ASSERT_EQ(0u, caps[0].effective & kSysTime);
        // Valid formatting: permission precedes REALTIME >= MONOTONIC. Zero
        // also cannot change the host clock even if permission checking regresses.
        const timespec valid = {0, 0};
        const timespec invalid = {-1, 0};
        ExpectError(CLOCK_REALTIME, &valid, EPERM);
        ExpectError(CLOCK_REALTIME, &invalid, EINVAL);
        ExpectError(CLOCK_REALTIME, nullptr, EFAULT);
    }));
}

TEST(ClockSettime, ChildUserNamespaceRootCannotSetTheGlobalClock) {
    bool skipped = false;
    const bool passed = InChild([] {
        const uid_t outside_uid = geteuid();
        if (unshare(CLONE_NEWUSER)) {
            if (errno == EPERM || errno == EACCES || errno == ENOSYS || errno == EINVAL) {
                _exit(77);
            }
            FAIL() << "unshare(CLONE_NEWUSER): " << strerror(errno);
        }
        char mapping[80];
        const int length = snprintf(mapping, sizeof(mapping), "0 %u 1\n", outside_uid);
        const int fd = open("/proc/self/uid_map", O_WRONLY | O_CLOEXEC);
        ASSERT_GE(fd, 0);
        EXPECT_EQ(length, write(fd, mapping, length));
        EXPECT_EQ(0, close(fd));
        ASSERT_EQ(uid_t(0), geteuid());
        cap_user_data_t caps[2] = {};
        ASSERT_EQ(0, capget_errno(_LINUX_CAPABILITY_VERSION_3, 0, caps));
        ASSERT_NE(0u, caps[0].effective & kSysTime);
        const timespec valid = {0, 0};
        const timespec invalid = {0, 1000000000};
        ExpectError(CLOCK_REALTIME, &valid, EPERM);
        ExpectError(CLOCK_REALTIME, &invalid, EINVAL);
        ExpectError(CLOCK_REALTIME, nullptr, EFAULT);
    }, &skipped);
    if (skipped) {
        GTEST_SKIP() << "user namespace creation unavailable or not permitted";
    }
    EXPECT_TRUE(passed);
}

TEST(ClockSettime, DynamicCpuClockKeepsTargetAndCopySemantics) {
    const timespec invalid = {-1, -1};
    for (unsigned kind : {0U, 1U, 2U}) {
        // Dynamic CPU clocks have a setter callback that validates the target
        // and returns EPERM even when the copied timespec is not normalized.
        ExpectError(ThreadCpuClock(kind), &invalid, EPERM);
        ExpectError(ThreadCpuClock(kind), nullptr, EFAULT);
    }
    ExpectError(CLOCK_PROCESS_CPUTIME_ID, nullptr, EINVAL);
    ExpectError(CLOCK_THREAD_CPUTIME_ID, &invalid, EINVAL);
}

} // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
