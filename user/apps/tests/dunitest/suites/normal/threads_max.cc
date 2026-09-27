#include <gtest/gtest.h>

#include <cerrno>
#include <climits>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <pthread.h>
#include <sys/stat.h>
#include <sys/fsuid.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>
#include <array>

namespace {

constexpr char kPath[] = "/proc/sys/kernel/threads-max";

long read_limit() {
    int fd = open(kPath, O_RDONLY);
    if (fd < 0) return -1;
    char buf[64] = {};
    ssize_t count = read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (count <= 0) return -1;
    char* end = nullptr;
    long value = strtol(buf, &end, 10);
    return end == buf || value < 1 || value > 0x3fffffff ? -1 : value;
}

ssize_t write_limit(const char* text) {
    int fd = open(kPath, O_WRONLY);
    if (fd < 0) return -1;
    ssize_t result = write(fd, text, strlen(text));
    int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return result;
}

class RestoreLimit {
public:
    RestoreLimit() : original_(read_limit()) {}
    ~RestoreLimit() {
        if (original_ > 0) {
            const std::string value = std::to_string(original_) + "\n";
            write_limit(value.c_str());
        }
    }
    long original() const { return original_; }

private:
    long original_;
};

long current_threads() {
    FILE* file = fopen("/proc/loadavg", "r");
    if (!file) return -1;
    char line[128] = {};
    const bool read_ok = fgets(line, sizeof(line), file) != nullptr;
    fclose(file);
    unsigned runnable = 0;
    unsigned total = 0;
    if (!read_ok || sscanf(line, "%*s %*s %*s %u/%u", &runnable, &total) != 2) return -1;
    return total;
}

class HeldThreads {
public:
    HeldThreads() {
        pthread_mutex_init(&mutex_, nullptr);
        pthread_cond_init(&condition_, nullptr);
        for (auto& thread : threads_) {
            if (pthread_create(&thread, nullptr, &HeldThreads::wait, this) != 0) break;
            ++started_;
        }
    }

    ~HeldThreads() {
        pthread_mutex_lock(&mutex_);
        release_ = true;
        pthread_cond_broadcast(&condition_);
        pthread_mutex_unlock(&mutex_);
        for (size_t i = 0; i < started_; ++i) pthread_join(threads_[i], nullptr);
        pthread_cond_destroy(&condition_);
        pthread_mutex_destroy(&mutex_);
    }

    size_t started() const { return started_; }

private:
    static void* wait(void* arg) {
        auto* self = static_cast<HeldThreads*>(arg);
        pthread_mutex_lock(&self->mutex_);
        while (!self->release_) pthread_cond_wait(&self->condition_, &self->mutex_);
        pthread_mutex_unlock(&self->mutex_);
        return nullptr;
    }

