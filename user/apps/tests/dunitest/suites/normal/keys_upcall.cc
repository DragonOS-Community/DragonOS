#include <gtest/gtest.h>

#include <fcntl.h>
#include <linux/keyctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <climits>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

namespace {

constexpr const char* kHelperPath = "/sbin/request-key";
constexpr const char* kOptIn = "DRAGONOS_KEYRING_UPCALL_TEST";
constexpr const char* kPayload = "upcall-payload";
// EPROTO is a valid key rejection errno but not the usual ENOKEY fallback.
constexpr int kRejectedErrno = EPROTO;

long Keyctl(int command, unsigned long arg2 = 0, unsigned long arg3 = 0,
            unsigned long arg4 = 0, unsigned long arg5 = 0) {
    return syscall(SYS_keyctl, command, arg2, arg3, arg4, arg5);
}

long RequestKey(const char* description, const char* callout, int destination) {
    return syscall(SYS_request_key, "user", description, callout, destination);
}

long AddRing(const char* description, int destination) {
    return syscall(SYS_add_key, "keyring", description, nullptr, 0, destination);
}

unsigned long Arg(long value) {
    return static_cast<unsigned long>(value);
}

unsigned long Ptr(const void* value) {
    return reinterpret_cast<unsigned long>(value);
}

std::string AbsoluteExecutable(const char* argv0) {
    if (argv0[0] == '/') return argv0;
    std::array<char, PATH_MAX> cwd{};
    if (!getcwd(cwd.data(), cwd.size())) return {};
    return std::string(cwd.data()) + "/" + argv0;
}

std::string executable;

// The helper is installed only by an explicit guest opt-in.  Before removal,
// check inode and target so the test never deletes a preexisting or replaced
// /sbin/request-key.
class ScopedHelperLink {
public:
    enum class CreateResult { Created, Exists, Error };

    CreateResult Create() {
        struct stat old{};
        if (lstat(kHelperPath, &old) == 0) return CreateResult::Exists;
        if (errno != ENOENT) return CreateResult::Error;
        if (symlink(executable.c_str(), kHelperPath) != 0)
            return errno == EEXIST ? CreateResult::Exists : CreateResult::Error;
        created_link_ = true;
        if (lstat(kHelperPath, &created_) != 0) return CreateResult::Error;
        has_identity_ = true;
        return CreateResult::Created;
    }

    ~ScopedHelperLink() {
        if (!created_link_) return;
        struct stat current{};
        if (lstat(kHelperPath, &current) != 0 || !S_ISLNK(current.st_mode) ||
            (has_identity_ &&
             (current.st_dev != created_.st_dev ||
              current.st_ino != created_.st_ino)))
            return;
        std::array<char, PATH_MAX> target{};
        const ssize_t length = readlink(kHelperPath, target.data(), target.size());
        if (length == static_cast<ssize_t>(executable.size()) &&
            std::memcmp(target.data(), executable.data(), executable.size()) == 0)
            unlink(kHelperPath);
    }

private:
    struct stat created_{};
    bool created_link_ = false;
    bool has_identity_ = false;
};

class ScopedCounter {
public:
    bool Create() {
        char pattern[] = "/tmp/keys-upcall-XXXXXX";
        const int fd = mkstemp(pattern);
        if (fd < 0) return false;
        path_ = pattern;
        close(fd);
        return true;
    }

    ~ScopedCounter() {
        if (!path_.empty()) unlink(path_.c_str());
    }

    const std::string& path() const { return path_; }

