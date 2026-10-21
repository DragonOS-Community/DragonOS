#include <gtest/gtest.h>

#include <atomic>
#include <cerrno>
#include <climits>
#include <csignal>
#include <cstring>
#include <cstdlib>
#include <fcntl.h>
#include <linux/futex.h>
#include <pthread.h>
#include <string>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <thread>
#include <unistd.h>

namespace {

long Futex(int* word, int op, int value, const void* fourth = nullptr,
           int* other = nullptr, int comparison = 0) {
    return syscall(SYS_futex, word, op, value, fourth, other, comparison);
}

long Requeue(int* source, int* target, int wake, int move, bool compare = false,
             int expected = 0) {
    return Futex(source, (compare ? FUTEX_CMP_REQUEUE : FUTEX_REQUEUE) |
                            FUTEX_PRIVATE_FLAG,
                 wake, reinterpret_cast<void*>(static_cast<intptr_t>(move)),
                 target, expected);
}

// Registration is observed through the operation's actual return value, never
// inferred from a sleep. Every waiter also has its own bounded timeout.
long AwaitRequeue(int* source, int* target, int count = 1) {
    for (int attempt = 0; attempt < 2000; ++attempt) {
        long result = Requeue(source, target, 0, count);
        if (result < 0 || result >= count) return result;
        usleep(1000);
    }
    return 0;
}

class WaitTask {
public:
    explicit WaitTask(int* word, long timeout_ns = 500000000)
        : word_(word), worker_([this, timeout_ns] {
              const timespec timeout = {0, timeout_ns};
              result_ = Futex(word_, FUTEX_WAIT_PRIVATE, 0, &timeout);
              error_ = errno;
          }) {}
    ~WaitTask() {
        // Cleanup both the original address and any explicitly selected target.
        Futex(word_, FUTEX_WAKE_PRIVATE, INT_MAX);
        if (target_) Futex(target_, FUTEX_WAKE_PRIVATE, INT_MAX);
        if (worker_.joinable()) worker_.join();
    }
    void Target(int* target) { target_ = target; }
    void Join() { worker_.join(); }
    int Result() const { return result_; }
    int Error() const { return error_; }
    pthread_t Handle() { return worker_.native_handle(); }

private:
    int* word_;
    int* target_ = nullptr;
    int result_ = -2;
    int error_ = 0;
    std::thread worker_;
};

TEST(FutexRegistration, FailedWaitHasLinuxErrors) {
    int words[512] = {};
    const timespec zero = {0, 0};
    for (int& word : words) {
        errno = 0;
        EXPECT_EQ(-1, Futex(&word, FUTEX_WAIT_PRIVATE, 1));
        EXPECT_EQ(EAGAIN, errno);
        errno = 0;
        EXPECT_EQ(-1, Futex(&word, FUTEX_WAIT_PRIVATE, 0, &zero));
        EXPECT_EQ(ETIMEDOUT, errno);
    }
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    void* inaccessible = mmap(nullptr, page, PROT_NONE,
                              MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, inaccessible);
    errno = 0;
    EXPECT_EQ(-1, Futex(static_cast<int*>(inaccessible), FUTEX_WAIT_PRIVATE, 0));
    EXPECT_EQ(EFAULT, errno);
    EXPECT_EQ(0, munmap(inaccessible, page));
}

TEST(FutexRegistration, ColdAnonymousComparisonRefaultsOutsideMap) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    for (int protection : {PROT_READ | PROT_WRITE, PROT_READ}) {
        for (bool compare : {false, true}) {
            SCOPED_TRACE(protection);
            SCOPED_TRACE(compare);
            // Do not touch the mapping before the syscall: the first value
            // load must take the real anonymous-page fault path.
            auto* word = static_cast<int*>(mmap(nullptr, page, protection,
                                                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
            ASSERT_NE(MAP_FAILED, word);
            errno = 0;
            const long result = compare ? Requeue(word, word, 0, 0, true, 1)
                                        : Futex(word, FUTEX_WAIT_PRIVATE, 1);
            EXPECT_EQ(-1, result);
            EXPECT_EQ(EAGAIN, errno);
            EXPECT_EQ(0, munmap(word, page));
        }
    }
}

TEST(FutexRegistration, InaccessibleComparisonReturnsEfault) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    auto* word = static_cast<int*>(mmap(nullptr, page, PROT_NONE,
                                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, word);
    errno = 0;
    EXPECT_EQ(-1, Futex(word, FUTEX_WAIT_PRIVATE, 0));
    EXPECT_EQ(EFAULT, errno);
    errno = 0;
    EXPECT_EQ(-1, Requeue(word, word, 0, 0, true, 0));
    EXPECT_EQ(EFAULT, errno);
    EXPECT_EQ(0, munmap(word, page));
}

TEST(FutexRegistration, EmptyRequeueReturnsZero) {
    int words[2] = {};
    EXPECT_EQ(0, Requeue(&words[0], &words[1], 1, 8));
    EXPECT_EQ(0, Requeue(&words[0], &words[0], 0, 8));
}

TEST(FutexRegistration, ComparePrecedesEmptySource) {
    int words[2] = {};
    errno = 0;
    EXPECT_EQ(-1, Requeue(&words[0], &words[1], 0, 1, true, 1));
    EXPECT_EQ(EAGAIN, errno);
    EXPECT_EQ(0, Requeue(&words[0], &words[1], 0, 1, true, 0));
}

TEST(FutexRegistration, RequeueCreatesFreshDestination) {
    int words[2] = {};
    WaitTask waiter(&words[0]);
    waiter.Target(&words[1]);
    ASSERT_EQ(1, AwaitRequeue(&words[0], &words[1]));
    EXPECT_EQ(0, Futex(&words[0], FUTEX_WAKE_PRIVATE, 1));
    EXPECT_EQ(1, Futex(&words[1], FUTEX_WAKE_PRIVATE, 1));
    waiter.Join();
    EXPECT_EQ(0, waiter.Result()) << waiter.Error();
}

TEST(FutexRegistration, SameKeyCountsWithoutMoving) {
    int word = 0;
    WaitTask waiter(&word);
    ASSERT_EQ(1, AwaitRequeue(&word, &word));
    EXPECT_EQ(1, Futex(&word, FUTEX_WAKE_PRIVATE, 1));
    waiter.Join();
    EXPECT_EQ(0, waiter.Result()) << waiter.Error();
}

TEST(FutexRegistration, RequeueReturnsWakeAndMoveCount) {
    int words[2] = {};
    WaitTask first(&words[0]), second(&words[0]);
    first.Target(&words[1]);
    second.Target(&words[1]);
    // Same-key requeue observes both registrations without changing their state.
    ASSERT_EQ(2, AwaitRequeue(&words[0], &words[0], 2));
    EXPECT_EQ(2, Requeue(&words[0], &words[1], 1, 1));
    EXPECT_EQ(1, Futex(&words[1], FUTEX_WAKE_PRIVATE, 1));
    first.Join();
    second.Join();
    EXPECT_EQ(0, first.Result()) << first.Error();
    EXPECT_EQ(0, second.Result()) << second.Error();
}

TEST(FutexRegistration, ChainedRequeueTimeoutUnregistersCurrentKey) {
    int words[3] = {};
    WaitTask waiter(&words[0], 300000000);
    waiter.Target(&words[2]);
    ASSERT_EQ(1, AwaitRequeue(&words[0], &words[1]));
    ASSERT_EQ(1, Requeue(&words[1], &words[2], 0, 1));
    waiter.Join();
    EXPECT_EQ(-1, waiter.Result());
    EXPECT_EQ(ETIMEDOUT, waiter.Error());
    for (int& word : words) EXPECT_EQ(0, Futex(&word, FUTEX_WAKE_PRIVATE, 1));
}

void SignalHandler(int) {}

TEST(FutexRegistration, RequeueSignalUnregistersCurrentKey) {
    struct sigaction action = {}, old = {};
    action.sa_handler = SignalHandler;
    sigemptyset(&action.sa_mask);
    ASSERT_EQ(0, sigaction(SIGUSR1, &action, &old));
    int words[2] = {};
    WaitTask waiter(&words[0]);
    waiter.Target(&words[1]);
    const long moved = AwaitRequeue(&words[0], &words[1]);
    if (moved == 1) { EXPECT_EQ(0, pthread_kill(waiter.Handle(), SIGUSR1)); }
    waiter.Join();
    EXPECT_EQ(1, moved);
    EXPECT_EQ(-1, waiter.Result());
    EXPECT_EQ(EINTR, waiter.Error());
    EXPECT_EQ(0, Futex(&words[1], FUTEX_WAKE_PRIVATE, 1));
    EXPECT_EQ(0, sigaction(SIGUSR1, &old, nullptr));
}

class FutexFileIdentity : public ::testing::Test {
protected:
    void SetUp() override {
        char pattern[] = "/tmp/dunitest_futex_file_XXXXXX";
        char* path = mkdtemp(pattern);
        ASSERT_NE(nullptr, path);
        directory_ = path;
        ASSERT_EQ(0, mount("none", directory_.c_str(), "ramfs", 0, nullptr));
        mounted_ = true;
    }
    void TearDown() override {
        if (mounted_) { EXPECT_EQ(0, umount2(directory_.c_str(), MNT_DETACH)); }
        if (!directory_.empty()) { EXPECT_EQ(0, rmdir(directory_.c_str())); }
    }
    int Create(const std::string& name, size_t bytes) {
        const int fd = open((directory_ + "/" + name).c_str(), O_CREAT | O_RDWR, 0600);
        if (fd >= 0 && ftruncate(fd, bytes) != 0) { close(fd); return -1; }
        return fd;
    }
    std::string directory_;
    bool mounted_ = false;
};

// Child's real blocked shared WAIT must match independently opened mapping
// aliases. A different page or another file must never wake that child.
void CheckSharedWake(int* waiting, int* matching, int* nonmatching = nullptr) {
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        const timespec timeout = {2, 0};
        const long result = Futex(waiting, FUTEX_WAIT, 0, &timeout);
        _exit(result == 0 ? 0 : 71);
    }
    long woke = 0;
    for (int attempt = 0; attempt < 1000 && woke == 0; ++attempt) {
        if (nonmatching) { EXPECT_EQ(0, Futex(nonmatching, FUTEX_WAKE, 1)); }
        woke = Futex(matching, FUTEX_WAKE, 1);
        if (woke == 0) usleep(1000);
    }
    EXPECT_EQ(1, woke);
    int status = 0;
    EXPECT_EQ(child, waitpid(child, &status, 0));
    EXPECT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST_F(FutexFileIdentity, ColdReadonlyFileComparisonNeedsNoWriteAccess) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    const int fd = Create("readonly", page);
    ASSERT_GE(fd, 0);
    for (int private_flag : {0, FUTEX_PRIVATE_FLAG}) {
        for (bool compare : {false, true}) {
            SCOPED_TRACE(private_flag);
            SCOPED_TRACE(compare);
            auto* word = static_cast<int*>(mmap(nullptr, page, PROT_READ,
                                                MAP_SHARED, fd, 0));
            ASSERT_NE(MAP_FAILED, word);
            errno = 0;
            const long result = compare
                ? Futex(word, FUTEX_CMP_REQUEUE | private_flag, 0, nullptr, word, 1)
                : Futex(word, FUTEX_WAIT | private_flag, 1);
            EXPECT_EQ(-1, result);
            EXPECT_EQ(EAGAIN, errno);
            EXPECT_EQ(0, munmap(word, page));
        }
    }
    EXPECT_EQ(0, close(fd));
}

TEST_F(FutexFileIdentity, IndependentOpensHardlinkAndOffsets) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    const int original = Create("original", 2 * page);
    ASSERT_GE(original, 0);
    const std::string alias_path = directory_ + "/alias";
    ASSERT_EQ(0, link((directory_ + "/original").c_str(), alias_path.c_str()));
    const int alias = open(alias_path.c_str(), O_RDWR);
    ASSERT_GE(alias, 0);
    auto* first = static_cast<int*>(mmap(nullptr, 2 * page, PROT_READ | PROT_WRITE,
                                       MAP_SHARED, original, 0));
    auto* second = static_cast<int*>(mmap(nullptr, 2 * page, PROT_READ | PROT_WRITE,
                                        MAP_SHARED, alias, 0));
    ASSERT_NE(MAP_FAILED, first);
    ASSERT_NE(MAP_FAILED, second);
    *first = 0;
    CheckSharedWake(first, second, reinterpret_cast<int*>(reinterpret_cast<char*>(second) + page));
    EXPECT_EQ(0, rename((directory_ + "/original").c_str(), (directory_ + "/renamed").c_str()));
    EXPECT_EQ(0, unlink(alias_path.c_str()));
    CheckSharedWake(first, second);
    EXPECT_EQ(0, munmap(first, 2 * page));
    EXPECT_EQ(0, munmap(second, 2 * page));
    EXPECT_EQ(0, close(original));
    EXPECT_EQ(0, close(alias));
}