    std::array<pthread_t, 8> threads_ = {};
    size_t started_ = 0;
    pthread_mutex_t mutex_ = {};
    pthread_cond_t condition_ = {};
    bool release_ = false;
};

TEST(ThreadsMax, ExposedAsGlobalNumericSysctl) {
    struct stat st = {};
    ASSERT_EQ(0, stat(kPath, &st));
    EXPECT_TRUE(S_ISREG(st.st_mode));
    EXPECT_EQ(0644u, static_cast<unsigned>(st.st_mode & 0777));
    ASSERT_GT(read_limit(), 0);

    int fd = open(kPath, O_RDONLY);
    ASSERT_GE(fd, 0);
    char c = 0;
    EXPECT_EQ(1, read(fd, &c, 1));
    EXPECT_EQ(0, read(fd, &c, 1));
    close(fd);
}

TEST(ThreadsMax, ParsesAndEnforcesBounds) {
    RestoreLimit restore;
    ASSERT_GT(restore.original(), 0);
    const long safe = current_threads() + 128;
    ASSERT_GT(safe, 128);
    char hex[64] = {};
    char octal[64] = {};
    snprintf(hex, sizeof(hex), "0x%lx\n", safe);
    snprintf(octal, sizeof(octal), "0%lo\n", safe);
    ASSERT_EQ(static_cast<ssize_t>(strlen(hex)), write_limit(hex));
    EXPECT_EQ(safe, read_limit());
    ASSERT_EQ(static_cast<ssize_t>(strlen(octal)), write_limit(octal));
    EXPECT_EQ(safe, read_limit());
    const std::string decimal = std::to_string(safe);
    const std::string two_values = decimal + " " + decimal;
    EXPECT_EQ(static_cast<ssize_t>(decimal.size() + 1), write_limit(two_values.c_str()));
    EXPECT_EQ(safe, read_limit());

    for (const char* invalid : {"0\n", "-1\n", "1073741824\n", "bad\n"}) {
        errno = 0;
        EXPECT_EQ(-1, write_limit(invalid));
        EXPECT_EQ(EINVAL, errno);
        EXPECT_EQ(safe, read_limit());
    }
    const std::string page_boundary(4095, ' ');
    errno = 0;
    EXPECT_EQ(-1, write_limit((page_boundary + "8").c_str()));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(safe, read_limit());
    const std::string long_write = decimal + " " + std::string(4095, 'x');
    EXPECT_EQ(static_cast<ssize_t>(2 * (decimal.size() + 1)), write_limit(long_write.c_str()));
    EXPECT_EQ(safe, read_limit());

    int fd = open(kPath, O_WRONLY);
    ASSERT_GE(fd, 0);
    EXPECT_EQ(0, write(fd, "", 0));
    ASSERT_EQ(1, lseek(fd, 1, SEEK_SET));
    EXPECT_EQ(2, write(fd, "2\n", 2));
    EXPECT_EQ(safe, read_limit());
    close(fd);
}

TEST(ThreadsMax, StopsNewTasksWithoutKillingExistingTasks) {
    HeldThreads held;
    ASSERT_EQ(8u, held.started());
    RestoreLimit restore;
    ASSERT_GT(restore.original(), 0);
    const long current = current_threads();
    ASSERT_GT(current, 8);
    const long threshold = current - 4;
    const std::string limit = std::to_string(threshold) + "\n";
    ASSERT_EQ(static_cast<ssize_t>(limit.size()), write_limit(limit.c_str()));
    errno = 0;
    pid_t child = fork();
    if (child == 0) _exit(99);
    if (child > 0) waitpid(child, nullptr, 0);
    EXPECT_EQ(-1, child);
    EXPECT_EQ(EAGAIN, errno);
    pthread_t thread;
    const int result = pthread_create(&thread, nullptr, [](void*) -> void* { return nullptr; }, nullptr);
    if (result == 0) pthread_join(thread, nullptr);
    EXPECT_EQ(EAGAIN, result);
    EXPECT_EQ(threshold, read_limit());
}

TEST(ThreadsMax, NonRootCannotChangeGlobalLimit) {
    pid_t child = fork();
    if (child == 0) {
        if (setuid(65534) != 0) _exit(2);
        errno = 0;
        const int fd = open(kPath, O_WRONLY);
        if (fd >= 0) close(fd);
        _exit(fd == -1 && errno == EACCES ? 0 : 3);
    }
    ASSERT_GT(child, 0);
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(ThreadsMax, EffectiveRootCanOpenDespiteNonRootFsuid) {
    pid_t child = fork();
    if (child == 0) {
        if (setresuid(65534, 0, 0) != 0) _exit(2);
        if (setfsuid(65534) != 0) _exit(2);
        if (setfsuid(-1) != 65534) _exit(2);
        const int fd = open(kPath, O_WRONLY);
        if (fd < 0) _exit(3);
        const bool zero_write_ok = write(fd, "", 0) == 0;
        close(fd);
        _exit(zero_write_ok ? 0 : 4);
    }
    ASSERT_GT(child, 0);
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(ThreadsMax, NonRootEuidCannotOpenWithRootFsuid) {
    pid_t child = fork();
    if (child == 0) {
        if (setresuid(0, 65534, 0) != 0) _exit(2);
        setfsuid(0);
        if (setfsuid(-1) != 0) _exit(2);
        errno = 0;
        const int fd = open(kPath, O_WRONLY);
        const int saved_errno = errno;
        if (fd >= 0) close(fd);
        _exit(fd == -1 && saved_errno == EACCES ? 0 : 3);
    }
    ASSERT_GT(child, 0);
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(ThreadsMax, OpenDescriptorDoesNotBypassLaterCredentialChange) {
    pid_t child = fork();
    if (child == 0) {
        int fd = open(kPath, O_WRONLY);
        if (fd < 0 || setuid(65534) != 0) _exit(2);
        errno = 0;
        const ssize_t written = write(fd, "1\n", 2);
        const int saved_errno = errno;
        close(fd);
        _exit(written == -1 && saved_errno == EPERM ? 0 : 3);
    }
    ASSERT_GT(child, 0);
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