    off_t Count() const {
        struct stat st{};
        return stat(path_.c_str(), &st) == 0 ? st.st_size : -1;
    }

private:
    std::string path_;
};

int AppendCounter(const std::string& path) {
    const int fd = open(path.c_str(), O_WRONLY | O_APPEND | O_NOFOLLOW);
    if (fd < 0) return 1;
    const ssize_t written = write(fd, "1", 1);
    close(fd);
    return written == 1 ? 0 : 2;
}

int RunHelper(int argc, char** argv) {
    if (argc < 8) return 10;
    char* end = nullptr;
    const long serial = strtol(argv[2], &end, 10);
    if (end == argv[2] || *end != '\0' || serial <= 0 || serial > INT_MAX)
        return 11;
    if (Keyctl(KEYCTL_ASSUME_AUTHORITY, Arg(serial)) <= 0) return 12;

    std::array<char, 4096> buffer{};
    const long count = Keyctl(KEYCTL_READ, Arg(KEY_SPEC_REQKEY_AUTH_KEY),
                              Ptr(buffer.data()), buffer.size());
    if (count < 0 || count > static_cast<long>(buffer.size())) return 13;
    const std::string callout(buffer.data(), static_cast<size_t>(count));
    if (callout == "success") {
        return Keyctl(KEYCTL_INSTANTIATE, Arg(serial), Ptr(kPayload),
                      strlen(kPayload), 0) == 0 ? 0 : 14;
    }
    if (callout == "reject") {
        return Keyctl(KEYCTL_REJECT, Arg(serial), 60, kRejectedErrno, 0) == 0
                   ? 0 : 15;
    }
    if (callout == "expire-before-completion") {
        if (Keyctl(KEYCTL_SET_TIMEOUT, Arg(serial), 1) != 0) return 20;
        sleep(2);
        return Keyctl(KEYCTL_INSTANTIATE, Arg(serial), Ptr(kPayload),
                      strlen(kPayload), 0) == 0 ? 0 : 21;
    }
    const size_t separator = callout.find(':');
    if (separator == std::string::npos) return 16;
    const std::string mode = callout.substr(0, separator);
    const std::string marker = callout.substr(separator + 1);
    if (AppendCounter(marker) != 0) return 17;
    if (mode == "exit") return 42;  // Supervisor must finish the key negatively.
    if (mode != "singleflight") return 18;
    usleep(300000);
    return Keyctl(KEYCTL_INSTANTIATE, Arg(serial), Ptr(kPayload),
                  strlen(kPayload), 0) == 0 ? 0 : 19;
}

int WaitForChild(pid_t child) {
    int status = 0;
    pid_t waited;
    do {
        waited = waitpid(child, &status, 0);
    } while (waited < 0 && errno == EINTR);
    if (waited != child || !WIFEXITED(status)) return -1;
    return WEXITSTATUS(status);
}

bool ReadExactly(int fd, void* buffer, size_t size) {
    auto* bytes = static_cast<char*>(buffer);
    while (size) {
        const ssize_t n = read(fd, bytes, size);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return false;
        bytes += n;
        size -= n;
    }
    return true;
}

class KeysUpcallTest : public ::testing::Test {
protected:
    int session_ = -1;
    ScopedHelperLink helper_;

    void SetUp() override {
        const char* opt_in = getenv(kOptIn);
        if (!opt_in || strcmp(opt_in, "1") != 0)
            GTEST_SKIP() << "explicit guest opt-in required";
        ASSERT_FALSE(executable.empty()) << "cannot resolve test executable";
        const ScopedHelperLink::CreateResult result = helper_.Create();
        if (result == ScopedHelperLink::CreateResult::Exists)
            GTEST_SKIP() << "/sbin/request-key already exists";
        ASSERT_EQ(ScopedHelperLink::CreateResult::Created, result)
            << "cannot install temporary helper: " << strerror(errno);
        const long serial = Keyctl(KEYCTL_JOIN_SESSION_KEYRING);
        ASSERT_GT(serial, 0) << strerror(errno);
        session_ = static_cast<int>(serial);
    }
};

TEST_F(KeysUpcallTest, HelperReadsAuthorizationAndInstantiatesUserKey) {
    const long key = RequestKey("dunitest-upcall-success", "success", session_);
    ASSERT_GT(key, 0) << strerror(errno);
    std::array<char, 32> payload{};
    EXPECT_EQ(static_cast<long>(strlen(kPayload)),
              Keyctl(KEYCTL_READ, Arg(key), Ptr(payload.data()), payload.size()));
    EXPECT_EQ(0, std::memcmp(payload.data(), kPayload, strlen(kPayload)));
    EXPECT_EQ(key, RequestKey("dunitest-upcall-success", nullptr, session_));
}

TEST_F(KeysUpcallTest, ExistingKeyIsLinkedToExplicitDestination) {
    const long first_ring = AddRing("dunitest-upcall-ring-a", session_);
    const long second_ring = AddRing("dunitest-upcall-ring-b", session_);
    ASSERT_GT(first_ring, 0) << strerror(errno);
    ASSERT_GT(second_ring, 0) << strerror(errno);
    const long key = RequestKey("dunitest-upcall-relocate", "success",
                                static_cast<int>(first_ring));
    ASSERT_GT(key, 0) << strerror(errno);
    EXPECT_EQ(key, RequestKey("dunitest-upcall-relocate", nullptr,
                              static_cast<int>(second_ring)));
    EXPECT_EQ(key, Keyctl(KEYCTL_SEARCH, Arg(second_ring), Ptr("user"),
                          Ptr("dunitest-upcall-relocate"), 0));
}

TEST_F(KeysUpcallTest, RejectedKeyPreservesOriginalErrno) {
    errno = 0;
    EXPECT_EQ(-1, RequestKey("dunitest-upcall-reject", "reject", session_));
    EXPECT_EQ(kRejectedErrno, errno);
    errno = 0;
    EXPECT_EQ(-1, RequestKey("dunitest-upcall-reject", nullptr, session_));
    EXPECT_EQ(kRejectedErrno, errno);
}

TEST_F(KeysUpcallTest, ExpiredPendingKeyCompletesAndWakesRequester) {
    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        alarm(8);  // A lost construction wakeup must fail, not hang the suite.
        errno = 0;
        const long result = RequestKey("dunitest-upcall-expired",
                                       "expire-before-completion", session_);
        if (result != -1 || errno != EKEYEXPIRED)
            fprintf(stderr, "expired request: result=%ld errno=%d\n", result,
                    errno);
        _exit(result == -1 && errno == EKEYEXPIRED ? 0 : 1);
    }
    EXPECT_EQ(0, WaitForChild(child));
}