TEST_F(FutexFileIdentity, BindMountAliasesMatch) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    const std::string base = directory_ + "/base", view = directory_ + "/view";
    ASSERT_EQ(0, mkdir(base.c_str(), 0700));
    ASSERT_EQ(0, mkdir(view.c_str(), 0700));
    const int original = open((base + "/file").c_str(), O_CREAT | O_RDWR, 0600);
    ASSERT_GE(original, 0);
    ASSERT_EQ(0, ftruncate(original, page));
    ASSERT_EQ(0, mount(base.c_str(), view.c_str(), nullptr, MS_BIND, nullptr));
    const int alias = open((view + "/file").c_str(), O_RDWR);
    ASSERT_GE(alias, 0);
    auto* first = static_cast<int*>(mmap(nullptr, page, PROT_READ | PROT_WRITE,
                                       MAP_SHARED, original, 0));
    auto* second = static_cast<int*>(mmap(nullptr, page, PROT_READ | PROT_WRITE,
                                        MAP_SHARED, alias, 0));
    ASSERT_NE(MAP_FAILED, first);
    ASSERT_NE(MAP_FAILED, second);
    *first = 0;
    CheckSharedWake(first, second);
    EXPECT_EQ(0, munmap(first, page));
    EXPECT_EQ(0, munmap(second, page));
    EXPECT_EQ(0, close(original));
    EXPECT_EQ(0, close(alias));
    EXPECT_EQ(0, umount2(view.c_str(), MNT_DETACH));
}