TEST_F(KeysUpcallTest, HelperExitCompletesNegativeKeyAndCachesIt) {
    ScopedCounter counter;
    ASSERT_TRUE(counter.Create()) << strerror(errno);
    const std::string callout = "exit:" + counter.path();
    errno = 0;
    EXPECT_EQ(-1, RequestKey("dunitest-upcall-exit", callout.c_str(), session_));
    EXPECT_EQ(ENOKEY, errno);
    EXPECT_EQ(1, counter.Count());
    errno = 0;
    EXPECT_EQ(-1, RequestKey("dunitest-upcall-exit", callout.c_str(), session_));
    EXPECT_EQ(ENOKEY, errno);
    EXPECT_EQ(1, counter.Count());
}

TEST_F(KeysUpcallTest, ConcurrentRequestersShareOneConstruction) {
    ScopedCounter counter;
    ASSERT_TRUE(counter.Create()) << strerror(errno);
    const std::string callout = "singleflight:" + counter.path();
    int start_pipe[2] = {-1, -1};
    int result_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(start_pipe)) << strerror(errno);
    if (pipe(result_pipe) != 0) {
        const int saved_errno = errno;
        close(start_pipe[0]);
        close(start_pipe[1]);
        FAIL() << "result pipe: " << strerror(saved_errno);
    }
    constexpr size_t kChildren = 3;
    std::vector<pid_t> children;
    for (size_t i = 0; i < kChildren; ++i) {
        const pid_t child = fork();
        if (child < 0) {
            const int saved_errno = errno;
            close(start_pipe[0]);
            close(start_pipe[1]);  // Blocked children observe EOF.
            close(result_pipe[0]);
            close(result_pipe[1]);
            for (pid_t started : children) WaitForChild(started);
            FAIL() << "fork: " << strerror(saved_errno);
        }
        if (child == 0) {
            close(start_pipe[1]);
            close(result_pipe[0]);
            char start;
            if (!ReadExactly(start_pipe[0], &start, 1)) _exit(10);
            const long key = RequestKey("dunitest-upcall-race", callout.c_str(),
                                        session_);
            const long observed = key >= 0 ? key : -errno;
            if (write(result_pipe[1], &observed, sizeof(observed)) !=
                sizeof(observed))
                _exit(11);
            _exit(0);
        }
        children.push_back(child);
    }
    close(start_pipe[0]);
    close(result_pipe[1]);
    const char starts[kChildren] = {'1', '1', '1'};
    const ssize_t released = write(start_pipe[1], starts, sizeof(starts));
    close(start_pipe[1]);
    if (released != static_cast<ssize_t>(sizeof(starts))) {
        close(result_pipe[0]);
        for (pid_t child : children) WaitForChild(child);
        FAIL() << "failed to release all requesters";
    }
    long first = -1;
    bool received_all = true;
    for (size_t i = 0; i < kChildren; ++i) {
        long result = -1;
        if (!ReadExactly(result_pipe[0], &result, sizeof(result))) {
            received_all = false;
            break;
        }
        EXPECT_GT(result, 0) << "request_key errno " << -result;
        if (i == 0) first = result;
        EXPECT_EQ(first, result);
    }
    close(result_pipe[0]);
    for (pid_t child : children) EXPECT_EQ(0, WaitForChild(child));
    ASSERT_TRUE(received_all);
    EXPECT_EQ(1, counter.Count());
}

}  // namespace

int main(int argc, char** argv) {
    if (argc >= 2 && strcmp(argv[1], "create") == 0)
        return RunHelper(argc, argv);
    executable = AbsoluteExecutable(argv[0]);
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