TEST_F(FutexFileIdentity, DifferentMountsDoNotAlias) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    const std::string other = directory_ + "/other";
    ASSERT_EQ(0, mkdir(other.c_str(), 0700));
    ASSERT_EQ(0, mount("none", other.c_str(), "ramfs", 0, nullptr));
    const int original = Create("file", page);
    const int foreign = open((other + "/file").c_str(), O_CREAT | O_RDWR, 0600);
    ASSERT_GE(original, 0);
    ASSERT_GE(foreign, 0);
    ASSERT_EQ(0, ftruncate(foreign, page));
    auto* first = static_cast<int*>(mmap(nullptr, page, PROT_READ | PROT_WRITE,
                                       MAP_SHARED, original, 0));
    auto* second = static_cast<int*>(mmap(nullptr, page, PROT_READ | PROT_WRITE,
                                        MAP_SHARED, foreign, 0));
    ASSERT_NE(MAP_FAILED, first);
    ASSERT_NE(MAP_FAILED, second);
    *first = 0;
    *second = 0;
    CheckSharedWake(first, first, second);
    EXPECT_EQ(0, munmap(first, page));
    EXPECT_EQ(0, munmap(second, page));
    EXPECT_EQ(0, close(original));
    EXPECT_EQ(0, close(foreign));
    EXPECT_EQ(0, umount2(other.c_str(), MNT_DETACH));
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
